//! Modern CCD (Connecting and Configuring Displays) API wrappers.
//!
//! Reads active topology through `QueryDisplayConfig` and applies resolution
//! changes to an already-active, Phantom-managed VDD through
//! `SetDisplayConfig` (`SDC_VIRTUAL_MODE_AWARE` is required for IDD drivers).
//!
//! # Safety design — topology ownership is explicit
//!
//! Earlier versions cleared `PATH_ACTIVE` on non-VDD paths so VDD was the
//! ONLY active display. This worked, but when uninstall or service relaunch
//! happened while VDD-only topology was current, Windows could persist a
//! topology that no longer had a valid VDD driver on next boot. Two Win10/Win11
//! VMs got bricked this way.
//!
//! Explicit Phantom-managed provisioning may select the VDD as the sole active
//! path, with rollback if any post-condition fails. Preserve-console and
//! externally managed modes never activate, detach, or rearrange paths.
//! Resolution changes are allowed only after proving the capture target is the
//! sole active Phantom-owned VDD.
//!
//! Runtime-only (no `SDC_SAVE_TO_DATABASE`) — reboot reverts to defaults.
#![cfg(target_os = "windows")]

use anyhow::{bail, Context, Result};
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::ERROR_SUCCESS;

/// Flag inside `DISPLAYCONFIG_PATH_INFO.flags`. Not exposed by windows-rs 0.58.
const DISPLAYCONFIG_PATH_ACTIVE: u32 = 0x0000_0001;
const DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE: u32 = 0x0000_0008;
const DISPLAYCONFIG_PATH_MODE_IDX_INVALID: u32 = 0xffff_ffff;

#[derive(Clone)]
pub struct Topology {
    pub paths: Vec<DISPLAYCONFIG_PATH_INFO>,
    pub modes: Vec<DISPLAYCONFIG_MODE_INFO>,
}

/// Query all active display paths + modes (virtual-mode-aware for IDD support).
pub fn query_active_config() -> Result<Topology> {
    query_config(QDC_ONLY_ACTIVE_PATHS | QDC_VIRTUAL_MODE_AWARE)
}

fn query_all_config() -> Result<Topology> {
    query_config(QDC_ALL_PATHS | QDC_VIRTUAL_MODE_AWARE)
}

fn query_config(flags: QUERY_DISPLAY_CONFIG_FLAGS) -> Result<Topology> {
    unsafe {
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

/// Extract the source-mode index from a path's bitfield. In virtual-mode-aware
/// mode the `sourceInfo.Anonymous` field is `{cloneGroupId:16, sourceModeInfoIdx:16}`.
fn source_mode_idx(path: &DISPLAYCONFIG_PATH_INFO) -> usize {
    unsafe {
        let raw = path.sourceInfo.Anonymous.modeInfoIdx;
        if (path.flags & DISPLAYCONFIG_PATH_SUPPORT_VIRTUAL_MODE) != 0 {
            ((raw >> 16) & 0xFFFF) as usize
        } else {
            raw as usize
        }
    }
}

/// Fast-path: is VDD already the primary (at (0,0))? Skips the ~100-200ms
/// SetDisplayConfig call when no change is needed.
pub fn is_vdd_primary(vdd_gdi_name: &str) -> bool {
    let topo = match query_active_config() {
        Ok(t) => t,
        Err(_) => return false,
    };
    let vdd_idx = match find_vdd_path_idx(&topo, vdd_gdi_name) {
        Some(i) => i,
        None => return false,
    };
    let src_idx = source_mode_idx(&topo.paths[vdd_idx]);
    if src_idx >= topo.modes.len()
        || topo.modes[src_idx].infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
    {
        return false;
    }
    unsafe {
        let p = topo.modes[src_idx].Anonymous.sourceMode.position;
        p.x == 0 && p.y == 0
    }
}

/// Return the active CCD source rectangle for a GDI display name.
///
/// Prefer this over `EnumDisplaySettingsW` during Winlogon/Default transitions:
/// CCD is the topology we just applied with `SetDisplayConfig`, while legacy
/// GDI settings can lag behind and report the display's previous position.
pub fn active_source_rect(gdi_name: &str) -> Option<(i32, i32, u32, u32)> {
    let topo = query_active_config().ok()?;
    source_rect_from_topology(&topo, gdi_name)
}

pub fn source_rect_from_topology(topo: &Topology, gdi_name: &str) -> Option<(i32, i32, u32, u32)> {
    let path_idx = find_vdd_path_idx(topo, gdi_name)?;
    let src_idx = source_mode_idx(&topo.paths[path_idx]);
    if src_idx >= topo.modes.len()
        || topo.modes[src_idx].infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
    {
        return None;
    }
    unsafe {
        let src = topo.modes[src_idx].Anonymous.sourceMode;
        Some((src.position.x, src.position.y, src.width, src.height))
    }
}

/// Human-readable active topology summary for Windows capture diagnostics.
pub fn active_config_summary() -> Result<Vec<String>> {
    let topo = query_active_config()?;
    Ok(active_config_summary_from_topology(&topo))
}

pub fn active_path_count() -> Result<usize> {
    let topo = query_active_config()?;
    Ok(topo
        .paths
        .iter()
        .filter(|path| (path.flags & DISPLAYCONFIG_PATH_ACTIVE) != 0)
        .count())
}

/// Change an active display source mode through CCD and return the observed
/// topology. This is more reliable for IDD/VDD than ChangeDisplaySettingsExW
/// during Winlogon -> Default transitions, where Windows can reconnect VDD at
/// its 640x480 default.
pub fn set_source_resolution(gdi_name: &str, width: u32, height: u32) -> Result<Topology> {
    let current = query_active_config()?;
    let path_idx = find_vdd_path_idx(&current, gdi_name)
        .with_context(|| format!("active display path not found for {gdi_name}"))?;
    let src_idx = source_mode_idx(&current.paths[path_idx]);
    if src_idx >= current.modes.len()
        || current.modes[src_idx].infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
    {
        bail!("source mode not found for {gdi_name} at idx {src_idx}");
    }

    let current_source = unsafe { current.modes[src_idx].Anonymous.sourceMode };
    if current_source.width == width && current_source.height == height {
        return Ok(current);
    }

    crate::service_win::svc_log(&format!(
        "CCD: applying relaxed source mode {width}x{height} to {gdi_name}"
    ));
    match apply_source_resolution(&current, path_idx, src_idx, width, height, true) {
        Ok(observed) if source_resolution_matches(&observed, gdi_name, width, height) => {
            log_source_resolution(&observed);
            return Ok(observed);
        }
        Ok(_) => crate::service_win::svc_log(
            "CCD relaxed source mode was accepted but the requested resolution did not stick; retrying strictly",
        ),
        Err(e) => crate::service_win::svc_log(&format!(
            "CCD relaxed source mode failed: {e:#}; retrying strictly"
        )),
    }

    let strict_current =
        query_active_config().context("query topology before strict mode retry")?;
    let strict_path_idx = find_vdd_path_idx(&strict_current, gdi_name).with_context(|| {
        format!("active display path disappeared before strict retry: {gdi_name}")
    })?;
    let strict_src_idx = source_mode_idx(&strict_current.paths[strict_path_idx]);
    if strict_src_idx >= strict_current.modes.len()
        || strict_current.modes[strict_src_idx].infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
    {
        bail!("source mode not found for strict retry: {gdi_name}");
    }

    crate::service_win::svc_log(&format!(
        "CCD: applying strict source mode {width}x{height} to {gdi_name}"
    ));
    let observed = apply_source_resolution(
        &strict_current,
        strict_path_idx,
        strict_src_idx,
        width,
        height,
        false,
    )
    .context("SetDisplayConfig (strict source resolution)")?;
    log_source_resolution(&observed);
    if source_resolution_matches(&observed, gdi_name, width, height) {
        Ok(observed)
    } else {
        bail!(
            "CCD source resolution did not stick for {gdi_name}: requested {width}x{height}, observed {:?}",
            source_rect_from_topology(&observed, gdi_name)
        )
    }
}

fn apply_source_resolution(
    current: &Topology,
    path_idx: usize,
    src_idx: usize,
    width: u32,
    height: u32,
    allow_changes: bool,
) -> Result<Topology> {
    let mut paths = current.paths.clone();
    let mut modes = current.modes.clone();

    unsafe {
        let source = &mut modes[src_idx].Anonymous.sourceMode;
        source.width = width;
        source.height = height;

        // Resolution changes invalidate the target timing and desktop-image
        // modes. Leaving either index populated makes Win11 reject an otherwise
        // valid IDD source-mode update with ERROR_INVALID_PARAMETER.
        paths[path_idx].targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    }

    apply_source_mode(&paths, &modes, allow_changes)?;
    query_active_config().context("QueryDisplayConfig after source resolution")
}

fn apply_source_mode(
    paths: &[DISPLAYCONFIG_PATH_INFO],
    modes: &[DISPLAYCONFIG_MODE_INFO],
    allow_changes: bool,
) -> Result<()> {
    unsafe {
        let mut flags = SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_VIRTUAL_MODE_AWARE;
        if allow_changes {
            flags |= SDC_ALLOW_CHANGES;
        }
        let result = SetDisplayConfig(Some(paths), Some(modes), flags);
        if result != ERROR_SUCCESS.0 as i32 {
            bail!("SetDisplayConfig source mode failed: {result}");
        }
        Ok(())
    }
}

fn source_resolution_matches(topology: &Topology, gdi_name: &str, width: u32, height: u32) -> bool {
    source_rect_from_topology(topology, gdi_name)
        .is_some_and(|(_, _, observed_w, observed_h)| observed_w == width && observed_h == height)
}

fn log_source_resolution(topology: &Topology) {
    for line in active_config_summary_from_topology(topology) {
        crate::service_win::svc_log(&format!("CCD after source resolution: {line}"));
    }
}

fn active_config_summary_from_topology(topo: &Topology) -> Vec<String> {
    let mut lines = Vec::with_capacity(topo.paths.len());
    for (idx, path) in topo.paths.iter().enumerate() {
        let active = (path.flags & DISPLAYCONFIG_PATH_ACTIVE) != 0;
        let flags = path.flags;
        let raw_source_mode = unsafe { path.sourceInfo.Anonymous.modeInfoIdx };
        let source_name = gdi_name_for_path(path).unwrap_or_else(|e| format!("<source-name: {e}>"));
        let src_idx = source_mode_idx(path);
        let source_mode = if src_idx < topo.modes.len()
            && topo.modes[src_idx].infoType == DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
        {
            unsafe {
                let src = topo.modes[src_idx].Anonymous.sourceMode;
                format!(
                    "{}x{} pos=({},{})",
                    src.width, src.height, src.position.x, src.position.y
                )
            }
        } else {
            format!("source-mode-missing idx={src_idx}")
        };
        lines.push(format!(
            "path[{idx}] active={active} flags=0x{flags:X} raw_source_mode=0x{raw_source_mode:X} source={source_name} {source_mode}"
        ));
    }
    lines
}

/// Repair the origin of an already-active sole VDD after resizing it.
/// Refuse multi-display topology so this helper can never relocate user windows.
pub fn repair_sole_vdd_origin(vdd_gdi_name: &str) -> Result<Topology> {
    let current = query_active_config()?;
    let active_count = current
        .paths
        .iter()
        .filter(|path| (path.flags & DISPLAYCONFIG_PATH_ACTIVE) != 0)
        .count();
    if active_count != 1 {
        bail!("refusing to move VDD origin with {active_count} active display paths");
    }
    let vdd_idx = find_vdd_path_idx(&current, vdd_gdi_name)
        .with_context(|| format!("active VDD path not found: {vdd_gdi_name}"))?;

    let src_idx = source_mode_idx(&current.paths[vdd_idx]);
    if src_idx >= current.modes.len()
        || current.modes[src_idx].infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE
    {
        bail!("VDD source mode not found at idx {src_idx}");
    }

    let paths = current.paths.clone();
    let mut modes = current.modes.clone();
    let source = unsafe { &mut modes[src_idx].Anonymous.sourceMode };
    let changed = source.position.x != 0 || source.position.y != 0;
    source.position.x = 0;
    source.position.y = 0;

    if !changed {
        crate::service_win::svc_log(&format!(
            "CCD: sole VDD {vdd_gdi_name} origin already stable"
        ));
        for line in active_config_summary_from_topology(&current) {
            crate::service_win::svc_log(&format!("CCD stable primary: {line}"));
        }
        return Ok(current);
    }

    crate::service_win::svc_log(&format!(
        "CCD: repairing sole VDD {vdd_gdi_name} origin to (0,0)"
    ));

    apply(&paths, &modes).context("SetDisplayConfig (repair sole VDD origin)")?;
    let observed =
        query_active_config().context("QueryDisplayConfig after repairing sole VDD origin")?;
    for line in active_config_summary_from_topology(&observed) {
        crate::service_win::svc_log(&format!("CCD after primary: {line}"));
    }
    Ok(observed)
}

/// Select one display path and let Windows choose valid source/target modes.
///
/// This is an explicit provisioning operation, not runtime recovery. Callers
/// must only use it for a dedicated managed desktop and should retain the
/// returned topology until all post-conditions (including resolution) pass.
pub fn provision_single_display(gdi_name: &str) -> Result<Topology> {
    let original = query_active_config().context("snapshot active display topology")?;
    let all = query_all_config().context("query all display paths")?;
    let mut candidates = Vec::new();

    for path in &all.paths {
        if gdi_name_for_path(path).is_ok_and(|name| name.eq_ignore_ascii_case(gdi_name)) {
            candidates.push(*path);
        }
    }

    let mut selected = candidates
        .iter()
        .find(|path| (path.flags & DISPLAYCONFIG_PATH_ACTIVE) != 0)
        .copied()
        .or_else(|| {
            candidates
                .iter()
                .find(|path| path.targetInfo.targetAvailable.as_bool())
                .copied()
        })
        .or_else(|| candidates.first().copied())
        .with_context(|| format!("display path absent from QDC_ALL_PATHS: {gdi_name}"))?;

    selected.flags |= DISPLAYCONFIG_PATH_ACTIVE;
    selected.sourceInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    selected.targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;

    crate::service_win::svc_log(&format!(
        "CCD provisioning: selecting {gdi_name} as the sole active display"
    ));
    apply_auto_modes(&[selected]).context("SetDisplayConfig (provision single display)")?;

    let observed = query_active_config().context("query provisioned display topology")?;
    let active_count = observed
        .paths
        .iter()
        .filter(|path| (path.flags & DISPLAYCONFIG_PATH_ACTIVE) != 0)
        .count();
    if active_count != 1 || find_vdd_path_idx(&observed, gdi_name).is_none() {
        let _ = restore_topology(&original);
        bail!(
            "single-display provisioning did not stick for {gdi_name}: active_paths={active_count}"
        );
    }

    for line in active_config_summary_from_topology(&observed) {
        crate::service_win::svc_log(&format!("CCD after provisioning: {line}"));
    }
    Ok(original)
}

/// Restore a topology captured by `query_active_config`.
pub fn restore_topology(topology: &Topology) -> Result<()> {
    apply(&topology.paths, &topology.modes).context("restore display topology")
}

fn apply_auto_modes(paths: &[DISPLAYCONFIG_PATH_INFO]) -> Result<()> {
    unsafe {
        let flags = SDC_APPLY
            | SDC_USE_SUPPLIED_DISPLAY_CONFIG
            | SDC_ALLOW_CHANGES
            | SDC_VIRTUAL_MODE_AWARE;
        let result = SetDisplayConfig(Some(paths), None, flags);
        if result == ERROR_SUCCESS.0 as i32 {
            return Ok(());
        }

        let legacy_result = SetDisplayConfig(
            Some(paths),
            None,
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES,
        );
        if legacy_result != ERROR_SUCCESS.0 as i32 {
            bail!(
                "SetDisplayConfig auto-mode selection failed: virtual={result} legacy={legacy_result}"
            );
        }
        Ok(())
    }
}

fn apply(paths: &[DISPLAYCONFIG_PATH_INFO], modes: &[DISPLAYCONFIG_MODE_INFO]) -> Result<()> {
    unsafe {
        // No SDC_SAVE_TO_DATABASE — stay runtime-only so reboot reverts.
        let flags = SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_VIRTUAL_MODE_AWARE;
        let r = SetDisplayConfig(Some(paths), Some(modes), flags);
        if r != ERROR_SUCCESS.0 as i32 {
            // Retry with SDC_ALLOW_CHANGES as a permissive fallback.
            let r2 = SetDisplayConfig(
                Some(paths),
                Some(modes),
                SDC_APPLY
                    | SDC_USE_SUPPLIED_DISPLAY_CONFIG
                    | SDC_ALLOW_CHANGES
                    | SDC_VIRTUAL_MODE_AWARE,
            );
            if r2 != ERROR_SUCCESS.0 as i32 {
                bail!("SetDisplayConfig failed: primary={r} fallback={r2}");
            }
        }
        Ok(())
    }
}
