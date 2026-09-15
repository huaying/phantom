//! Shared display mode selection for managed virtual displays.
//!
//! Windows VDD/IDD devices can only switch to modes advertised by the driver.
//! Keep this list as the single source for client hints and installer-generated
//! VDD configuration so the client never asks for a mode the driver does not
//! expose.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
}

impl DisplayMode {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    const fn area(self) -> u64 {
        self.width as u64 * self.height as u64
    }

    fn aspect(self) -> f64 {
        self.width as f64 / self.height.max(1) as f64
    }
}

/// Smallest resolution we will request for a remote desktop.
///
/// Below 1024x768 Windows has been observed to reshuffle windows across the
/// physical/VDD topology on multi-display VMs.
pub const MIN_REMOTE_MODE: DisplayMode = DisplayMode::new(1024, 768);

/// Conservative browser default while the web H.264 codec string is capped at
/// Baseline Level 4.0. Larger modes are available in the VDD bank for future
/// native/adaptive work, but web should keep this cap until the codec path is
/// negotiated above 1080p.
pub const WEB_DEFAULT_MAX_MODE: DisplayMode = DisplayMode::new(1920, 1080);

/// Native keeps the same conservative default cap until the server can report
/// the managed display's advertised modes. Higher modes stay in `VDD_MODE_BANK`
/// for installer/diagnostic work, but blindly requesting them can wedge older
/// VDD installs that only expose 1080p.
pub const NATIVE_DEFAULT_MAX_MODE: DisplayMode = WEB_DEFAULT_MAX_MODE;

/// Expanded VDD mode bank inspired by DCV/Sunshine-style managed displays.
///
/// This is intentionally broader than the modes the clients request by default.
/// The driver can expose these modes now; later protocol work can safely choose
/// higher resolutions after codec/layout negotiation.
pub const VDD_MODE_BANK: &[DisplayMode] = &[
    DisplayMode::new(640, 480),
    DisplayMode::new(800, 600),
    DisplayMode::new(1024, 768),
    DisplayMode::new(1152, 864),
    DisplayMode::new(1280, 720),
    DisplayMode::new(1280, 800),
    DisplayMode::new(1280, 960),
    DisplayMode::new(1280, 1024),
    DisplayMode::new(1366, 768),
    DisplayMode::new(1440, 900),
    DisplayMode::new(1600, 900),
    DisplayMode::new(1600, 1200),
    DisplayMode::new(1680, 1050),
    DisplayMode::new(1920, 1080),
    DisplayMode::new(1920, 1200),
    DisplayMode::new(2048, 1152),
    DisplayMode::new(2048, 1536),
    DisplayMode::new(2560, 1080),
    DisplayMode::new(2560, 1440),
    DisplayMode::new(2560, 1600),
    DisplayMode::new(3440, 1440),
    DisplayMode::new(3840, 1600),
    DisplayMode::new(3840, 2160),
    DisplayMode::new(4096, 2160),
];

/// Choose the best advertised mode for a viewport.
///
/// `scale` lets small browser/native windows still request a useful desktop
/// resolution. The result is capped by `max_mode`, then chosen by aspect-ratio
/// closeness first and area second to avoid picking a 16:10/4:3 mode for a
/// clearly 16:9 viewport just because it appears later in the list.
pub fn closest_mode_for_viewport(
    viewport_width: u32,
    viewport_height: u32,
    max_mode: DisplayMode,
    scale: f64,
) -> DisplayMode {
    if viewport_width == 0 || viewport_height == 0 {
        return DisplayMode::new(0, 0);
    }

    let target = DisplayMode::new(
        ((viewport_width as f64 * scale) as u32).clamp(MIN_REMOTE_MODE.width, max_mode.width),
        ((viewport_height as f64 * scale) as u32).clamp(MIN_REMOTE_MODE.height, max_mode.height),
    );
    closest_advertised_mode(target, max_mode)
}

pub fn closest_advertised_mode(target: DisplayMode, max_mode: DisplayMode) -> DisplayMode {
    let target_area = target.area().max(1) as f64;
    let target_aspect = target.aspect();

    let mut best = MIN_REMOTE_MODE;
    let mut best_score = f64::INFINITY;

    for &mode in VDD_MODE_BANK {
        if mode.width < MIN_REMOTE_MODE.width
            || mode.height < MIN_REMOTE_MODE.height
            || mode.width > max_mode.width
            || mode.height > max_mode.height
            || mode.width > target.width
            || mode.height > target.height
        {
            continue;
        }

        let aspect_delta = (mode.aspect() - target_aspect).abs();
        let area_delta = 1.0 - (mode.area() as f64 / target_area).min(1.0);
        let score = aspect_delta * 10.0 + area_delta;
        if score < best_score || (score == best_score && mode.area() > best.area()) {
            best = mode;
            best_score = score;
        }
    }

    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_cap_keeps_1080p_limit() {
        let mode = closest_mode_for_viewport(3840, 2160, WEB_DEFAULT_MAX_MODE, 1.3);
        assert_eq!(mode, DisplayMode::new(1920, 1080));
    }

    #[test]
    fn chooser_preserves_widescreen_aspect() {
        let mode = closest_mode_for_viewport(1280, 720, DisplayMode::new(4096, 2160), 1.3);
        assert_eq!(mode, DisplayMode::new(1600, 900));
    }

    #[test]
    fn chooser_allows_expanded_vdd_modes() {
        let mode = closest_mode_for_viewport(3000, 1700, DisplayMode::new(4096, 2160), 1.3);
        assert_eq!(mode, DisplayMode::new(3840, 2160));
    }
}
