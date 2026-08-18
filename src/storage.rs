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
    let mut config = serde_json::from_str(&data)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    migrate_legacy_clone_groups(&mut config);
    Ok(config)
}

pub fn load() -> Result<AppConfig> {
    let path = config_path();
    let backup = backup_path(&path);

    if !path.exists() {
        return if backup.exists() {
            read_config(&backup).context("primary config is missing and backup recovery failed")
        } else {
            Ok(AppConfig::default())
        };
    }

    match read_config(&path) {
        Ok(config) => Ok(config),
        Err(primary_err) if backup.exists() => read_config(&backup).with_context(|| {
            format!("primary config failed ({primary_err:#}) and backup recovery also failed")
        }),
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

    // Keep a parseable backup in case the next write is interrupted.
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
                    .entry(monitor.source_key())
                    .or_default()
                    .push(index);
            }
        }

        // Only active legacy paths sharing a source represent clones.
        for indices in by_source.into_values().filter(|indices| indices.len() > 1) {
            let group = next_group;
            next_group = next_group.saturating_add(1);
            for index in indices {
                layout.monitors[index].clone_group = Some(group);
            }
        }
    }
}
