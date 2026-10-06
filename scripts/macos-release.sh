#!/bin/bash
# Build a self-contained, signed GTK app and a drag-to-Applications disk image.
set -euo pipefail

: "${SIGNING_IDENTITY:?Set SIGNING_IDENTITY to a Developer ID Application certificate}"
minimum_version="${MACOSX_DEPLOYMENT_TARGET:-26.0}"
release_dir="$PWD/target/macos-release"
bundle="$release_dir/Lan Mouse.app"
runtime_app="${MACOS_RUNTIME_APP:-}"
mkdir -p "$release_dir"

# A compatible, already bundled GTK runtime can be reused when Homebrew's
# current bottles require a newer macOS than the receiving machine. Only the
# libraries/data are reused; the Lan Mouse executable is always rebuilt.
if [ -n "$runtime_app" ]; then
    runtime_lib="$release_dir/runtime/lib"
    rm -rf "$release_dir/runtime"
    mkdir -p "$runtime_lib" "$release_dir/runtime/pkgconfig"
    ditto "$runtime_app/Contents/Frameworks" "$runtime_lib"
    for library in "$runtime_lib"/*.dylib; do
        if ! otool -l "$library" | grep -qF '@loader_path/.'; then
            install_name_tool -add_rpath '@loader_path/.' "$library"
        fi
        codesign --force --sign - "$library"
    done
    python3 - "$runtime_lib" "$release_dir/runtime/pkgconfig" "$(brew --prefix)/lib/pkgconfig" <<'PY'
import pathlib, re, sys
lib, dest, source = map(pathlib.Path, sys.argv[1:])
for file in lib.glob('*.dylib'):
    alias = file.name
    while re.search(r'\.\d+\.dylib$', alias):
        alias = re.sub(r'\.\d+\.dylib$', '.dylib', alias)
        if not (lib / alias).exists():
            (lib / alias).symlink_to(file.name)
for file in source.glob('*.pc'):
    data = file.read_text()
    names = re.findall(r'(?:^|\s)-l([^\s]+)', data)
    if any((lib / f'lib{name}.dylib').exists() for name in names):
        data = re.sub(r'^libdir=.*$', f'libdir={lib}', data, flags=re.M)
        data = re.sub(r'-L/[^\s]+', f'-L{lib}', data)
        (dest / file.name).write_text(data)
PY
    export PKG_CONFIG_PATH="$release_dir/runtime/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
    export DYLD_FALLBACK_LIBRARY_PATH="$runtime_lib${DYLD_FALLBACK_LIBRARY_PATH:+:$DYLD_FALLBACK_LIBRARY_PATH}"
fi

# Rust's LLVM stripping currently produces proc-macro dylibs that macOS 27
# rejects (rust-lang/rust#157750). Keep compiler plugins intact.
# Build libgit2 from its pinned sources: Homebrew's build brings in its own
# SSH/OpenSSL runtime even though this app only needs Git build metadata.
MACOSX_DEPLOYMENT_TARGET="$minimum_version" CARGO_PROFILE_RELEASE_STRIP=none LIBGIT2_NO_PKG_CONFIG=1 cargo build --locked --release \
    -p lan-mouse --no-default-features --features gtk
bash scripts/makeicns.sh
rm -rf "$bundle"
mkdir -p "$bundle/Contents/MacOS" "$bundle/Contents/Resources"
cp target/release/lan-mouse "$bundle/Contents/MacOS/lan-mouse"
cp target/icon.icns target/menubar-template.png "$bundle/Contents/Resources/"
cp LICENSE "$bundle/Contents/Resources/"

python3 - "$bundle" "$minimum_version" <<'PY'
import datetime, pathlib, plistlib, subprocess, sys, tomllib
bundle = pathlib.Path(sys.argv[1])
version = tomllib.loads(pathlib.Path('Cargo.toml').read_text())['package']['version']
info = {
    'CFBundleExecutable': 'lan-mouse', 'CFBundleIdentifier': 'de.feschber.LanMouse',
    'CFBundleName': 'Lan Mouse', 'CFBundleDisplayName': 'Lan Mouse',
    'CFBundlePackageType': 'APPL', 'CFBundleIconFile': 'icon.icns',
    'CFBundleShortVersionString': version,
    'CFBundleVersion': datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%d.%H%M%S'),
    'LSMinimumSystemVersion': sys.argv[2], 'LSUIElement': True,
    'NSHighResolutionCapable': True, 'NSAppSleepDisabled': True,
    'NSInputMonitoringUsageDescription': 'Share keyboard and trackpad input with authorized Macs.',
    'NSLocalNetworkUsageDescription': 'Connect to authorized LAN Mouse peers on your local network.',
    # Launch Services supplies this to the GUI; its daemon child inherits it.
    'LSEnvironment': {'LAN_MOUSE_SPACES_SWIPE': '1'},
}
with (bundle / 'Contents/Info.plist').open('wb') as f:
    plistlib.dump(info, f)
revision = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
modified = bool(subprocess.check_output(['git', 'status', '--porcelain'], text=True).strip())
(bundle / 'Contents/Resources/build-info.txt').write_text(
    f'Lan Mouse {version}\nRevision: {revision}\nWorking tree modified: {modified}\n'
    'Features: GTK, native macOS gestures, secure shared text clipboard\n'
    'Clipboard sharing remains opt-in and requires configured, authorized certificate pins.\n')
PY

if [ -n "$runtime_app" ]; then
    install_name_tool -add_rpath "$runtime_lib" "$bundle/Contents/MacOS/lan-mouse"
fi
bash scripts/copy-macos-dylib.sh "$bundle/Contents/MacOS/lan-mouse"
if [ -n "$runtime_app" ]; then
    install_name_tool -delete_rpath "$runtime_lib" "$bundle/Contents/MacOS/lan-mouse"
    rm -rf "$bundle/Contents/Resources/share"
    ditto "$runtime_app/Contents/Resources/share" "$bundle/Contents/Resources/share"
fi

# Refuse a misleading minimum OS or dependencies on the build machine.
python3 - "$bundle" "$minimum_version" <<'PY'
import pathlib, re, subprocess, sys
bundle = pathlib.Path(sys.argv[1])
def os_version(value):
    return tuple((list(map(int, value.split('.'))) + [0, 0, 0])[:3])
maximum = os_version(sys.argv[2])
for file in [bundle / 'Contents/MacOS/lan-mouse', *sorted((bundle / 'Contents/Frameworks').iterdir())]:
    commands = subprocess.check_output(['otool', '-l', str(file)], text=True)
    versions = re.findall(r'^\s*minos\s+(\S+)', commands, re.M)
    for version in versions:
        if os_version(version) > maximum:
            raise SystemExit(f'{file.name} requires macOS {version}, above requested {sys.argv[2]}')
    links = subprocess.check_output(['otool', '-L', str(file)], text=True).splitlines()[1:]
    for line in links:
        ref = line.strip().split(' (', 1)[0]
        if not ref.startswith(('@rpath/', '/usr/lib/', '/System/Library/')):
            raise SystemExit(f'Unbundled dependency in {file.name}: {ref}')
    for path in re.findall(r'^\s*path (.*?) \(offset \d+\)', commands, re.M):
        if not path.startswith(('@loader_path/', '@executable_path/')):
            raise SystemExit(f'External runtime search path in {file.name}: {path}')
PY

find "$bundle/Contents/Frameworks" -type f -exec codesign --force --options runtime \
    --timestamp --sign "$SIGNING_IDENTITY" {} +
codesign --force --options runtime --timestamp --sign "$SIGNING_IDENTITY" "$bundle"
codesign --verify --deep --strict --verbose=2 "$bundle"
"$bundle/Contents/MacOS/lan-mouse" --version

image_dir="$release_dir/image"
rm -rf "$image_dir"
mkdir -p "$image_dir"
ditto "$bundle" "$image_dir/Lan Mouse.app"
ln -s /Applications "$image_dir/Applications"
hdiutil create -ov -format UDZO -volname 'Lan Mouse' -srcfolder "$image_dir" \
    "$release_dir/Lan-Mouse-macOS-arm64.dmg"
codesign --force --timestamp --sign "$SIGNING_IDENTITY" "$release_dir/Lan-Mouse-macOS-arm64.dmg"
ditto -c -k --sequesterRsrc --keepParent "$bundle" "$release_dir/Lan-Mouse-macOS-arm64.zip"
shasum -a 256 "$release_dir/Lan-Mouse-macOS-arm64.dmg" "$release_dir/Lan-Mouse-macOS-arm64.zip" \
    > "$release_dir/SHA256SUMS.txt"
echo "Installable artifacts: $release_dir"
