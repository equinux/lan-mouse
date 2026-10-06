use env_logger::Env;
use input_capture::InputCaptureError;
use input_emulation::InputEmulationError;
use lan_mouse::{
    capture_test,
    config::{self, Command, Config, ConfigError},
    emulation_test,
    service::{Service, ServiceError},
};
use lan_mouse_cli::CliError;
#[cfg(feature = "gtk")]
use lan_mouse_gtk::GtkError;
use lan_mouse_ipc::{IpcError, IpcListenerCreationError};
use std::{
    future::Future,
    io,
    process::{self, Child},
};
use thiserror::Error;
use tokio::task::LocalSet;

#[derive(Debug, Error)]
enum LanMouseError {
    #[error(transparent)]
    Service(#[from] ServiceError),
    #[error(transparent)]
    IpcError(#[from] IpcError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Capture(#[from] InputCaptureError),
    #[error(transparent)]
    Emulation(#[from] InputEmulationError),
    #[cfg(feature = "gtk")]
    #[error(transparent)]
    Gtk(#[from] GtkError),
    #[error(transparent)]
    Cli(#[from] CliError),
}

fn main() {
    // init logging
    let env = Env::default().filter_or("LAN_MOUSE_LOG_LEVEL", "info");
    env_logger::init_from_env(env);

    #[cfg(target_os = "macos")]
    if std::env::var("LAN_MOUSE_DEV_APP").as_deref() == Ok("1") {
        initialize_dev_app();
        check_dev_permissions();
    }

    if let Err(e) = run() {
        log::error!("{e}");
        process::exit(1);
    }
}

#[cfg(target_os = "macos")]
fn initialize_dev_app() {
    use std::ffi::c_void;
    type Ref = *mut c_void;
    #[link(name = "AppKit", kind = "framework")]
    extern "C" {
        fn NSApplicationLoad() -> bool;
    }
    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const std::ffi::c_char) -> Ref;
        fn sel_registerName(name: *const std::ffi::c_char) -> Ref;
        fn objc_msgSend(receiver: Ref, selector: Ref) -> Ref;
    }
    // Register the headless development bundle as an AppKit application before
    // querying TCC. Production GTK builds already initialize AppKit themselves.
    unsafe {
        if NSApplicationLoad() {
            let class = objc_getClass(c"NSApplication".as_ptr());
            let app = objc_msgSend(class, sel_registerName(c"sharedApplication".as_ptr()));
            let send_void: unsafe extern "C" fn(Ref, Ref) =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn(Ref, Ref) -> Ref);
            send_void(app, sel_registerName(c"finishLaunching".as_ptr()));
        }
    }
}

#[cfg(target_os = "macos")]
fn check_dev_permissions() {
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> std::ffi::c_uchar;
    }
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGPreflightListenEventAccess() -> bool;
        fn CGPreflightPostEventAccess() -> bool;
        fn CGRequestListenEventAccess() -> bool;
        fn CGRequestPostEventAccess() -> bool;
    }
    unsafe {
        let accessibility = AXIsProcessTrusted() != 0;
        let listen = CGPreflightListenEventAccess();
        let post = CGPreflightPostEventAccess();
        log::info!(
            "Development permissions: accessibility={accessibility}, listen={listen}, post={post}"
        );
        if std::env::var("LAN_MOUSE_DEV_PERMISSIONS").as_deref() == Ok("1") {
            // Explicit Accessibility prompting can be retriggered after a
            // denial, whereas an event-access request may not show another alert.
            // Request only one category per invocation to avoid stacked dialogs.
            if !accessibility {
                log::info!("Requesting development Accessibility permission once");
                prompt_dev_accessibility();
            } else if !post {
                log::info!("Requesting development event-control permission once");
                CGRequestPostEventAccess();
            } else if !listen {
                log::info!("Requesting development input-monitoring permission once");
                CGRequestListenEventAccess();
            }
        }
    }
}

#[cfg(target_os = "macos")]
unsafe fn prompt_dev_accessibility() {
    use std::ffi::{c_uchar, c_void};
    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        static kAXTrustedCheckOptionPrompt: *const c_void;
        fn AXIsProcessTrustedWithOptions(options: *const c_void) -> c_uchar;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        static kCFBooleanTrue: *const c_void;
        fn CFDictionaryCreate(
            allocator: *const c_void,
            keys: *const *const c_void,
            values: *const *const c_void,
            count: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> *const c_void;
        fn CFRelease(object: *const c_void);
    }
    // The key and value are immortal CF constants, so no retain callbacks are needed.
    let options = CFDictionaryCreate(
        std::ptr::null(),
        &kAXTrustedCheckOptionPrompt,
        &kCFBooleanTrue,
        1,
        std::ptr::null(),
        std::ptr::null(),
    );
    if !options.is_null() {
        AXIsProcessTrustedWithOptions(options);
        CFRelease(options);
    }
}

fn run() -> Result<(), LanMouseError> {
    let config = config::Config::new()?;
    match config.command() {
        Some(command) => match command {
            Command::TestEmulation(args) => run_async(emulation_test::run(config, args))?,
            Command::TestCapture(args) => run_async(capture_test::run(config, args))?,
            Command::Cli(cli_args) => run_async(lan_mouse_cli::run(cli_args))?,
            Command::Daemon => {
                // if daemon is specified we run the service
                match run_async(run_service(config)) {
                    Err(LanMouseError::Service(ServiceError::IpcListen(
                        IpcListenerCreationError::AlreadyRunning,
                    ))) => log::info!("service already running!"),
                    r => r?,
                }
            }
        },
        None => {
            //  otherwise start the service as a child process and
            //  run a frontend
            #[cfg(feature = "gtk")]
            {
                // Only spawn a new daemon if one isn't already running
                let mut service = if lan_mouse_ipc::is_service_running() {
                    log::info!("daemon already running, connecting to existing instance");
                    None
                } else {
                    Some(start_service()?)
                };
                let res = lan_mouse_gtk::run(config::local_commit());
                if let Some(ref mut service) = service {
                    #[cfg(unix)]
                    {
                        // on unix we give the service a chance to terminate gracefully
                        let pid = service.id() as libc::pid_t;
                        unsafe {
                            libc::kill(pid, libc::SIGINT);
                        }
                        service.wait()?;
                    }
                    service.kill()?;
                }
                res?;
            }
            #[cfg(not(feature = "gtk"))]
            {
                // run daemon if gtk is diabled
                match run_async(run_service(config)) {
                    Err(LanMouseError::Service(ServiceError::IpcListen(
                        IpcListenerCreationError::AlreadyRunning,
                    ))) => log::info!("service already running!"),
                    r => r?,
                }
            }
        }
    }

    Ok(())
}

fn run_async<F, E>(f: F) -> Result<(), LanMouseError>
where
    F: Future<Output = Result<(), E>>,
    LanMouseError: From<E>,
{
    // create single threaded tokio runtime
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;

    // run async event loop
    Ok(runtime.block_on(LocalSet::new().run_until(f))?)
}

fn start_service() -> Result<Child, io::Error> {
    let mut command = process::Command::new(std::env::current_exe()?);
    command.args(std::env::args().skip(1)).arg("daemon");
    #[cfg(target_os = "macos")]
    if !input_event::macos_permissions::Permissions::input_allowed() {
        log::info!("Native input disabled for this launch; grant permissions and relaunch");
        command.env(input_event::macos_permissions::INPUT_DISABLED_ENV, "1");
    }
    let child = command.spawn()?;
    Ok(child)
}

async fn run_service(config: Config) -> Result<(), ServiceError> {
    let release_bind = config.release_bind();
    let config_path = config.config_path().to_owned();
    let mut service = Service::new(config).await?;
    log::info!("using config: {config_path:?}");
    log::info!("Press {release_bind:?} to release the mouse");
    service.run().await?;
    log::info!("service exited!");
    Ok(())
}
