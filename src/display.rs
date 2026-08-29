use crate::model::{MonitorConfig, MonitorIdentity, MonitorLayout};
use anyhow::{Context, Result, bail};

#[cfg(not(windows))]
pub fn capture_layout(name: impl Into<String>) -> Result<MonitorLayout> {
    let _ = name;
    unsupported()
}

#[cfg(not(windows))]
pub fn apply_layout(_layout: &MonitorLayout) -> Result<()> {
    unsupported()
}

#[cfg(not(windows))]
pub fn startup_topology_needs_recovery() -> Result<bool> {
    unsupported()
}

#[cfg(not(windows))]
pub fn ensure_layout_available(_layout: &MonitorLayout) -> Result<()> {
    unsupported()
}

#[cfg(not(windows))]
pub fn restore_connected_topology() -> Result<()> {
    unsupported()
}

#[cfg(not(windows))]
fn unsupported<T>() -> Result<T> {
    bail!("MonMan display control is only available on Windows")
}

#[cfg(windows)]
mod win {
    use super::*;
    use std::collections::{HashMap, HashSet};
    use std::mem::size_of;
    use windows::Win32::Devices::Display::*;
    use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};
    use windows::Win32::Graphics::Gdi::DISPLAYCONFIG_PATH_ACTIVE;

    const INVALID_MODE_INDEX: u32 = 0xFFFF_FFFF;
    type SourceKey = (i32, u32, u32);

    #[derive(Clone)]
    struct Snapshot {
        paths: Vec<DISPLAYCONFIG_PATH_INFO>,
        modes: Vec<DISPLAYCONFIG_MODE_INFO>,
    }

    #[derive(Debug, Clone)]
    struct TargetMeta {
        identity: MonitorIdentity,
        friendly_name: String,
    }

    pub(super) fn capture_layout_impl(name: impl Into<String>) -> Result<MonitorLayout> {
        let all = query(QDC_ALL_PATHS)?;
        let active = query(QDC_ONLY_ACTIVE_PATHS)?;

        let mut active_by_identity = HashMap::<String, &DISPLAYCONFIG_PATH_INFO>::new();
        for path in &active.paths {
            if let Ok(meta) = target_meta(path) {
                active_by_identity.insert(meta.identity.stable_key(), path);
            }
        }

        let mut seen = HashSet::<String>::new();
        let mut monitors = Vec::new();

        for path in &all.paths {
            if !path.targetInfo.targetAvailable.as_bool() {
                continue;
            }

            let meta = match target_meta(path) {
                Ok(meta) => meta,
                Err(_) => continue,
            };

            let identity_key = meta.identity.stable_key();
            if !seen.insert(identity_key.clone()) {
                continue;
            }

            let active_path = active_by_identity.get(&identity_key).copied();
            let enabled = active_path.is_some();
            let selected_path = active_path.unwrap_or(path);
            let rotation = valid_rotation(selected_path.targetInfo.rotation.0);

            let (x, y, width, height) = active_path
                .and_then(|p| source_mode_for_path(p, &active.modes))
                .map(|m| (m.position.x, m.position.y, m.width, m.height))
                .unwrap_or_else(|| {
                    preferred_size(path)
                        .map(|(w, h)| (0, 0, w, h))
                        .unwrap_or((0, 0, 1920, 1080))
                });

            let refresh = selected_path.targetInfo.refreshRate;
            let measured_hz = rational_hz(refresh);
            let refresh_hz = if enabled && measured_hz >= 1.0 {
                measured_hz
            } else {
                60.0
            };
            let (refresh_numerator, refresh_denominator) =
                if enabled && refresh.Numerator > 0 && refresh.Denominator > 0 {
                    (Some(refresh.Numerator), Some(refresh.Denominator))
                } else {
                    (None, None)
                };

            monitors.push(MonitorConfig {
                identity: meta.identity,
                friendly_name: meta.friendly_name,
                enabled,
                source_adapter_low: selected_path.sourceInfo.adapterId.LowPart,
                source_adapter_high: selected_path.sourceInfo.adapterId.HighPart,
                source_id: selected_path.sourceInfo.id,
                clone_group: None,
                rotation,
                scaling: enabled.then_some(selected_path.targetInfo.scaling.0),
                x,
                y,
                width,
                height,
                refresh_hz,
                refresh_numerator,
                refresh_denominator,
            });
        }

        // Only active targets sharing a source are clones.
        let mut source_counts = HashMap::<SourceKey, usize>::new();
        for monitor in monitors.iter().filter(|m| m.enabled) {
            *source_counts.entry(monitor.source_key()).or_default() += 1;
        }
        let mut source_groups = HashMap::<SourceKey, u32>::new();
        let mut next_group = 1u32;
        for monitor in monitors.iter_mut().filter(|m| m.enabled) {
            let key = monitor.source_key();
            if source_counts.get(&key).copied().unwrap_or(0) > 1 {
                let group = *source_groups.entry(key).or_insert_with(|| {
                    let group = next_group;
                    next_group = next_group.saturating_add(1);
                    group
                });
                monitor.clone_group = Some(group);
            }
        }

        monitors.sort_by_key(|m| (m.x, m.y, m.friendly_name.clone()));

        Ok(MonitorLayout {
            name: name.into(),
            monitors,
            playback_device: None,
            microphone_device: None,
            hotkey: None,
            controller_hotkey: None,
        })
    }

    pub(super) fn apply_layout_impl(layout: &MonitorLayout) -> Result<()> {
        validate_layout(layout)?;

        let all = query(QDC_ALL_PATHS)?;
        let selected = select_paths(layout, &all.paths)?;

        let topology_flags = SDC_VALIDATE | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES;
        set_display_config(
            &selected,
            None,
            topology_flags,
            "Windows rejected the requested monitor topology",
        )?;

        let apply_flags =
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES | SDC_SAVE_TO_DATABASE;
        set_display_config(
            &selected,
            None,
            apply_flags,
            "failed to apply monitor topology",
        )?;

        // Re-query modes after activating targets, then apply saved geometry.
        let mut active = query(QDC_ONLY_ACTIVE_PATHS)?;
        patch_active_modes(layout, &mut active)?;

        set_display_config(
            &active.paths,
            Some(&active.modes),
            SDC_VALIDATE | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES,
            "topology was enabled, but Windows rejected its edited position/mode data",
        )?;

        set_display_config(
            &active.paths,
            Some(&active.modes),
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES | SDC_SAVE_TO_DATABASE,
            "topology was enabled, but applying its edited position/mode data failed",
        )?;

        Ok(())
    }

    pub(super) fn startup_topology_needs_recovery_impl() -> Result<bool> {
        // Avoid recovery during the brief startup availability lag.
        for attempt in 0..4 {
            let active = query(QDC_ONLY_ACTIVE_PATHS)?;
            let availability: Vec<bool> = active
                .paths
                .iter()
                .map(|path| path.targetInfo.targetAvailable.as_bool())
                .collect();
            let needs_recovery = topology_state_needs_recovery(&availability);
            if !needs_recovery {
                return Ok(false);
            }
            if attempt < 3 {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
        }
        Ok(true)
    }

    fn topology_state_needs_recovery(active_target_availability: &[bool]) -> bool {
        active_target_availability
            .iter()
            .all(|available| !available)
    }

    pub(super) fn ensure_layout_available_impl(layout: &MonitorLayout) -> Result<()> {
        validate_layout(layout)?;
        let all = query(QDC_ALL_PATHS)?;
        select_paths(layout, &all.paths)?;
        Ok(())
    }

    pub(super) fn restore_connected_topology_impl() -> Result<()> {
        // Ask CCD for the last persisted configuration that matches the monitors
        // connected now. If Windows has no matching database entry, its topology
        // and best-mode logic choose a usable arrangement (extended first on a
        // desktop) instead of preserving an unavailable target.
        let flags = SDC_APPLY | SDC_USE_DATABASE_CURRENT | SDC_ALLOW_CHANGES;
        let code = unsafe { SetDisplayConfig(None, None, flags) };
        if code != 0 {
            bail!(
                "Windows could not restore a topology for the currently connected monitors (SetDisplayConfig: {code})"
            );
        }

        for attempt in 0..4 {
            let active = query(QDC_ONLY_ACTIVE_PATHS)?;
            if active
                .paths
                .iter()
                .any(|path| path.targetInfo.targetAvailable.as_bool())
            {
                return Ok(());
            }
            if attempt < 3 {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        }

        bail!("Windows reported success but still has no available active display")
    }

    fn validate_layout(layout: &MonitorLayout) -> Result<()> {
        let enabled: Vec<&MonitorConfig> = layout
            .monitors
            .iter()
            .filter(|monitor| monitor.enabled)
            .collect();
        if enabled.is_empty() {
            bail!("a layout must keep at least one monitor enabled");
        }

        let mut identities = HashSet::new();
        for monitor in &enabled {
            if !identities.insert(monitor.identity.stable_key()) {
                bail!(
                    "monitor '{}' appears more than once in this layout",
                    monitor.friendly_name
                );
            }
            if monitor.width == 0 || monitor.height == 0 {
                bail!(
                    "monitor '{}' must have a non-zero width and height",
                    monitor.friendly_name
                );
            }
            if !monitor.refresh_hz.is_finite() || monitor.refresh_hz < 1.0 {
                bail!(
                    "monitor '{}' has an invalid refresh rate",
                    monitor.friendly_name
                );
            }
            if let Some(rotation) = monitor.rotation
                && !(1..=4).contains(&rotation)
            {
                bail!(
                    "monitor '{}' has an invalid orientation value",
                    monitor.friendly_name
                );
            }
        }

        let mut geometry_by_group = HashMap::<u32, (i32, i32, u32, u32, String)>::new();
        for monitor in &enabled {
            let Some(group) = monitor.clone_group else {
                continue;
            };
            let geometry = (monitor.x, monitor.y, monitor.width, monitor.height);
            if let Some((x, y, width, height, first_name)) = geometry_by_group.get(&group) {
                if (*x, *y, *width, *height) != geometry {
                    bail!(
                        "cloned monitors '{}' and '{}' must share the same position and source resolution",
                        first_name,
                        monitor.friendly_name
                    );
                }
            } else {
                geometry_by_group.insert(
                    group,
                    (
                        monitor.x,
                        monitor.y,
                        monitor.width,
                        monitor.height,
                        monitor.friendly_name.clone(),
                    ),
                );
            }
        }

        let source_at_origin = enabled
            .iter()
            .filter(|monitor| monitor.x == 0 && monitor.y == 0)
            .map(|monitor| {
                monitor
                    .clone_group
                    .map(|group| format!("clone:{group}"))
                    .unwrap_or_else(|| format!("monitor:{}", monitor.identity.stable_key()))
            })
            .collect::<HashSet<_>>()
            .len();
        if source_at_origin == 0 {
            bail!("one enabled monitor must be primary at (0, 0); use 'Make primary'");
        }
        if source_at_origin > 1 {
            bail!(
                "more than one independent monitor is positioned at (0, 0); move the others or make one monitor primary"
            );
        }

        Ok(())
    }

    fn select_paths(
        layout: &MonitorLayout,
        all_paths: &[DISPLAYCONFIG_PATH_INFO],
    ) -> Result<Vec<DISPLAYCONFIG_PATH_INFO>> {
        #[derive(Debug, Clone, PartialEq, Eq)]
        enum SavedGroupKey {
            Clone(u32),
            Single(String),
        }

        let mut candidates = Vec::with_capacity(all_paths.len());
        for path in all_paths {
            if path.targetInfo.targetAvailable.as_bool()
                && let Ok(meta) = target_meta(path)
            {
                candidates.push((path, meta));
            }
        }

        // Inactive source IDs are routes, not implicit clone membership.
        let mut groups: Vec<(SavedGroupKey, SourceKey, Vec<&MonitorConfig>)> = Vec::new();
        for monitor in layout.monitors.iter().filter(|m| m.enabled) {
            let saved_source = monitor.source_key();
            let group_key = match monitor.clone_group {
                Some(group) => SavedGroupKey::Clone(group),
                None => SavedGroupKey::Single(monitor.identity.stable_key()),
            };

            if let Some((_, preferred_source, monitors)) =
                groups.iter_mut().find(|(key, _, _)| *key == group_key)
            {
                if matches!(group_key, SavedGroupKey::Clone(_)) && *preferred_source != saved_source
                {
                    bail!("clone group contains monitors captured from different display sources");
                }
                monitors.push(monitor);
            } else {
                groups.push((group_key, saved_source, vec![monitor]));
            }
        }

        let mut prepared_groups = Vec::new();

        for (_group_key, saved_source, monitors) in groups {
            let mut matching_per_monitor: Vec<Vec<&DISPLAYCONFIG_PATH_INFO>> = Vec::new();

            for monitor in &monitors {
                let matching: Vec<&DISPLAYCONFIG_PATH_INFO> = candidates
                    .iter()
                    .filter(|(_, meta)| monitor.identity.matches(&meta.identity))
                    .map(|(path, _)| *path)
                    .collect();

                if matching.is_empty() {
                    bail!(
                        "monitor '{}' is not currently connected",
                        monitor.friendly_name
                    );
                }
                matching_per_monitor.push(matching);
            }

            // A clone group's targets must share one reachable source.
            let mut common_sources: HashSet<SourceKey> = matching_per_monitor[0]
                .iter()
                .map(|path| source_key(path))
                .collect();
            for matching in &matching_per_monitor[1..] {
                let available: HashSet<SourceKey> =
                    matching.iter().map(|path| source_key(path)).collect();
                common_sources.retain(|key| available.contains(key));
            }

            if common_sources.is_empty() {
                if monitors.len() > 1 {
                    bail!(
                        "Windows has no common display source available for cloned monitors '{}'",
                        monitors
                            .iter()
                            .map(|m| m.friendly_name.as_str())
                            .collect::<Vec<_>>()
                            .join("' + '")
                    );
                }
                bail!(
                    "Windows has no unused display source available for '{}'",
                    monitors[0].friendly_name
                );
            }

            let mut candidate_sources: Vec<SourceKey> = common_sources.into_iter().collect();
            candidate_sources.sort_unstable();
            if let Some(saved_position) = candidate_sources
                .iter()
                .position(|source| *source == saved_source)
            {
                candidate_sources.swap(0, saved_position);
            }
            prepared_groups.push((monitors, matching_per_monitor, candidate_sources));
        }

        // Match groups globally so flexible groups do not consume a required source.
        let source_candidates: Vec<Vec<SourceKey>> = prepared_groups
            .iter()
            .map(|(_, _, sources)| sources.clone())
            .collect();
        let assigned_sources = assign_sources(&source_candidates).context(
            "Windows cannot assign distinct display sources to every enabled monitor/clone group",
        )?;

        let mut selected = Vec::new();
        for ((monitors, matching_per_monitor, _), chosen_source) in
            prepared_groups.into_iter().zip(assigned_sources)
        {
            for (monitor, matching) in monitors.iter().zip(matching_per_monitor.iter()) {
                let chosen = matching
                    .iter()
                    .copied()
                    .find(|path| source_key(path) == chosen_source)
                    .context("no display path matched the selected source")?;

                let mut path = *chosen;
                path.flags |= DISPLAYCONFIG_PATH_ACTIVE;
                if let Some(rotation) = monitor.rotation {
                    path.targetInfo.rotation = DISPLAYCONFIG_ROTATION(rotation);
                }
                if let Some(scaling) = monitor.scaling {
                    path.targetInfo.scaling = DISPLAYCONFIG_SCALING(scaling);
                }
                path.sourceInfo.Anonymous.modeInfoIdx = INVALID_MODE_INDEX;
                path.targetInfo.Anonymous.modeInfoIdx = INVALID_MODE_INDEX;
                selected.push(path);
            }
        }

        Ok(selected)
    }

    fn assign_sources(candidates: &[Vec<SourceKey>]) -> Option<Vec<SourceKey>> {
        let mut group_order: Vec<usize> = (0..candidates.len()).collect();
        group_order.sort_by_key(|index| (candidates[*index].len(), *index));

        let mut source_owner = HashMap::<SourceKey, usize>::new();
        for group in group_order {
            let mut visited = HashSet::new();
            if !assign_source(group, candidates, &mut visited, &mut source_owner) {
                return None;
            }
        }

        let mut assigned = vec![None; candidates.len()];
        for (source, group) in source_owner {
            assigned[group] = Some(source);
        }
        assigned.into_iter().collect()
    }

    fn assign_source(
        group: usize,
        candidates: &[Vec<SourceKey>],
        visited: &mut HashSet<SourceKey>,
        source_owner: &mut HashMap<SourceKey, usize>,
    ) -> bool {
        for &source in &candidates[group] {
            if !visited.insert(source) {
                continue;
            }

            let previous_owner = source_owner.get(&source).copied();
            if previous_owner
                .is_none_or(|owner| assign_source(owner, candidates, visited, source_owner))
            {
                source_owner.insert(source, group);
                return true;
            }
        }
        false
    }

    fn patch_active_modes(layout: &MonitorLayout, active: &mut Snapshot) -> Result<()> {
        for path_index in 0..active.paths.len() {
            let meta = target_meta(&active.paths[path_index])?;
            let Some(saved) = layout
                .monitors
                .iter()
                .find(|m| m.enabled && m.identity.matches(&meta.identity))
            else {
                continue;
            };

            patch_target_settings(&mut active.paths[path_index], saved);

            let source_idx =
                unsafe { active.paths[path_index].sourceInfo.Anonymous.modeInfoIdx } as usize;
            if source_idx >= active.modes.len() {
                continue;
            }
            if active.modes[source_idx].infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
                continue;
            }

            unsafe {
                let mut mode = active.modes[source_idx].Anonymous.sourceMode;
                mode.position.x = saved.x;
                mode.position.y = saved.y;
                if saved.width > 0 && saved.height > 0 {
                    mode.width = saved.width;
                    mode.height = saved.height;
                }
                active.modes[source_idx].Anonymous.sourceMode = mode;
            }
        }

        Ok(())
    }

    fn patch_target_settings(path: &mut DISPLAYCONFIG_PATH_INFO, saved: &MonitorConfig) {
        if let Some(rotation) = saved.rotation {
            path.targetInfo.rotation = DISPLAYCONFIG_ROTATION(rotation);
        }
        if let Some(scaling) = saved.scaling {
            path.targetInfo.scaling = DISPLAYCONFIG_SCALING(scaling);
        }
        if saved.refresh_hz >= 1.0 {
            set_path_refresh_rate(path, saved_refresh_rational(saved));
        }
    }

    fn set_path_refresh_rate(
        path: &mut DISPLAYCONFIG_PATH_INFO,
        refresh_rate: DISPLAYCONFIG_RATIONAL,
    ) {
        path.targetInfo.refreshRate = refresh_rate;

        // Drop the concrete timing so Windows honors the requested refresh rate.
        path.targetInfo.Anonymous.modeInfoIdx = INVALID_MODE_INDEX;
    }

    fn saved_refresh_rational(saved: &MonitorConfig) -> DISPLAYCONFIG_RATIONAL {
        if let (Some(numerator), Some(denominator)) =
            (saved.refresh_numerator, saved.refresh_denominator)
            && numerator > 0
            && denominator > 0
        {
            let exact_hz = numerator as f32 / denominator as f32;
            if (exact_hz - saved.refresh_hz).abs() < 0.01 {
                return DISPLAYCONFIG_RATIONAL {
                    Numerator: numerator,
                    Denominator: denominator,
                };
            }
        }

        // GUI values use millihertz precision; Windows may choose a nearby mode.
        let hz1000 = (saved.refresh_hz * 1000.0)
            .round()
            .clamp(1.0, u32::MAX as f32) as u32;
        DISPLAYCONFIG_RATIONAL {
            Numerator: hz1000,
            Denominator: 1000,
        }
    }

    fn source_mode_for_path<'a>(
        path: &DISPLAYCONFIG_PATH_INFO,
        modes: &'a [DISPLAYCONFIG_MODE_INFO],
    ) -> Option<&'a DISPLAYCONFIG_SOURCE_MODE> {
        let idx = unsafe { path.sourceInfo.Anonymous.modeInfoIdx } as usize;
        let mode = modes.get(idx)?;
        if mode.infoType != DISPLAYCONFIG_MODE_INFO_TYPE_SOURCE {
            return None;
        }
        Some(unsafe { &mode.Anonymous.sourceMode })
    }

    fn preferred_size(path: &DISPLAYCONFIG_PATH_INFO) -> Option<(u32, u32)> {
        let mut preferred = DISPLAYCONFIG_TARGET_PREFERRED_MODE::default();
        preferred.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_PREFERRED_MODE;
        preferred.header.size = size_of::<DISPLAYCONFIG_TARGET_PREFERRED_MODE>() as u32;
        preferred.header.adapterId = path.targetInfo.adapterId;
        preferred.header.id = path.targetInfo.id;

        let code = unsafe { DisplayConfigGetDeviceInfo(&mut preferred.header) };
        if code == 0 && preferred.width > 0 && preferred.height > 0 {
            Some((preferred.width, preferred.height))
        } else {
            None
        }
    }

    fn target_meta(path: &DISPLAYCONFIG_PATH_INFO) -> Result<TargetMeta> {
        let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
        target.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
        target.header.size = size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
        target.header.adapterId = path.targetInfo.adapterId;
        target.header.id = path.targetInfo.id;

        let code = unsafe { DisplayConfigGetDeviceInfo(&mut target.header) };
        if code != 0 {
            bail!("DisplayConfigGetDeviceInfo(GET_TARGET_NAME) failed: {code}");
        }

        let friendly = wide_z(&target.monitorFriendlyDeviceName);
        let device_path = wide_z(&target.monitorDevicePath);
        let friendly_name = if friendly.trim().is_empty() {
            format!("Monitor {}", path.targetInfo.id)
        } else {
            friendly
        };

        Ok(TargetMeta {
            identity: MonitorIdentity {
                device_path,
                adapter_low: path.targetInfo.adapterId.LowPart,
                adapter_high: path.targetInfo.adapterId.HighPart,
                target_id: path.targetInfo.id,
            },
            friendly_name,
        })
    }

    fn source_key(path: &DISPLAYCONFIG_PATH_INFO) -> SourceKey {
        (
            path.sourceInfo.adapterId.HighPart,
            path.sourceInfo.adapterId.LowPart,
            path.sourceInfo.id,
        )
    }

    fn rational_hz(r: DISPLAYCONFIG_RATIONAL) -> f32 {
        if r.Denominator == 0 {
            0.0
        } else {
            r.Numerator as f32 / r.Denominator as f32
        }
    }

    fn valid_rotation(rotation: i32) -> Option<i32> {
        matches!(rotation, 1..=4).then_some(rotation)
    }

    fn wide_z(buf: &[u16]) -> String {
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf16_lossy(&buf[..len])
    }

    fn set_display_config(
        paths: &[DISPLAYCONFIG_PATH_INFO],
        modes: Option<&[DISPLAYCONFIG_MODE_INFO]>,
        flags: SET_DISPLAY_CONFIG_FLAGS,
        action: &str,
    ) -> Result<()> {
        let code = unsafe { SetDisplayConfig(Some(paths), modes, flags) };
        if code != 0 {
            bail!("{action} (SetDisplayConfig: {code})");
        }
        Ok(())
    }

    fn query(flags: QUERY_DISPLAY_CONFIG_FLAGS) -> Result<Snapshot> {
        for _ in 0..5 {
            let mut path_count = 0u32;
            let mut mode_count = 0u32;
            let code =
                unsafe { GetDisplayConfigBufferSizes(flags, &mut path_count, &mut mode_count) };
            if code != ERROR_SUCCESS {
                bail!("GetDisplayConfigBufferSizes failed: {}", code.0);
            }

            let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
            let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
            let code = unsafe {
                QueryDisplayConfig(
                    flags,
                    &mut path_count,
                    paths.as_mut_ptr(),
                    &mut mode_count,
                    modes.as_mut_ptr(),
                    None,
                )
            };

            if code == ERROR_INSUFFICIENT_BUFFER {
                continue;
            }
            if code != ERROR_SUCCESS {
                bail!("QueryDisplayConfig failed: {}", code.0);
            }

            paths.truncate(path_count as usize);
            modes.truncate(mode_count as usize);
            return Ok(Snapshot { paths, modes });
        }

        bail!("display topology kept changing while it was being queried")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn source_assignment_rehomes_a_flexible_group() {
            let source_a = (0, 1, 0);
            let source_b = (0, 1, 1);
            let assigned =
                assign_sources(&[vec![source_a, source_b], vec![source_a]]).expect("valid routing");

            assert_eq!(assigned, vec![source_b, source_a]);
        }

        #[test]
        fn source_assignment_rejects_an_impossible_routing() {
            let only_source = (0, 1, 0);
            assert!(assign_sources(&[vec![only_source], vec![only_source]]).is_none());
        }

        #[test]
        fn startup_recovery_triggers_when_no_active_target_is_available() {
            assert!(topology_state_needs_recovery(&[]));
            assert!(topology_state_needs_recovery(&[false]));
            assert!(topology_state_needs_recovery(&[false, false]));
            assert!(!topology_state_needs_recovery(&[true]));
            assert!(!topology_state_needs_recovery(&[false, true]));
        }

        #[test]
        fn setting_refresh_invalidates_the_current_target_timing() {
            let mut path = DISPLAYCONFIG_PATH_INFO::default();
            path.targetInfo.Anonymous.modeInfoIdx = 7;
            let requested = DISPLAYCONFIG_RATIONAL {
                Numerator: 143_990,
                Denominator: 1_000,
            };

            set_path_refresh_rate(&mut path, requested);

            assert_eq!(path.targetInfo.refreshRate.Numerator, 143_990);
            assert_eq!(path.targetInfo.refreshRate.Denominator, 1_000);
            assert_eq!(
                unsafe { path.targetInfo.Anonymous.modeInfoIdx },
                INVALID_MODE_INDEX
            );
        }

        #[test]
        fn final_mode_patch_restores_saved_orientation_and_scaling() {
            let mut path = DISPLAYCONFIG_PATH_INFO::default();
            let saved = MonitorConfig {
                identity: MonitorIdentity {
                    device_path: String::new(),
                    adapter_low: 1,
                    adapter_high: 0,
                    target_id: 1,
                },
                friendly_name: "Portrait monitor".into(),
                enabled: true,
                source_adapter_low: 1,
                source_adapter_high: 0,
                source_id: 0,
                clone_group: None,
                rotation: Some(2),
                scaling: Some(3),
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
                refresh_hz: 60.0,
                refresh_numerator: Some(60),
                refresh_denominator: Some(1),
            };

            patch_target_settings(&mut path, &saved);

            assert_eq!(path.targetInfo.rotation, DISPLAYCONFIG_ROTATION(2));
            assert_eq!(path.targetInfo.scaling, DISPLAYCONFIG_SCALING(3));
        }
    }
}

#[cfg(windows)]
pub fn capture_layout(name: impl Into<String>) -> Result<MonitorLayout> {
    win::capture_layout_impl(name)
}

#[cfg(windows)]
pub fn apply_layout(layout: &MonitorLayout) -> Result<()> {
    win::apply_layout_impl(layout)
}

#[cfg(windows)]
pub fn startup_topology_needs_recovery() -> Result<bool> {
    win::startup_topology_needs_recovery_impl()
}

#[cfg(windows)]
pub fn ensure_layout_available(layout: &MonitorLayout) -> Result<()> {
    win::ensure_layout_available_impl(layout)
}

#[cfg(windows)]
pub fn restore_connected_topology() -> Result<()> {
    win::restore_connected_topology_impl()
}
