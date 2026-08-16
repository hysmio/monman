use crate::model::AppConfig;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

pub fn config_path() -> PathBuf {
    if let Ok(appdata) = std::env::var("APPDATA") {
        return PathBuf::from(appdata).join("MonMan").join("layouts.json");
    }

    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("monman-layouts.json")
}

fn backup_path(path: &Path) -> PathBuf {
    let mut backup = path.as_os_str().to_os_string();
    backup.push(".bak");
    PathBuf::from(backup)
}

fn read_config(path: &Path) -> Result<AppConfig> {
    let data =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&data).with_context(|| format!("failed to parse {}", path.display()))
}

pub fn load() -> Result<AppConfig> {
    let path = config_path();
    let backup = backup_path(&path);

    if !path.exists() {
        if backup.exists() {
            let mut config = read_config(&backup)
                .context("primary config is missing and backup recovery failed")?;
            migrate_legacy_clone_groups(&mut config);
            return Ok(config);
        }
        return Ok(AppConfig::default());
    }

    match read_config(&path) {
        Ok(mut config) => {
            migrate_legacy_clone_groups(&mut config);
            Ok(config)
        }
        Err(primary_err) if backup.exists() => {
            let mut config = read_config(&backup).with_context(|| {
                format!("primary config failed ({primary_err:#}) and backup recovery also failed")
            })?;
            migrate_legacy_clone_groups(&mut config);
            Ok(config)
        }
        Err(err) => Err(err),
    }
}

pub fn save(config: &AppConfig) -> Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let data = serde_json::to_string_pretty(config)?;

    // Keep the last known parseable config before replacing the primary file.
    // This makes a partial/interrupted primary write recoverable on the next launch.
    if path.exists() && read_config(&path).is_ok() {
        let backup = backup_path(&path);
        fs::copy(&path, &backup).with_context(|| {
            format!(
                "failed to back up {} to {}",
                path.display(),
                backup.display()
            )
        })?;
    }

    fs::write(&path, data).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn migrate_legacy_clone_groups(config: &mut AppConfig) {
    for layout in &mut config.layouts {
        let mut next_group = layout
            .monitors
            .iter()
            .filter_map(|monitor| monitor.clone_group)
            .max()
            .unwrap_or(0)
            .saturating_add(1);

        let mut by_source = HashMap::<(i32, u32, u32), Vec<usize>>::new();
        for (index, monitor) in layout.monitors.iter().enumerate() {
            if monitor.enabled && monitor.clone_group.is_none() {
                by_source
                    .entry((
                        monitor.source_adapter_high,
                        monitor.source_adapter_low,
                        monitor.source_id,
                    ))
                    .or_default()
                    .push(index);
            }
        }

        // In legacy files, equal source ids among *active* monitors can only mean
        // cloning. Inactive path source ids are intentionally ignored because they
        // merely represented possible routing choices in QDC_ALL_PATHS.
        for indices in by_source.into_values().filter(|indices| indices.len() > 1) {
            let group = next_group;
            next_group = next_group.saturating_add(1);
            for index in indices {
                layout.monitors[index].clone_group = Some(group);
            }
        }
    }
}
