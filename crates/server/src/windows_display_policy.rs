//! Pure policy for Windows desktop, display-topology, and resize decisions.
//!
//! Keep Win32 observation and mutation in the agent, but make the decision
//! table platform-independent so lock/login/topology regressions are unit-testable.

use phantom_core::display_modes::{closest_advertised_mode, DisplayMode, NATIVE_DEFAULT_MAX_MODE};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub const TIER1_BASELINE_RESOLUTION: (u32, u32) = (1920, 1080);
pub const TIER1_ADAPTIVE_MAX_MODE: DisplayMode = NATIVE_DEFAULT_MAX_MODE;
const TIER1_STARTUP_TIMEOUT: Duration = Duration::from_millis(2500);
const TIER1_STARTUP_NUDGE_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Default)]
pub struct Tier1StartupRecovery {
    waiting_for_frame_since: Option<Instant>,
    last_nudge_at: Option<Instant>,
    startup_retries: u8,
    tier1_disabled: bool,
    black_startup_logged: bool,
}

impl Tier1StartupRecovery {
    pub fn reset(&mut self) {
        self.waiting_for_frame_since = None;
        self.last_nudge_at = None;
        self.startup_retries = 0;
        self.tier1_disabled = false;
        self.black_startup_logged = false;
    }

    pub fn clear_startup_wait(&mut self) {
        self.waiting_for_frame_since = None;
        self.last_nudge_at = None;
        self.black_startup_logged = false;
    }

    pub fn wait_for_frame(&mut self, now: Instant) {
        // Repeated viewer retries must not extend the recovery deadline.
        if self.waiting_for_frame_since.is_none() {
            self.waiting_for_frame_since = Some(now);
            self.last_nudge_at = None;
            self.black_startup_logged = false;
        }
    }

    pub fn waiting_for_frame_since(&self) -> Option<Instant> {
        self.waiting_for_frame_since
    }

    pub fn should_nudge_startup(&mut self, now: Instant) -> bool {
        if self.waiting_for_frame_since.is_none()
            || self.last_nudge_at.is_some_and(|last| {
                now.saturating_duration_since(last) < TIER1_STARTUP_NUDGE_INTERVAL
            })
        {
            return false;
        }
        self.last_nudge_at = Some(now);
        true
    }

    pub fn mark_frame_ready(&mut self) {
        self.reset();
    }

    pub fn can_try_tier1(&self) -> bool {
        !self.tier1_disabled
    }

    pub fn disable_tier1(&mut self) {
        self.clear_startup_wait();
        self.tier1_disabled = true;
    }

    pub fn should_log_black_startup(&mut self) -> bool {
        if self.black_startup_logged {
            false
        } else {
            self.black_startup_logged = true;
            true
        }
    }

    pub fn startup_wait_timed_out(&self, now: Instant) -> bool {
        self.waiting_for_frame_since
            .is_some_and(|since| now.saturating_duration_since(since) > TIER1_STARTUP_TIMEOUT)
    }

    pub fn consume_startup_retry(&mut self) -> bool {
        if self.startup_retries < 1 {
            self.startup_retries += 1;
            self.waiting_for_frame_since = None;
            self.last_nudge_at = None;
            true
        } else {
            false
        }
    }
}

pub fn tier1_adaptive_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        parse_enabled_flag(
            std::env::var("PHANTOM_WINDOWS_TIER1_ADAPTIVE")
                .ok()
                .as_deref(),
        )
    })
}

fn parse_enabled_flag(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsDesktopPhase {
    Default,
    Winlogon,
    Transition,
}

/// Desktop assigned to a concrete Windows agent generation.
///
/// The previous generation remains connected during handoff so the viewer can
/// retain its last frame, but it must not follow the input desktop and capture
/// a topology that the replacement generation is still provisioning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsAgentDesktop {
    Default,
    Winlogon,
}

impl WindowsAgentDesktop {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "default" => Some(Self::Default),
            "winlogon" => Some(Self::Winlogon),
            _ => None,
        }
    }

    pub fn matches_input_desktop(self, desktop_name: Option<&str>) -> bool {
        desktop_name.is_some_and(|name| match self {
            Self::Default => name.eq_ignore_ascii_case("Default"),
            Self::Winlogon => name.eq_ignore_ascii_case("Winlogon"),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsDisplayPolicy {
    /// Windows is between input desktops or the display stack is not queryable.
    TransitionHold,
    /// Windows owns the secure desktop and Phantom captures the console path.
    SecureDesktopConsole,
    /// Phantom may target and resize its VDD on the logged-in desktop.
    DefaultManagedVdd,
    /// No Phantom-owned VDD exists, so the OS topology must remain untouched.
    DefaultUnmanaged,
}

impl WindowsDisplayPolicy {
    pub fn holds_capture(self) -> bool {
        matches!(self, Self::TransitionHold)
    }

    pub fn uses_console_capture(self) -> bool {
        matches!(self, Self::SecureDesktopConsole)
    }

    pub fn allows_managed_tier1_retry(self) -> bool {
        matches!(self, Self::DefaultManagedVdd)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsTopologyKind {
    SingleManagedVdd,
    VddPlusOtherDisplays,
    PhysicalOnly,
    NoDisplay,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsProvisioningMode {
    PreserveConsole,
    Auto,
    ManagedVdd,
}

impl WindowsProvisioningMode {
    /// Installing another indirect display can make an external manager keep
    /// reconfiguring the desktop, even while Phantom itself is stopped.
    pub fn installs_vdd(self, external_manager_installed: bool) -> bool {
        match self {
            Self::PreserveConsole => false,
            Self::Auto => !external_manager_installed,
            Self::ManagedVdd => true,
        }
    }

    pub fn marker_value(self) -> Option<&'static str> {
        match self {
            Self::PreserveConsole => None,
            Self::Auto => Some("auto"),
            Self::ManagedVdd => Some("managed-vdd"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsProvisioningDecision {
    PreserveExisting,
    ProvisionVdd,
    AdoptExternal,
    Defer,
}

/// CCD can report the requested source mode before DXGI publishes a matching
/// duplication surface. A generation is not capture-ready until both views of
/// the managed display agree.
pub fn capture_surface_matches_target(
    target_rect: Option<(i32, i32, u32, u32)>,
    actual_width: u32,
    actual_height: u32,
) -> bool {
    target_rect.is_none_or(|(_, _, width, height)| width == actual_width && height == actual_height)
}

pub fn external_capture_target_still_valid(
    selected_target: &str,
    selected_target_active: bool,
    preferred_target: Option<&str>,
) -> bool {
    selected_target_active
        && preferred_target.is_none_or(|preferred| preferred.eq_ignore_ascii_case(selected_target))
}

pub fn decide_display_provisioning(
    mode: WindowsProvisioningMode,
    topology: WindowsTopologyKind,
    primary_is_basic: bool,
    external_owner_preferred: bool,
    external_display_active: bool,
) -> WindowsProvisioningDecision {
    match mode {
        WindowsProvisioningMode::PreserveConsole => WindowsProvisioningDecision::PreserveExisting,
        WindowsProvisioningMode::ManagedVdd => WindowsProvisioningDecision::ProvisionVdd,
        WindowsProvisioningMode::Auto if external_owner_preferred => {
            if external_display_active {
                WindowsProvisioningDecision::AdoptExternal
            } else {
                WindowsProvisioningDecision::Defer
            }
        }
        WindowsProvisioningMode::Auto => match topology {
            WindowsTopologyKind::SingleManagedVdd
            | WindowsTopologyKind::VddPlusOtherDisplays
            | WindowsTopologyKind::NoDisplay => WindowsProvisioningDecision::ProvisionVdd,
            WindowsTopologyKind::PhysicalOnly if primary_is_basic => {
                WindowsProvisioningDecision::ProvisionVdd
            }
            WindowsTopologyKind::PhysicalOnly => WindowsProvisioningDecision::PreserveExisting,
            WindowsTopologyKind::Unknown => WindowsProvisioningDecision::Defer,
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsCapturePath {
    Uninitialized,
    ManagedTier1,
    Fallback,
}

impl WindowsCapturePath {
    pub fn from_runtime_name(name: &str) -> Self {
        match name {
            "none" => Self::Uninitialized,
            "dxgi_nvenc" => Self::ManagedTier1,
            _ => Self::Fallback,
        }
    }

    fn permits_managed_resize(self) -> bool {
        matches!(self, Self::Uninitialized | Self::ManagedTier1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowsLayoutDecision {
    Apply { width: u32, height: u32 },
    Deferred { reason: String, retry: bool },
    Denied { reason: String },
}

pub fn policy_for(phase: WindowsDesktopPhase, managed_vdd_present: bool) -> WindowsDisplayPolicy {
    match phase {
        WindowsDesktopPhase::Transition => WindowsDisplayPolicy::TransitionHold,
        WindowsDesktopPhase::Winlogon => WindowsDisplayPolicy::SecureDesktopConsole,
        WindowsDesktopPhase::Default if managed_vdd_present => {
            WindowsDisplayPolicy::DefaultManagedVdd
        }
        WindowsDesktopPhase::Default => WindowsDisplayPolicy::DefaultUnmanaged,
    }
}

pub fn classify_topology(
    managed_vdd_present: bool,
    managed_vdd_active: bool,
    active_path_count: Option<usize>,
) -> WindowsTopologyKind {
    let Some(active_paths) = active_path_count else {
        return WindowsTopologyKind::Unknown;
    };

    match (managed_vdd_present, managed_vdd_active, active_paths) {
        (true, true, 1) => WindowsTopologyKind::SingleManagedVdd,
        (true, true, _) => WindowsTopologyKind::VddPlusOtherDisplays,
        (true, false, 0) | (false, false, 0) => WindowsTopologyKind::NoDisplay,
        (true, false, _) | (false, false, _) => WindowsTopologyKind::PhysicalOnly,
        (false, true, _) => WindowsTopologyKind::Unknown,
    }
}

pub fn decide_layout_request(
    width: u32,
    height: u32,
    policy: WindowsDisplayPolicy,
    topology: WindowsTopologyKind,
    capture_path: WindowsCapturePath,
    adaptive_enabled: bool,
) -> WindowsLayoutDecision {
    if width == 0 || height == 0 {
        return WindowsLayoutDecision::Denied {
            reason: "zero-sized resolution request".to_string(),
        };
    }

    match policy {
        WindowsDisplayPolicy::TransitionHold => {
            return WindowsLayoutDecision::Deferred {
                reason: "desktop transition in progress".to_string(),
                retry: true,
            };
        }
        WindowsDisplayPolicy::SecureDesktopConsole => {
            return WindowsLayoutDecision::Deferred {
                reason: "Winlogon/login screen uses fixed primary capture".to_string(),
                retry: true,
            };
        }
        WindowsDisplayPolicy::DefaultUnmanaged => {
            return WindowsLayoutDecision::Denied {
                reason: "no managed VDD target".to_string(),
            };
        }
        WindowsDisplayPolicy::DefaultManagedVdd => {}
    }

    if !capture_path.permits_managed_resize() {
        return WindowsLayoutDecision::Deferred {
            reason:
                "fallback capture is active; only Tier 1 DXGI/NVENC can resize the managed display"
                    .to_string(),
            retry: true,
        };
    }

    if !adaptive_enabled {
        let (base_w, base_h) = TIER1_BASELINE_RESOLUTION;
        return WindowsLayoutDecision::Deferred {
            reason: format!("fixed managed VDD mode is enabled; keeping stable {base_w}x{base_h}"),
            retry: false,
        };
    }

    match topology {
        WindowsTopologyKind::SingleManagedVdd => {
            let mode = closest_advertised_mode(
                DisplayMode::new(width, height),
                TIER1_ADAPTIVE_MAX_MODE,
            );
            WindowsLayoutDecision::Apply {
                width: mode.width,
                height: mode.height,
            }
        }
        WindowsTopologyKind::VddPlusOtherDisplays => WindowsLayoutDecision::Denied {
            reason: "adaptive VDD resize requires a single managed VDD; VDD+physical would create a hidden desktop area".to_string(),
        },
        WindowsTopologyKind::PhysicalOnly => WindowsLayoutDecision::Denied {
            reason: "managed VDD is unavailable; physical-only resize is not owned by Phantom"
                .to_string(),
        },
        WindowsTopologyKind::NoDisplay => WindowsLayoutDecision::Deferred {
            reason: "no active display path".to_string(),
            retry: true,
        },
        WindowsTopologyKind::Unknown => WindowsLayoutDecision::Deferred {
            reason: "display topology is unavailable".to_string(),
            retry: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installer_keeps_external_display_ownership_across_service_restarts() {
        let mode = WindowsProvisioningMode::Auto;
        assert!(!mode.installs_vdd(true));
        for active in [false, true] {
            assert_ne!(
                decide_display_provisioning(
                    mode,
                    WindowsTopologyKind::NoDisplay,
                    true,
                    true,
                    active,
                ),
                WindowsProvisioningDecision::ProvisionVdd,
            );
        }
        assert!(mode.installs_vdd(false));
        for external_manager in [false, true] {
            assert!(!WindowsProvisioningMode::PreserveConsole.installs_vdd(external_manager));
            assert!(WindowsProvisioningMode::ManagedVdd.installs_vdd(external_manager));
        }
    }

    #[test]
    fn desktop_policy_never_grants_vdd_ownership_outside_default() {
        assert_eq!(
            policy_for(WindowsDesktopPhase::Transition, true),
            WindowsDisplayPolicy::TransitionHold
        );
        assert_eq!(
            policy_for(WindowsDesktopPhase::Winlogon, true),
            WindowsDisplayPolicy::SecureDesktopConsole
        );
        assert_eq!(
            policy_for(WindowsDesktopPhase::Default, false),
            WindowsDisplayPolicy::DefaultUnmanaged
        );
        assert_eq!(
            policy_for(WindowsDesktopPhase::Default, true),
            WindowsDisplayPolicy::DefaultManagedVdd
        );
    }

    #[test]
    fn adaptive_flag_parser_accepts_only_explicit_true_values() {
        for value in ["1", "true", "TRUE", "yes", "On", " true "] {
            assert!(parse_enabled_flag(Some(value)), "{value}");
        }
        for value in ["0", "false", "no", "enabled", ""] {
            assert!(!parse_enabled_flag(Some(value)), "{value}");
        }
        assert!(!parse_enabled_flag(None));
    }

    #[test]
    fn single_managed_vdd_requires_the_vdd_path_to_be_active() {
        assert_eq!(
            classify_topology(true, false, Some(1)),
            WindowsTopologyKind::PhysicalOnly
        );
        assert_eq!(
            classify_topology(true, true, Some(1)),
            WindowsTopologyKind::SingleManagedVdd
        );
        assert_eq!(
            classify_topology(true, true, Some(2)),
            WindowsTopologyKind::VddPlusOtherDisplays
        );
    }

    #[test]
    fn auto_provisioning_adopts_an_active_external_display() {
        assert_eq!(
            decide_display_provisioning(
                WindowsProvisioningMode::Auto,
                WindowsTopologyKind::PhysicalOnly,
                false,
                true,
                true,
            ),
            WindowsProvisioningDecision::AdoptExternal
        );
    }

    #[test]
    fn auto_provisioning_repairs_headless_basic_and_multi_display_states() {
        assert_eq!(
            decide_display_provisioning(
                WindowsProvisioningMode::Auto,
                WindowsTopologyKind::PhysicalOnly,
                true,
                false,
                false,
            ),
            WindowsProvisioningDecision::ProvisionVdd
        );
        assert_eq!(
            decide_display_provisioning(
                WindowsProvisioningMode::Auto,
                WindowsTopologyKind::VddPlusOtherDisplays,
                false,
                false,
                false,
            ),
            WindowsProvisioningDecision::ProvisionVdd
        );
        assert_eq!(
            decide_display_provisioning(
                WindowsProvisioningMode::Auto,
                WindowsTopologyKind::NoDisplay,
                false,
                false,
                false,
            ),
            WindowsProvisioningDecision::ProvisionVdd
        );
    }

    #[test]
    fn explicit_provisioning_modes_are_not_heuristic() {
        assert_eq!(
            decide_display_provisioning(
                WindowsProvisioningMode::PreserveConsole,
                WindowsTopologyKind::NoDisplay,
                true,
                false,
                false,
            ),
            WindowsProvisioningDecision::PreserveExisting
        );
        assert_eq!(
            decide_display_provisioning(
                WindowsProvisioningMode::ManagedVdd,
                WindowsTopologyKind::PhysicalOnly,
                false,
                true,
                true,
            ),
            WindowsProvisioningDecision::ProvisionVdd
        );
    }

    #[test]
    fn auto_provisioning_waits_for_an_external_display_manager_to_settle() {
        for topology in [
            WindowsTopologyKind::NoDisplay,
            WindowsTopologyKind::VddPlusOtherDisplays,
            WindowsTopologyKind::SingleManagedVdd,
            WindowsTopologyKind::Unknown,
        ] {
            assert_eq!(
                decide_display_provisioning(
                    WindowsProvisioningMode::Auto,
                    topology,
                    false,
                    true,
                    false,
                ),
                WindowsProvisioningDecision::Defer
            );
        }
    }

    #[test]
    fn agent_desktop_assignment_is_strict() {
        assert_eq!(
            WindowsAgentDesktop::parse("default"),
            Some(WindowsAgentDesktop::Default)
        );
        assert_eq!(
            WindowsAgentDesktop::parse("WINLOGON"),
            Some(WindowsAgentDesktop::Winlogon)
        );
        assert_eq!(WindowsAgentDesktop::parse("transition"), None);
        assert!(WindowsAgentDesktop::Default.matches_input_desktop(Some("Default")));
        assert!(WindowsAgentDesktop::Winlogon.matches_input_desktop(Some("winlogon")));
        assert!(!WindowsAgentDesktop::Default.matches_input_desktop(Some("Winlogon")));
        assert!(!WindowsAgentDesktop::Winlogon.matches_input_desktop(None));
    }

    #[test]
    fn managed_capture_waits_for_dxgi_to_match_ccd() {
        let target = Some((0, 0, 1920, 1080));
        assert!(capture_surface_matches_target(target, 1920, 1080));
        assert!(!capture_surface_matches_target(target, 640, 480));
        assert!(capture_surface_matches_target(None, 640, 480));
    }

    #[test]
    fn external_capture_follows_a_late_managed_target_without_requiring_one_initially() {
        assert!(external_capture_target_still_valid(
            r"\\.\DISPLAY1",
            true,
            None
        ));
        assert!(external_capture_target_still_valid(
            r"\\.\DISPLAY1",
            true,
            Some(r"\\.\display1")
        ));
        assert!(!external_capture_target_still_valid(
            r"\\.\DISPLAY1",
            true,
            Some(r"\\.\DISPLAY2")
        ));
        assert!(!external_capture_target_still_valid(
            r"\\.\DISPLAY1",
            false,
            None
        ));
    }

    #[test]
    fn transition_and_winlogon_requests_remain_pending() {
        for policy in [
            WindowsDisplayPolicy::TransitionHold,
            WindowsDisplayPolicy::SecureDesktopConsole,
        ] {
            assert!(matches!(
                decide_layout_request(
                    1600,
                    900,
                    policy,
                    WindowsTopologyKind::SingleManagedVdd,
                    WindowsCapturePath::ManagedTier1,
                    true,
                ),
                WindowsLayoutDecision::Deferred { retry: true, .. }
            ));
        }
    }

    #[test]
    fn fixed_mode_consumes_request_without_changing_topology() {
        assert!(matches!(
            decide_layout_request(
                1600,
                900,
                WindowsDisplayPolicy::DefaultManagedVdd,
                WindowsTopologyKind::SingleManagedVdd,
                WindowsCapturePath::ManagedTier1,
                false,
            ),
            WindowsLayoutDecision::Deferred { retry: false, .. }
        ));
    }

    #[test]
    fn fallback_capture_defers_adaptive_resize() {
        assert!(matches!(
            decide_layout_request(
                1600,
                900,
                WindowsDisplayPolicy::DefaultManagedVdd,
                WindowsTopologyKind::SingleManagedVdd,
                WindowsCapturePath::Fallback,
                true,
            ),
            WindowsLayoutDecision::Deferred { retry: true, .. }
        ));
    }

    #[test]
    fn adaptive_resize_requires_a_single_active_vdd() {
        assert!(matches!(
            decide_layout_request(
                1600,
                900,
                WindowsDisplayPolicy::DefaultManagedVdd,
                WindowsTopologyKind::SingleManagedVdd,
                WindowsCapturePath::ManagedTier1,
                true,
            ),
            WindowsLayoutDecision::Apply {
                width: 1600,
                height: 900
            }
        ));
        assert!(matches!(
            decide_layout_request(
                1600,
                900,
                WindowsDisplayPolicy::DefaultManagedVdd,
                WindowsTopologyKind::VddPlusOtherDisplays,
                WindowsCapturePath::ManagedTier1,
                true,
            ),
            WindowsLayoutDecision::Denied { .. }
        ));
    }

    #[test]
    fn invalid_or_unmanaged_requests_are_denied() {
        assert!(matches!(
            decide_layout_request(
                0,
                900,
                WindowsDisplayPolicy::DefaultManagedVdd,
                WindowsTopologyKind::SingleManagedVdd,
                WindowsCapturePath::ManagedTier1,
                true,
            ),
            WindowsLayoutDecision::Denied { .. }
        ));
        assert!(matches!(
            decide_layout_request(
                1600,
                900,
                WindowsDisplayPolicy::DefaultUnmanaged,
                WindowsTopologyKind::PhysicalOnly,
                WindowsCapturePath::Uninitialized,
                true,
            ),
            WindowsLayoutDecision::Denied { .. }
        ));
    }

    #[test]
    fn tier1_retries_do_not_extend_the_startup_deadline() {
        let start = Instant::now();
        let mut recovery = Tier1StartupRecovery::default();

        recovery.wait_for_frame(start);
        recovery.wait_for_frame(start + Duration::from_secs(2));

        assert_eq!(recovery.waiting_for_frame_since(), Some(start));
        assert!(!recovery.startup_wait_timed_out(start + Duration::from_millis(2500)));
        assert!(recovery.startup_wait_timed_out(start + Duration::from_millis(2501)));
    }

    #[test]
    fn tier1_startup_nudges_are_rate_limited() {
        let start = Instant::now();
        let mut recovery = Tier1StartupRecovery::default();

        assert!(!recovery.should_nudge_startup(start));
        recovery.wait_for_frame(start);
        assert!(recovery.should_nudge_startup(start));
        assert!(!recovery.should_nudge_startup(start + Duration::from_millis(249)));
        assert!(recovery.should_nudge_startup(start + Duration::from_millis(250)));
    }

    #[test]
    fn tier1_startup_allows_one_full_reinitialization() {
        let start = Instant::now();
        let mut recovery = Tier1StartupRecovery::default();

        recovery.wait_for_frame(start);
        assert!(recovery.consume_startup_retry());
        recovery.wait_for_frame(start + Duration::from_secs(3));
        assert!(!recovery.consume_startup_retry());

        recovery.disable_tier1();
        assert!(!recovery.can_try_tier1());
        recovery.mark_frame_ready();
        assert!(recovery.can_try_tier1());
    }
}
