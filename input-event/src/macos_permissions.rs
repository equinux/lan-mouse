//! Silent preflight checks shared by the launcher and native input backends.
use std::sync::atomic::{AtomicBool, Ordering};

/// Set only by a launcher that observed incomplete permissions. A child must
/// not override the parent's denial using inherited or stale TCC decisions.
pub const INPUT_DISABLED_ENV: &str = "LAN_MOUSE_MACOS_INPUT_DISABLED";

#[derive(Clone, Copy, Debug)]
pub struct Permissions {
    pub accessibility: bool,
    pub listen: bool,
    pub post: bool,
}

impl Permissions {
    pub fn read() -> Self {
        #[link(name = "ApplicationServices", kind = "framework")]
        extern "C" {
            // Apple's Boolean is an unsigned byte, not Rust's bool.
            fn AXIsProcessTrusted() -> std::ffi::c_uchar;
        }
        #[link(name = "CoreGraphics", kind = "framework")]
        extern "C" {
            fn CGPreflightListenEventAccess() -> bool;
            fn CGPreflightPostEventAccess() -> bool;
        }
        unsafe {
            Self {
                accessibility: AXIsProcessTrusted() != 0,
                listen: CGPreflightListenEventAccess(),
                post: CGPreflightPostEventAccess(),
            }
        }
    }

    pub fn ready(self) -> bool {
        self.accessibility && self.listen && self.post
    }

    pub fn input_allowed() -> bool {
        std::env::var_os(INPUT_DISABLED_ENV).is_none() && Self::read().ready()
    }
}

/// Once denied or revoked, native input stays disabled until a fresh backend
/// is created. A timeout or later permission grant cannot revive an old tap.
pub struct InputPermissionGate(AtomicBool);

impl Default for InputPermissionGate {
    fn default() -> Self {
        Self::new(Permissions::input_allowed())
    }
}

impl InputPermissionGate {
    pub fn new(allowed: bool) -> Self {
        Self(AtomicBool::new(allowed))
    }

    pub fn check(&self, allowed: bool) -> bool {
        if !allowed {
            self.disable();
        }
        self.0.load(Ordering::Acquire)
    }

    pub fn enabled(&self) -> bool {
        self.check(Permissions::input_allowed())
    }

    pub fn disable(&self) {
        self.0.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_permission_is_required_before_native_input() {
        for mask in 0..8 {
            let permissions = Permissions {
                accessibility: mask & 1 != 0,
                listen: mask & 2 != 0,
                post: mask & 4 != 0,
            };
            assert_eq!(permissions.ready(), mask == 7);
            let gate = InputPermissionGate::new(permissions.ready());
            let mut interceptions = 0;
            if gate.check(permissions.ready()) {
                interceptions += 1;
            }
            assert_eq!(interceptions, usize::from(mask == 7));
        }
    }

    #[test]
    fn denied_startup_cannot_be_enabled_by_a_later_grant() {
        let gate = InputPermissionGate::new(false);
        assert!(!gate.check(true));
    }

    #[test]
    fn revocation_prevents_timeout_recovery_and_later_capture() {
        let gate = InputPermissionGate::new(true);
        assert!(gate.check(true));
        assert!(!gate.check(false));
        assert!(!gate.check(true));
        assert!(InputPermissionGate::new(true).check(true));
    }
}
