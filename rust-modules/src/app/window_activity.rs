//! The window's foreground permission and the simulator's frame budget — application state, split
//! out of `system.rs` (step L15) so that file keeps only the calls that reach the platform.

/// Whether SDL has completed foreground entry. This gate is independent of idle damage:
/// queued uploads, animations, the video plane and noidle must never authorize a background
/// EGL swap. SDL/Mali owns additional Wayland proxies that clearing our borrowed handles cannot
/// protect. Owned by the app's main loop, and never inferred from the current UI route.
pub(crate) struct WindowActivity {
    active: bool,
}

impl WindowActivity {
    pub(crate) const fn new() -> Self { Self { active: true } }

    pub(crate) fn event(&mut self, event: u32) {
        match event {
            0x103 | 0x104 => self.active = false,
            0x106 => self.active = true,
            _ => {} // WILL foreground does not yet authorize rendering.
        }
    }

    pub(crate) fn allow_present(&self, requested: bool) -> bool {
        self.active && requested
    }
}

/// Whether a Linux host simulator's swap accepts interval 1 without waiting for anything:
/// WSLg's X11/GLX (it does not wait for the Windows compositor), and SDL's `offscreen` driver,
/// which has no display at all — uncapped, a headless run presents hundreds of frames a second,
/// and a recording taken there outgrows the replay decoder's budget (`ui::rec::CAP_BYTES`).
#[cfg(all(feature = "hostsim", target_os = "linux"))]
pub(crate) fn swap_ignores_vsync(wsl: bool, video_driver: Option<&str>) -> bool {
    match video_driver {
        Some("offscreen") => true,
        Some("x11") => wsl,
        _ => false,
    }
}

/// The 60 Hz software frame budget for a host whose swap ignores vsync. Keep that host-specific
/// wall-clock adapter here, outside the app's logical clock: recorded UI replays must continue
/// to see only their injected ticks.
#[cfg(all(feature = "hostsim", target_os = "linux"))]
pub(crate) struct SoftFrameBudget(std::time::Instant);

#[cfg(all(feature = "hostsim", target_os = "linux"))]
impl SoftFrameBudget {
    pub(crate) fn begin() -> Self { Self(std::time::Instant::now()) }

    pub(crate) fn finish(self) {
        if let Some(remaining) = std::time::Duration::from_nanos(16_666_667)
            .checked_sub(self.0.elapsed())
        {
            std::thread::sleep(remaining);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(feature = "hostsim", target_os = "linux"))]
    #[test]
    fn only_hosts_whose_swap_does_not_wait_get_the_software_budget() {
        assert!(swap_ignores_vsync(false, Some("offscreen")));
        assert!(swap_ignores_vsync(true, Some("offscreen")));
        assert!(swap_ignores_vsync(true, Some("x11")));
        assert!(!swap_ignores_vsync(false, Some("x11")));
        assert!(!swap_ignores_vsync(true, Some("wayland")));
        assert!(!swap_ignores_vsync(false, None));
    }

    #[test]
    fn background_blocks_every_present_request_until_did_foreground() {
        let mut window = WindowActivity::new();
        assert!(window.allow_present(true));
        assert!(!window.allow_present(false));
        for _ in 0..3 {
            for event in [0x103, 0x104, 0x105, 0x200] {
                window.event(event);
                // The request may include a bound plane, queued uploads, noidle or keepalive.
                assert!(!window.allow_present(true), "event {event:x} permits a background swap");
            }
            window.event(0x106);
            assert!(window.allow_present(true));
            assert!(!window.allow_present(false));
        }
        // Some platforms send only DID background. It must be sufficient on its own.
        window.event(0x104);
        assert!(!window.allow_present(true));
    }
}
