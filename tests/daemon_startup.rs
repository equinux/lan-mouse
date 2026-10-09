#![cfg(unix)]

use lan_mouse_ipc::{FrontendEvent, FrontendRequest};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct Daemon {
    child: Child,
    directory: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

#[test]
fn missing_clipboard_authorization_does_not_abort_daemon() {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // macOS's per-user temporary directory exceeds the Unix socket path limit.
    let directory = PathBuf::from("/tmp").join(format!("lan-mouse-startup-{unique}"));
    fs::create_dir_all(directory.join("Library/Caches")).unwrap();
    let config = directory.join("config.toml");
    fs::write(
        &config,
        format!(
            "[clipboard]\nenabled = true\nsend = true\npeer_fingerprint = {:?}\n",
            ["01"; 32].join(":"),
        ),
    )
    .unwrap();
    let log = directory.join("stderr.log");
    let child = Command::new(env!("CARGO_BIN_EXE_lan-mouse"))
        // Keep the daemon's config, certificate and IPC socket isolated.
        .env("HOME", &directory)
        .env("XDG_CONFIG_HOME", &directory)
        .env("XDG_RUNTIME_DIR", &directory)
        .env("LAN_MOUSE_MACOS_INPUT_DISABLED", "1")
        .args(["--capture-backend", "dummy", "--emulation-backend", "dummy"])
        .args(["--port", "0", "--config"])
        .arg(&config)
        .arg("--cert-path")
        .arg(directory.join("certificate.pem"))
        .arg("daemon")
        .stdout(Stdio::null())
        .stderr(fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let mut daemon = Daemon { child, directory };
    #[cfg(target_os = "macos")]
    let socket = daemon
        .directory
        .join("Library/Caches/lan-mouse-socket.sock");
    #[cfg(not(target_os = "macos"))]
    let socket = daemon.directory.join("lan-mouse-socket.sock");
    let started = Instant::now();
    let mut stream = loop {
        if let Ok(stream) = UnixStream::connect(&socket) {
            break stream;
        }
        assert!(
            daemon.child.try_wait().unwrap().is_none(),
            "daemon exited during startup: {}",
            fs::read_to_string(&log).unwrap(),
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "daemon never started"
        );
        thread::sleep(Duration::from_millis(10));
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    serde_json::to_writer(&mut stream, &FrontendRequest::Sync).unwrap();
    stream.write_all(b"\n").unwrap();
    let mut reader = BufReader::new(stream);
    let mut received_state = false;
    loop {
        let mut line = String::new();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "daemon closed IPC"
        );
        match serde_json::from_str::<FrontendEvent>(&line).unwrap() {
            FrontendEvent::Enumerate(_) => received_state = true,
            FrontendEvent::Error(error) => {
                assert!(error.contains("Clipboard disabled"));
                assert!(error.contains("authorized_fingerprints"));
                break;
            }
            _ => {}
        }
    }
    assert!(received_state, "daemon must still serve the frontend");
    unsafe { libc::kill(daemon.child.id() as libc::pid_t, libc::SIGINT) };
    let started = Instant::now();
    loop {
        if let Some(status) = daemon.child.try_wait().unwrap() {
            assert!(status.success(), "daemon did not terminate cleanly");
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "daemon did not stop"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
