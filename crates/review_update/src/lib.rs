//! Updates Galley from its GitHub releases: finds out whether a newer version was
//! published and swaps the installed `galley.exe` for it.
//!
//! Each release carries `galley-windows-x64.exe` and a `SHA256SUMS` file; the
//! download is only installed when its hash matches. Windows can't overwrite a
//! running executable but can rename it, so the current one moves aside to
//! `galley.old.exe` and is deleted on a later start.

use std::fs::File;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use semver::Version;
use sha2::{Digest as _, Sha256};
use ureq::Agent;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

const LATEST_RELEASE_URL: &str = "https://api.github.com/repos/nicobobb/galley/releases/latest";
const EXE_ASSET: &str = "galley-windows-x64.exe";
const SUMS_ASSET: &str = "SHA256SUMS";
const OLD_EXE: &str = "galley.old.exe";
const NEW_EXE: &str = "galley.new.exe";

#[derive(Debug, Clone)]
pub struct Release {
    pub version: Version,
    exe_url: String,
    sums_url: String,
}

/// The `galley.exe` that updates may replace: the running one, but only when it's
/// the copy `install.ps1` put in `%LOCALAPPDATA%\Programs\Galley`. Builds run from
/// `target` or `~/.cargo/bin` are left alone.
pub fn installed_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let install_dir = PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
        .join("Programs")
        .join("Galley");
    let dir = exe.parent()?;
    dir.as_os_str()
        .to_string_lossy()
        .trim_end_matches('\\')
        .eq_ignore_ascii_case(install_dir.to_string_lossy().trim_end_matches('\\'))
        .then_some(exe)
}

/// Whether `check_for_updates` isn't turned off in the settings file at `path`.
pub fn enabled_in(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|settings| settings.get("check_for_updates")?.as_bool())
        .unwrap_or(true)
}

/// Deletes the executable left behind by the last update. It fails harmlessly while
/// another Galley window still runs from it; a later start retries.
pub fn remove_old_exe(exe: &Path) {
    if let Some(dir) = exe.parent() {
        std::fs::remove_file(dir.join(OLD_EXE)).ok();
    }
}

/// The latest release, if it's newer than this build.
///
/// Unauthenticated API calls are limited to 60 an hour per IP, far more than
/// Galley makes.
pub fn check() -> Result<Option<Release>> {
    let json = agent(Duration::from_secs(15))
        .get(LATEST_RELEASE_URL)
        .header("Accept", "application/vnd.github+json")
        .call()
        .context("couldn't reach GitHub")?
        .body_mut()
        .read_to_string()?;
    newer_release(&json, &current_version())
}

/// Downloads `release` and puts it in place of `exe`. The running process keeps
/// working; the new version starts the next time Galley does.
pub fn install(release: &Release, exe: &Path) -> Result<()> {
    let dir = exe.parent().context("the executable has no folder")?;
    let agent = agent(Duration::from_secs(10 * 60));

    let sums = agent
        .get(&release.sums_url)
        .call()
        .context("couldn't download the checksums")?
        .body_mut()
        .read_to_string()?;
    let expected = expected_hash(&sums, EXE_ASSET)?;

    let new_exe = dir.join(NEW_EXE);
    let response = agent
        .get(&release.exe_url)
        .call()
        .context("couldn't download the new version")?;
    let hash = download(response.into_body().into_reader(), &new_exe).inspect_err(|_| {
        std::fs::remove_file(&new_exe).ok();
    })?;
    if !hash.eq_ignore_ascii_case(&expected) {
        std::fs::remove_file(&new_exe).ok();
        bail!("the download doesn't match its checksum; nothing was changed");
    }

    let old_exe = dir.join(OLD_EXE);
    match std::fs::remove_file(&old_exe) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            std::fs::remove_file(&new_exe).ok();
            return Err(error).context(
                "the previous version is still running in another Galley window; close it and try again",
            );
        }
        _ => {}
    }
    std::fs::rename(exe, &old_exe).context("couldn't move the current version aside")?;
    if let Err(error) = std::fs::rename(&new_exe, exe) {
        std::fs::rename(&old_exe, exe).ok();
        return Err(error).context("couldn't put the new version in place");
    }
    Ok(())
}

fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the crate version is valid semver")
}

fn agent(timeout: Duration) -> Agent {
    Agent::config_builder()
        .tls_config(
            TlsConfig::builder()
                .provider(TlsProvider::NativeTls)
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .timeout_global(Some(timeout))
        .user_agent(concat!("galley/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// Parses GitHub's "latest release" response. Releases without the expected
/// assets (or with an unparsable tag) are skipped rather than reported as errors.
fn newer_release(json: &str, current: &Version) -> Result<Option<Release>> {
    let release: serde_json::Value =
        serde_json::from_str(json).context("unexpected response from GitHub")?;
    let Some(version) = release["tag_name"]
        .as_str()
        .and_then(|tag| Version::parse(tag.trim_start_matches('v')).ok())
    else {
        return Ok(None);
    };
    if version <= *current {
        return Ok(None);
    }
    let asset_url = |name: &str| {
        release["assets"]
            .as_array()?
            .iter()
            .find(|asset| asset["name"] == name)?["browser_download_url"]
            .as_str()
            .map(str::to_string)
    };
    Ok(match (asset_url(EXE_ASSET), asset_url(SUMS_ASSET)) {
        (Some(exe_url), Some(sums_url)) => Some(Release {
            version,
            exe_url,
            sums_url,
        }),
        _ => None,
    })
}

/// Finds `name`'s hash in `sha256sum`-style output: `<hash>  <name>` per line.
fn expected_hash(sums: &str, name: &str) -> Result<String> {
    sums.lines()
        .find_map(|line| {
            let (hash, file) = line.split_once(char::is_whitespace)?;
            (file.trim_start_matches([' ', '*']) == name).then(|| hash.to_string())
        })
        .with_context(|| format!("{SUMS_ASSET} has no entry for {name}"))
}

/// Writes `reader` to `path`, returning the SHA-256 of what was written.
fn download(mut reader: impl io::Read, path: &Path) -> Result<String> {
    let mut file =
        File::create(path).with_context(|| format!("couldn't create {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .context("the download was interrupted")?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])?;
    }
    file.sync_all()?;
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASE: &str = r#"{
        "tag_name": "v0.2.0",
        "assets": [
            {"name": "galley-0.2.0-windows-x64.zip", "browser_download_url": "https://example.com/zip"},
            {"name": "galley-windows-x64.exe", "browser_download_url": "https://example.com/exe"},
            {"name": "SHA256SUMS", "browser_download_url": "https://example.com/sums"}
        ]
    }"#;

    #[test]
    fn reports_only_newer_releases_with_both_assets() {
        let release = newer_release(RELEASE, &Version::new(0, 1, 0))
            .unwrap()
            .unwrap();
        assert_eq!(release.version, Version::new(0, 2, 0));
        assert_eq!(release.exe_url, "https://example.com/exe");
        assert_eq!(release.sums_url, "https://example.com/sums");

        assert!(
            newer_release(RELEASE, &Version::new(0, 2, 0))
                .unwrap()
                .is_none()
        );
        assert!(
            newer_release(RELEASE, &Version::new(1, 0, 0))
                .unwrap()
                .is_none()
        );

        let without_exe = RELEASE.replace("galley-windows-x64.exe", "other.exe");
        assert!(
            newer_release(&without_exe, &Version::new(0, 1, 0))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn finds_hashes_in_sha256sum_output() {
        let sums = "AB12  galley-0.2.0-windows-x64.zip\ncd34 *galley-windows-x64.exe\n";
        assert_eq!(expected_hash(sums, EXE_ASSET).unwrap(), "cd34");
        assert!(expected_hash(sums, "missing.exe").is_err());
    }

    #[test]
    fn hashes_what_it_downloads() {
        let path = std::env::temp_dir().join(format!("galley_update_{}", std::process::id()));
        let hash = download(&b"hello"[..], &path).unwrap();
        assert_eq!(
            hash,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn reads_the_opt_out_setting() {
        let dir =
            std::env::temp_dir().join(format!("galley_update_settings_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        assert!(enabled_in(&path));
        std::fs::write(&path, r#"{"allowed_commands": {}}"#).unwrap();
        assert!(enabled_in(&path));
        std::fs::write(&path, r#"{"check_for_updates": false}"#).unwrap();
        assert!(!enabled_in(&path));
        std::fs::remove_dir_all(&dir).ok();
    }
}
