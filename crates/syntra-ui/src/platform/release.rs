//! Installation from published GitHub releases.
//!
//! The release workflow MUST publish an archive named
//! `syntra-{version}-{os}-{arch}.tar.gz` (or `.zip`), where version is the
//! semver tag without `v`, and os/arch are Rust's `std::env::consts` values.
//! It must also publish a sibling `{archive}.sha256` containing the conventional
//! SHA-256 text when integrity verification is desired. A checksum proves only
//! transport integrity: without a digital signature it does NOT authenticate
//! the publisher or the release contents.

use flate2::read::GzDecoder;
use reqwest::blocking::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use thiserror::Error;

const API: &str = "https://api.github.com/repos/DarkPhilosophy/syntra/releases";
const TEMPLATE: &str = "syntra-{version}-{os}-{arch}";

#[derive(Clone, Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    pub name: String,
    pub html_url: String,
    pub assets: Vec<Asset>,
}
#[derive(Clone, Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
    pub size: u64,
}

#[derive(Debug, Error)]
pub enum ReleaseError {
    #[error("no network connection to GitHub: {0}")]
    Network(#[source] reqwest::Error),
    #[error("GitHub rate limit or HTTP failure: {0}")]
    Http(#[source] reqwest::Error),
    #[error("GitHub returned malformed release data: {0}")]
    Json(#[source] reqwest::Error),
    #[error("no release asset matches {0}")]
    NoMatchingAsset(String),
    #[error("checksum mismatch for {asset}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        asset: String,
        expected: String,
        actual: String,
    },
    #[error("checksum file is malformed: {0}")]
    BadChecksum(String),
    #[error("archive unpack failed: {0}")]
    Unpack(#[source] io::Error),
    #[error("installation destination is not writable: {0}")]
    Destination(#[source] crate::platform::install::InstallError),
    #[error("operation cancelled")]
    Cancelled,
    #[error("download failed: {0}")]
    Io(#[source] io::Error),
}

pub type ProgressCallback = Arc<dyn Fn(u64, Option<u64>, &str) + Send + Sync>;

fn client() -> Result<Client, ReleaseError> {
    Client::builder()
        .https_only(true)
        .timeout(Duration::from_secs(30))
        .user_agent("syntra-updater")
        .build()
        .map_err(ReleaseError::Network)
}
fn fetch(url: &str) -> Result<reqwest::blocking::Response, ReleaseError> {
    let r = client()?
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .map_err(ReleaseError::Network)?;
    r.error_for_status().map_err(ReleaseError::Http)
}

/// Lists recent published releases (newest first).
pub fn list_recent(limit: usize) -> Result<Vec<Release>, ReleaseError> {
    fetch(&format!("{API}?per_page={}", limit.clamp(1, 100)))
        .and_then(|r| r.json().map_err(ReleaseError::Json))
}
/// Fetches the latest published release.
pub fn latest() -> Result<Release, ReleaseError> {
    fetch(&format!("{API}/latest")).and_then(|r| r.json().map_err(ReleaseError::Json))
}

/// Downloads, verifies, unpacks and installs a release. `cancel` is checked
/// while downloading and extracting; progress reports received/total bytes.
pub fn install_release(
    release: &Release,
    destination: &Path,
    cancel: Arc<AtomicBool>,
    progress: ProgressCallback,
) -> Result<crate::platform::install::InstalledBinaries, ReleaseError> {
    let stem = TEMPLATE
        .replace("{version}", release.tag_name.trim_start_matches('v'))
        .replace("{os}", std::env::consts::OS)
        .replace("{arch}", std::env::consts::ARCH);
    let asset = release
        .assets
        .iter()
        .find(|a| a.name == format!("{stem}.tar.gz") || a.name == format!("{stem}.zip"))
        .ok_or_else(|| ReleaseError::NoMatchingAsset(stem.clone()))?;
    let dir = tempfile::tempdir().map_err(ReleaseError::Io)?;
    let archive = dir.path().join(&asset.name);
    let mut response = fetch(&asset.browser_download_url)?;
    let mut file = fs::File::create(&archive).map_err(ReleaseError::Io);
    let mut file = file?;
    let mut hash = Sha256::new();
    let mut received = 0;
    let mut buf = [0u8; 8192];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err(ReleaseError::Cancelled);
        }
        let n = response.read(&mut buf).map_err(ReleaseError::Io)?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(ReleaseError::Io)?;
        hash.update(&buf[..n]);
        received += n as u64;
        progress(received, Some(asset.size), "Downloading release");
    }
    let checksum_name = format!("{}.sha256", asset.name);
    if let Some(sum) = release.assets.iter().find(|a| a.name == checksum_name) {
        let text = String::from_utf8_lossy(
            &fetch(&sum.browser_download_url)?
                .bytes()
                .map_err(ReleaseError::Http)?,
        )
        .into_owned();
        let expected = text
            .split_whitespace()
            .next()
            .ok_or_else(|| ReleaseError::BadChecksum("empty checksum".into()))?
            .to_ascii_lowercase();
        let actual = format!("{:x}", hash.finalize());
        if expected != actual {
            return Err(ReleaseError::ChecksumMismatch {
                asset: asset.name.clone(),
                expected,
                actual,
            });
        }
    }
    let extracted = dir.path().join("extracted");
    fs::create_dir_all(&extracted).map_err(ReleaseError::Unpack)?;
    if asset.name.ends_with(".tar.gz") {
        let f = fs::File::open(&archive).map_err(ReleaseError::Unpack)?;
        tar::Archive::new(GzDecoder::new(f))
            .unpack(&extracted)
            .map_err(ReleaseError::Unpack)?;
    } else {
        let f = fs::File::open(&archive).map_err(ReleaseError::Unpack)?;
        let mut z =
            zip::ZipArchive::new(f).map_err(|e| ReleaseError::Unpack(io::Error::other(e)))?;
        z.extract(&extracted)
            .map_err(|e| ReleaseError::Unpack(io::Error::other(e)))?;
    }
    let source = find_root(&extracted);
    if cancel.load(Ordering::Relaxed) {
        return Err(ReleaseError::Cancelled);
    }
    progress(received, Some(received), "Installing release");
    crate::platform::install::install_binaries_from_to(&source, destination)
        .map_err(ReleaseError::Destination)
}
fn find_root(dir: &Path) -> PathBuf {
    if dir
        .join(crate::platform::install::APPLICATION_EXECUTABLE)
        .is_file()
    {
        return dir.into();
    }
    fs::read_dir(dir)
        .ok()
        .and_then(|mut i| i.next())
        .and_then(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| dir.into())
}
trait WriteExt {
    fn write_all(&mut self, b: &[u8]) -> io::Result<()>;
}
impl WriteExt for fs::File {
    fn write_all(&mut self, b: &[u8]) -> io::Result<()> {
        use std::io::Write;
        Write::write_all(self, b)
    }
}
