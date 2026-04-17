//! Modern CCD (Connecting and Configuring Displays) API wrappers.
//!
//! Used to make the VDD the primary display and optionally detach all other
//! displays so windows are forced onto it. Required because the legacy
//! `ChangeDisplaySettingsExW(CDS_SET_PRIMARY)` API returns `DISP_CHANGE_FAILED`
//! on Windows 11 24H2+ with IDD-based virtual displays (VDD issue #471).
//!
//! Follows the same pattern as Sunshine's libdisplaydevice:
//! QueryDisplayConfig + mutate modes + SetDisplayConfig.
//!
//! Runtime-only (no `SDC_SAVE_TO_DATABASE`): topology reverts on reboot so
//! a VDD failure can never brick the machine.
#![cfg(target_os = "windows")]

use anyhow::{bail, Context, Result};
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::{ERROR_SUCCESS, LUID};

/// Flag inside `DISPLAYCONFIG_PATH_INFO.flags`. Not exposed by windows-rs 0.58.
const DISPLAYCONFIG_PATH_ACTIVE: u32 = 0x0000_0001;

#[derive(Clone)]
pub struct Topology {
    pub paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    pub modes: Vec<DISPLAYCONFIG_MODE_INFO>,
}

/// Query all active display paths + modes (virtual-mode-aware for IDD support).
pub fn query_active_config() -> Result<Topology> {
    unsafe {
        let flags = QDC_ONLY_ACTIVE_PATHS | QDC_VIRTUAL_MODE_AWARE;
        let mut path_count: u32 = 0;
        let mut mode_count: u32 = 0;
        let r = GetDisplayConfigBufferSizes(flags, &mut path_count, &mut mode_count);
        if r != ERROR_SUCCESS {
            bail!("GetDisplayConfigBufferSizes failed: {:?}", r);
        }
        let mut paths: Vec<DISPLAYCONFIG_PATH_INFO> =
            vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes: Vec<DISPLAYCONFIG_MODE_INFO> =
            vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        let r = QueryDisplayConfig(
            flags,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        );
        if r != ERROR_SUCCESS {
            bail!("QueryDisplayConfig failed: {:?}", r);
        }
        paths.truncate(path_count as usize);
        modes.truncate(mode_count as usize);
        Ok(Topology { paths, modes })
    }
}

/// Get the GDI device name (e.g. `\\.\DISPLAY6`) for a path's source.
fn gdi_name_for_path(path: &DISPLAYCONFIG_PATH_INFO) -> Result<String> {
    unsafe {
        let mut info = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        info.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        info.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
        info.header.adapterId = path.sourceInfo.adapterId;
        info.header.id = path.sourceInfo.id;
        let r = DisplayConfigGetDeviceInfo(&mut info.header as *mut _);
        if r != ERROR_SUCCESS.0 as i32 {
            bail!("DisplayConfigGetDeviceInfo failed: {r}");
        }
        let end = info
            .viewGdiDeviceName
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(info.viewGdiDeviceName.len());
        Ok(String::from_utf16_lossy(&info.viewGdiDeviceName[..end]))
    }
}

/// Locate the path whose GDI source name matches `vdd_gdi_name`.
pub fn find_vdd_path_idx(topo: &Topology, vdd_gdi_name: &str) -> Option<usize> {
    for (idx, path) in topo.paths.iter().enumerate() {
        if (path.flags & DISPLAYCONFIG_PATH_ACTIVE) == 0 {
            continue;
        }
        match gdi_name_for_path(path) {
            Ok(name) if name == vdd_gdi_name => return Some(idx),
            _ => continue,
        }
    }
    None
}

/// Check if VDD is already the only active display at (0,0). Used as a
/// fast-path skip — if nothing changed, don't pay the SetDisplayConfig cost
/// (~100-200ms on Win10 where NVIDIA can come back via legacy API calls).
pub fn is_vdd_already_exclusive(vdd_gdi_name: &str) -> bool {
    let topo = match query_active_config() {
        Ok(t) => t,
        Err(_) => return false,
    };
    let vdd_idx = match find_vdd_path_idx(&topo, vdd_gdi_name) {
        Some(i) => i,
        None => return false,
    };
    // Exactly one active path (VDD).
    let active_count = topo
        .paths
        .iter()
        .filter(|p| (p.flags & DISPLAYCONFIG_PATH_ACTIVE) != 0)
        .count();
    if active_count != 1 {
        return false;
    }
    // VDD's source mode at (0,0)?
    unsafe {
        let bf = topo.paths[vdd_idx].sourceInfo.Anonymous.modeInfoIdx;
        let src_idx = ((bf >> 16) & 0xFFFF) as usize;
        if src_idx >= topo.modes.len()
            || topo.modes[src_idx].infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
        {
            return false;
        }
        let p = topo.modes[src_idx].Anonymous.sourceMode.position;
        p.x == 0 && p.y == 0
    }
}

/// Make the VDD the ONLY active display (detaches all others at runtime).
///
/// Tries three strategies in order (each saved original, so we can restore):
///   1. Shift all source positions so VDD → (0,0). Others stay active but
///      placed relative to VDD. New windows should open on VDD as primary.
///   2. (future) Clear PATH_ACTIVE on non-VDD paths to make VDD exclusive.
///
/// Changes are runtime-only — reboot reverts to default topology.
pub fn set_vdd_exclusive(vdd_gdi_name: &str) -> Result<Topology> {
    let current = query_active_config()?;
    let vdd_idx = find_vdd_path_idx(&current, vdd_gdi_name)
        .with_context(|| format!("VDD path not found: {vdd_gdi_name}"))?;

    // Log current topology for debugging
    crate::service_win::svc_log(&format!(
        "CCD: current topology {} paths, {} modes, vdd_idx={vdd_idx}",
        current.paths.len(),
        current.modes.len()
    ));
    for (i, m) in current.modes.iter().enumerate() {
        unsafe {
            let ty = m.infoType.0;
            if ty == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE.0 {
                let src = m.Anonymous.sourceMode;
                crate::service_win::svc_log(&format!(
                    "CCD: mode[{i}] SOURCE {}x{} pos=({},{})",
                    src.width, src.height, src.position.x, src.position.y
                ));
            } else if ty == DISPLAYCONFIG_MODE_INFO_TYPE_TARGET.0 {
                crate::service_win::svc_log(&format!("CCD: mode[{i}] TARGET"));
            }
        }
    }

    // --- Strategy 1: position-only shift so VDD lands at (0,0) ---
    // Find VDD's current source mode position to compute the shift.
    //
    // In virtual-mode-aware mode the sourceInfo.Anonymous field is a bitfield
    // `{cloneGroupId: 16, sourceModeInfoIdx: 16}`. Reading `modeInfoIdx` as u32
    // gives us the whole 32 bits — need to extract the upper 16 bits to get
    // the actual mode index.
    let (shift_x, shift_y) = unsafe {
        let bf = current.paths[vdd_idx].sourceInfo.Anonymous.modeInfoIdx;
        let src_idx = ((bf >> 16) & 0xFFFF) as usize;
        crate::service_win::svc_log(&format!(
            "CCD: vdd bitfield=0x{bf:X} sourceModeInfoIdx={src_idx}"
        ));
        if src_idx < current.modes.len()
            && current.modes[src_idx].infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
        {
            let p = current.modes[src_idx].Anonymous.sourceMode.position;
            (p.x, p.y)
        } else {
            (0, 0)
        }
    };
    crate::service_win::svc_log(&format!("CCD: shift=(-{shift_x},-{shift_y})"));

    let mut paths = current.paths.clone();
    let mut modes = current.modes.clone();
    // Shift every source mode by (-shift_x, -shift_y) so VDD lands at (0,0).
    // (Harmless for paths we're about to deactivate; those modes just get
    //  ignored by Windows.)
    for m in modes.iter_mut() {
        unsafe {
            if m.infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
                m.Anonymous.sourceMode.position.x -= shift_x;
                m.Anonymous.sourceMode.position.y -= shift_y;
            }
        }
    }
    // Deactivate all non-VDD paths so VDD is the ONLY active display.
    // This prevents Windows from moving primary back to NVIDIA when the user
    // logs in (user display config is otherwise restored from registry).
    let mut detached = 0;
    for (i, path) in paths.iter_mut().enumerate() {
        if i != vdd_idx && (path.flags & DISPLAYCONFIG_PATH_ACTIVE) != 0 {
            path.flags &= !DISPLAYCONFIG_PATH_ACTIVE;
            detached += 1;
        }
    }
    crate::service_win::svc_log(&format!(
        "CCD: detaching {detached} non-VDD paths, shifting VDD to (0,0)"
    ));

    apply(&paths, &modes).context("SetDisplayConfig (exclusive)")?;
    Ok(current)
}

/// Apply a previously saved topology. Used to restore default when stopping.
pub fn restore(topo: &Topology) -> Result<()> {
    apply(&topo.paths, &topo.modes).context("SetDisplayConfig (restore)")
}

fn apply(
    paths: &[DISPLAYCONFIG_PATH_INFO],
    modes: &[DISPLAYCONFIG_MODE_INFO],
) -> Result<()> {
    unsafe {
        // Try a progression of flag combos. Starting strictest (matching
        // Sunshine) down to more permissive. No SDC_SAVE_TO_DATABASE — stay
        // runtime-only so reboot reverts.
        let attempts: [(SET_DISPLAY_CONFIG_FLAGS, &str); 4] = [
            (
                SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_VIRTUAL_MODE_AWARE,
                "supplied|virtual",
            ),
            (
                SDC_APPLY
                    | SDC_USE_SUPPLIED_DISPLAY_CONFIG
                    | SDC_ALLOW_CHANGES
                    | SDC_VIRTUAL_MODE_AWARE,
                "supplied|virtual|allow",
            ),
            (
                SDC_APPLY
                    | SDC_USE_SUPPLIED_DISPLAY_CONFIG
                    | SDC_ALLOW_CHANGES
                    | SDC_ALLOW_PATH_ORDER_CHANGES
                    | SDC_VIRTUAL_MODE_AWARE,
                "supplied|virtual|allow|path-order",
            ),
            (
                SDC_VALIDATE | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_VIRTUAL_MODE_AWARE,
                "VALIDATE-only",
            ),
        ];
        let mut last_err: i32 = 0;
        for (flags, label) in attempts {
            let r = SetDisplayConfig(Some(paths), Some(modes), flags);
            crate::service_win::svc_log(&format!(
                "SetDisplayConfig [{label}] = {r} (0=success, 87=bad-param, 5=access-denied, 31=gen-failure)"
            ));
            if r == ERROR_SUCCESS.0 as i32 {
                return Ok(());
            }
            last_err = r;
        }
        bail!("SetDisplayConfig: all flag combos failed, last err={last_err}")
    }
}

#[allow(dead_code)]
fn _unused_luid() -> LUID {
    LUID::default()
}
