//! Screen-capture permission probe: report-only, never requests.
//!
//! - macOS gates capture behind Screen Recording consent: the preflight API
//!   below reports the status *without* prompting (unlike its `Request`
//!   sibling, which we deliberately never call).
//! - Windows and Linux have no OS-level capture prompt, so there is nothing
//!   to deny: the probe reports granted.

/// Whether the OS currently lets us capture the screen.
///
/// Never prompts and never blocks: on macOS this is one preflight syscall;
/// everywhere else it is a constant `true` (no prompt exists to deny).
pub fn screen_capture_granted() -> bool {
    imp::screen_capture_granted()
}

#[cfg(target_os = "macos")]
mod imp {
    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
    }

    pub fn screen_capture_granted() -> bool {
        // SAFETY: the preflight call is a pure status query — no callbacks,
        // no allocation, no prompt. Present since macOS 10.15.
        unsafe { CGPreflightScreenCaptureAccess() }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub fn screen_capture_granted() -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_never_prompts_and_repeats() {
        // Must be side-effect free: two calls agree, no panic, no prompt.
        assert_eq!(screen_capture_granted(), screen_capture_granted());
    }
}
