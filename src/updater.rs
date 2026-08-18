use anyhow::{Context, Result, anyhow, bail};
use eframe::egui;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

const RELEASE_API: &str = "https://api.github.com/repos/hysmio/monman/releases/latest";
const MAX_INSTALLER_SIZE: u64 = 100 * 1024 * 1024;
const UPDATE_FILE_PREFIX: &str = "monman-update-";

#[derive(Clone, Debug)]
pub struct AvailableUpdate {
    pub tag: String,
    asset_name: String,
    download_url: String,
    sha256: String,
    size: u64,
}

pub enum UpdateEvent {
    Available(AvailableUpdate),
    InstallerLaunched,
    InstallFailed(String),
}

pub struct UpdateManager {
    events: Receiver<UpdateEvent>,
    event_sender: Sender<UpdateEvent>,
    context: egui::Context,
}

impl UpdateManager {
    pub fn new(context: egui::Context) -> Self {
        let (event_sender, events) = mpsc::channel();
        let manager = Self {
            events,
            event_sender,
            context,
        };

        #[cfg(windows)]
        {
            clean_stale_installers();
            let sender = manager.event_sender.clone();
            let context = manager.context.clone();
            let _ = std::thread::Builder::new()
                .name("monman-update-check".into())
                .spawn(move || {
                    if let Ok(Some(update)) = check_for_update() {
                        let _ = sender.send(UpdateEvent::Available(update));
                        context.request_repaint();
                    }
                });
        }

        manager
    }

    pub fn try_recv(&self) -> Option<UpdateEvent> {
        self.events.try_recv().ok()
    }

    pub fn install(&self, update: AvailableUpdate) {
        let sender = self.event_sender.clone();
        let context = self.context.clone();
        let failure_sender = sender.clone();
        let result = std::thread::Builder::new()
            .name("monman-update-install".into())
            .spawn(move || {
                let event = match download_and_launch(&update) {
                    Ok(()) => UpdateEvent::InstallerLaunched,
                    Err(error) => UpdateEvent::InstallFailed(format!("{error:#}")),
                };
                let _ = sender.send(event);
                context.request_repaint();
            });

        if let Err(error) = result {
            let _ = failure_sender.send(UpdateEvent::InstallFailed(format!(
                "Could not start the updater: {error}"
            )));
            self.context.request_repaint();
        }
    }
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
    size: u64,
}

fn check_for_update() -> Result<Option<AvailableUpdate>> {
    let mut response = github_get(RELEASE_API)?;
    let release: GitHubRelease = response
        .body_mut()
        .read_json()
        .context("GitHub returned invalid release metadata")?;
    select_update(release, env!("CARGO_PKG_VERSION"))
}

fn select_update(release: GitHubRelease, current: &str) -> Result<Option<AvailableUpdate>> {
    let current = Version::parse(current).context("the application version is invalid")?;
    let latest = Version::parse(
        release
            .tag_name
            .strip_prefix('v')
            .ok_or_else(|| anyhow!("release tag is not v-prefixed"))?,
    )
    .context("the latest release tag is not a semantic version")?;

    if latest.cmp_precedence(&current).is_le() {
        return Ok(None);
    }

    let expected_name = format!("monman-{}-windows-x86_64-setup.exe", release.tag_name);
    let asset = release
        .assets
        .into_iter()
        .find(|asset| asset.name == expected_name)
        .ok_or_else(|| anyhow!("release does not contain {expected_name}"))?;
    if asset.size > MAX_INSTALLER_SIZE {
        bail!("release installer is unexpectedly large");
    }

    let sha256 = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|digest| digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()))
        .ok_or_else(|| anyhow!("release installer does not have a valid SHA-256 digest"))?
        .to_ascii_lowercase();

    Ok(Some(AvailableUpdate {
        tag: release.tag_name,
        asset_name: asset.name,
        download_url: asset.browser_download_url,
        sha256,
        size: asset.size,
    }))
}

fn download_and_launch(update: &AvailableUpdate) -> Result<()> {
    let installer_path = std::env::temp_dir().join(format!(
        "{UPDATE_FILE_PREFIX}{}-{}.exe",
        update.tag,
        std::process::id()
    ));

    if let Err(error) = download_installer(update, &installer_path) {
        let _ = fs::remove_file(&installer_path);
        return Err(error);
    }

    Command::new(&installer_path)
        .args([
            "/VERYSILENT",
            "/SUPPRESSMSGBOXES",
            "/NORESTART",
            "/CLOSEAPPLICATIONS",
        ])
        .spawn()
        .with_context(|| format!("could not launch {}", update.asset_name))?;
    Ok(())
}

fn download_installer(update: &AvailableUpdate, path: &PathBuf) -> Result<()> {
    let mut response = github_get(&update.download_url).context("could not download update")?;
    let mut reader = response.body_mut().as_reader();
    let mut file = File::create(path).context("could not create the temporary installer")?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut bytes_written = 0_u64;

    loop {
        let count = reader
            .read(&mut buffer)
            .context("could not read the update download")?;
        if count == 0 {
            break;
        }
        bytes_written += count as u64;
        if bytes_written > MAX_INSTALLER_SIZE {
            bail!("downloaded installer is unexpectedly large");
        }
        file.write_all(&buffer[..count])
            .context("could not write the temporary installer")?;
        hasher.update(&buffer[..count]);
    }
    file.sync_all()
        .context("could not finish writing the temporary installer")?;

    if bytes_written != update.size {
        bail!(
            "installer size mismatch: expected {} bytes, received {bytes_written}",
            update.size
        );
    }

    let actual_digest = format!("{:x}", hasher.finalize());
    if actual_digest != update.sha256 {
        bail!("installer SHA-256 digest did not match the GitHub release");
    }
    Ok(())
}

fn github_get(url: &str) -> Result<ureq::http::Response<ureq::Body>> {
    let config = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(Duration::from_secs(5 * 60)))
        .build();
    ureq::Agent::new_with_config(config)
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", concat!("MonMan/", env!("CARGO_PKG_VERSION")))
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()
        .context("GitHub request failed")
}

fn clean_stale_installers() {
    let Ok(entries) = fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(UPDATE_FILE_PREFIX) || !name.ends_with(".exe") {
            continue;
        }
        let is_stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
            .is_ok_and(|age| age > Duration::from_secs(24 * 60 * 60));
        if is_stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, digest: Option<&str>) -> GitHubRelease {
        GitHubRelease {
            tag_name: tag.into(),
            assets: vec![GitHubAsset {
                name: format!("monman-{tag}-windows-x86_64-setup.exe"),
                browser_download_url: "https://example.invalid/installer.exe".into(),
                digest: digest.map(str::to_owned),
                size: 42,
            }],
        }
    }

    #[test]
    fn selects_newer_semver_release() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let update = select_update(release("v1.2.0", Some(&digest)), "1.1.9")
            .unwrap()
            .unwrap();
        assert_eq!(update.tag, "v1.2.0");
    }

    #[test]
    fn ignores_current_or_older_release() {
        assert!(
            select_update(release("v1.2.0", None), "1.2.0")
                .unwrap()
                .is_none()
        );
        assert!(
            select_update(release("v1.1.9", None), "1.2.0")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn rejects_unverified_installer() {
        assert!(select_update(release("v1.2.0", None), "1.1.0").is_err());
    }
}
