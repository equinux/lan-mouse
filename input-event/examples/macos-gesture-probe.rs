//! Check native Quartz transport compatibility without injecting any input.
#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use input_event::{MacGesture, macos_gesture};
    use std::{ffi::c_void, path::Path};
    type Ref = *mut c_void;
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGEventCreate(source: Ref) -> Ref;
        fn CGEventSetType(event: Ref, kind: u32);
        fn CGEventSetIntegerValueField(event: Ref, field: u32, value: i64);
        fn CGEventSetDoubleValueField(event: Ref, field: u32, value: f64);
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(object: Ref);
    }
    let args: Vec<_> = std::env::args().collect();
    let dir = Path::new(
        args.get(2)
            .ok_or("Usage: macos-gesture-probe {write|read} DIR")?,
    );
    if args[1] == "write" {
        std::fs::create_dir_all(dir)?;
        for (name, subtype, expected) in [
            ("rotate", 5, 18),
            ("zoom", 8, 30),
            ("swipe", 16, 31),
            ("smart-zoom", 22, 32),
        ] {
            let gesture = unsafe {
                let event = CGEventCreate(std::ptr::null_mut());
                assert!(!event.is_null());
                CGEventSetType(event, 29);
                CGEventSetIntegerValueField(event, 110, subtype);
                CGEventSetIntegerValueField(event, 132, 2);
                CGEventSetDoubleValueField(event, 113, 0.25);
                CGEventSetDoubleValueField(event, 114, 15.0);
                CGEventSetIntegerValueField(event, 115, 1);
                let gesture = macos_gesture::capture(event, 1);
                CFRelease(event);
                gesture.ok_or(format!("{name} was not recognized"))?
            };
            assert_eq!(gesture.kind, expected);
            assert_eq!(macos_gesture::reconstructed_kind(&gesture), Some(expected));
            let mut data = vec![gesture.kind];
            data.extend_from_slice(&gesture.data);
            std::fs::write(dir.join(name), data)?;
            println!(
                "{name}: kind={}, bytes={}, local roundtrip OK",
                gesture.kind,
                gesture.data.len()
            );
        }
    } else {
        for file in std::fs::read_dir(dir)? {
            let file = file?;
            let data = std::fs::read(file.path())?;
            let gesture = MacGesture {
                sequence: 1,
                kind: data[0],
                data: data[1..].to_vec(),
            };
            assert_eq!(
                macos_gesture::reconstructed_kind(&gesture),
                Some(gesture.kind)
            );
            println!(
                "{}: kind={}, bytes={}, reconstruction OK",
                file.file_name().to_string_lossy(),
                gesture.kind,
                gesture.data.len()
            );
        }
    }
    Ok(())
}
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("This transport probe requires macOS.");
}
