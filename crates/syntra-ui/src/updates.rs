//! GitHub Releases update discovery and bounded asset staging.
//!
//! The module has no implicit release source: [`Updater::prepare`] returns
//! [`Preparation::Unconfigured`] when both repository fields are absent. A configured
//! updater checks GitHub's real `releases/latest` endpoint only when [`Updater::check`]
//! is called. Downloads are written to a caller-selected staging directory and are
//! never executed, installed, or copied over the running executable.
//!
//! The implementation uses `reqwest::blocking`; [`Updater::prepare`] is validation-only,
//! while [`Updater::check`] and [`Updater::download`] must run on a dedicated blocking thread.
//! A successful download deliberately retains its unique `.part` filename to make the
//! staged-only state unmistakable to the later, platform-specific installation boundary.

use semver::Version;
use serde::Deserialize;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};
use thiserror::Error;

const GITHUB_API: &str = "https://api.github.com";
const USER_AGENT: &str = "syntra-updater";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
static STAGING_NONCE: AtomicU64 = AtomicU64::new(0);

/// Optional GitHub repository coordinates. No owner/repository is assumed.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RepositoryConfig {
    pub owner: Option<String>,
    pub repository: Option<String>,
}

/// Exact, configurable artifact naming contract.
///
/// `template` is matched against release asset names after replacing `{version}`,
/// `{os}`, and `{arch}`. It must contain `{os}` and `{arch}`; `{version}` is optional.
/// For example: `syntra-{version}-{os}-{arch}.tar.gz`. Exact matching deliberately
/// rejects both unsupported platforms and multiple assets with the same name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactNaming {
    pub template: String,
    pub os: String,
    pub arch: String,
}

impl ArtifactNaming {
    pub fn for_current_platform(template: impl Into<String>) -> Self {
        Self {
            template: template.into(),
            os: std::env::consts::OS.to_owned(),
            arch: std::env::consts::ARCH.to_owned(),
        }
    }

    fn validate(&self) -> Result<(), UpdateError> {
        if self.template.trim().is_empty() {
            return Err(UpdateError::InvalidArtifactNaming(
                "template is empty".into(),
            ));
        }
        if !self.template.contains("{os}") || !self.template.contains("{arch}") {
            return Err(UpdateError::InvalidArtifactNaming(
                "template must contain {os} and {arch}".into(),
            ));
        }
        if self.os.is_empty()
            || self.arch.is_empty()
            || self.os.contains(['/', '\\'])
            || self.arch.contains(['/', '\\'])
        {
            return Err(UpdateError::InvalidArtifactNaming(
                "OS and architecture must be non-empty filename components".into(),
            ));
        }
        let mut rest = self.template.as_str();
        while let Some(open) = rest.find('{') {
            rest = &rest[open..];
            let Some(close) = rest.find('}') else {
                return Err(UpdateError::InvalidArtifactNaming(
                    "unclosed placeholder".into(),
                ));
            };
            match &rest[..=close] {
                "{version}" | "{os}" | "{arch}" => {}
                placeholder => {
                    return Err(UpdateError::InvalidArtifactNaming(format!(
                        "unknown placeholder {placeholder}"
                    )));
                }
            }
            rest = &rest[close + 1..];
        }
        if rest.contains('}') {
            return Err(UpdateError::InvalidArtifactNaming(
                "unmatched closing brace".into(),
            ));
        }
        Ok(())
    }

    fn expected_name(&self, version: &Version) -> String {
        self.template
            .replace("{version}", &version.to_string())
            .replace("{os}", &self.os)
            .replace("{arch}", &self.arch)
    }
}

/// Result of preparing an updater. Preparation never performs network I/O.
#[derive(Debug)]
pub enum Preparation {
    Unconfigured,
    Configured(Updater),
}

/// Configured, synchronous update client.
#[derive(Debug)]
pub struct Updater {
    owner: String,
    repository: String,
    current_version: Version,
    naming: ArtifactNaming,
    max_download_bytes: u64,
}

impl Updater {
    /// Validates configuration without constructing an HTTP client or making a request.
    /// Missing owner *and* repository is an honest unconfigured state; supplying only
    /// one field is an error. `max_download_bytes` must be non-zero.
    pub fn prepare(
        config: RepositoryConfig,
        current_version: &str,
        naming: ArtifactNaming,
        max_download_bytes: u64,
    ) -> Result<Preparation, UpdateError> {
        let coordinates = match (config.owner, config.repository) {
            (None, None) => return Ok(Preparation::Unconfigured),
            (Some(owner), Some(repository)) => (owner, repository),
            _ => {
                return Err(UpdateError::InvalidConfiguration(
                    "owner and repository must be supplied together".into(),
                ));
            }
        };
        validate_slug("owner", &coordinates.0)?;
        validate_slug("repository", &coordinates.1)?;
        naming.validate()?;
        if max_download_bytes == 0 {
            return Err(UpdateError::InvalidConfiguration(
                "maximum download size must be greater than zero".into(),
            ));
        }
        let current_version = Version::parse(current_version.trim_start_matches('v'))
            .map_err(|source| UpdateError::InvalidCurrentVersion(source.to_string()))?;
        Ok(Preparation::Configured(Self {
            owner: coordinates.0,
            repository: coordinates.1,
            current_version,
            naming,
            max_download_bytes,
        }))
    }

    /// Performs a real HTTPS request to GitHub and compares the latest stable release.
    pub fn check(&self) -> Result<UpdateStatus, UpdateError> {
        let url = format!(
            "{GITHUB_API}/repos/{}/{}/releases/latest",
            self.owner, self.repository
        );
        let client = build_client()?;
        let response = client
            .get(url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .map_err(UpdateError::Http)?
            .error_for_status()
            .map_err(UpdateError::Http)?;
        let release: GitHubRelease = response.json().map_err(UpdateError::Http)?;
        classify_release(&self.current_version, &self.naming, release)
    }

    /// Streams one selected release asset into `staging_directory` with a hard byte
    /// limit. Partial files are removed on every read/write/size failure. Success means
    /// only that a staged file exists; the result intentionally has no installed state.
    pub fn download(
        &self,
        available: &AvailableUpdate,
        staging_directory: &Path,
    ) -> Result<DownloadedAsset, UpdateError> {
        if let Some(length) = available.asset.declared_bytes {
            if length > self.max_download_bytes {
                return Err(UpdateError::DownloadTooLarge {
                    limit: self.max_download_bytes,
                    observed: length,
                });
            }
        }
        fs::create_dir_all(staging_directory).map_err(UpdateError::Io)?;
        let client = build_client()?;
        let response = client
            .get(&available.asset.download_url)
            .header(reqwest::header::ACCEPT, "application/octet-stream")
            .send()
            .map_err(UpdateError::Http)?
            .error_for_status()
            .map_err(UpdateError::Http)?;
        if let Some(length) = response.content_length() {
            if length > self.max_download_bytes {
                return Err(UpdateError::DownloadTooLarge {
                    limit: self.max_download_bytes,
                    observed: length,
                });
            }
        }
        let (staged_path, bytes) = stage_reader(
            response,
            staging_directory,
            &available.asset.name,
            self.max_download_bytes,
        )?;
        Ok(DownloadedAsset {
            release_version: available.version.clone(),
            asset_name: available.asset.name.clone(),
            staged_path,
            bytes,
        })
    }
}

fn build_client() -> Result<reqwest::blocking::Client, UpdateError> {
    reqwest::blocking::Client::builder()
        .https_only(true)
        .timeout(DEFAULT_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(UpdateError::Http)
}

/// Current-version comparison result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateStatus {
    UpToDate { current: Version },
    AheadOfLatest { current: Version, latest: Version },
    Available(AvailableUpdate),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AvailableUpdate {
    pub version: Version,
    pub release_page: String,
    pub asset: ReleaseAsset,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseAsset {
    pub name: String,
    pub download_url: String,
    pub declared_bytes: Option<u64>,
}

/// A downloaded-but-not-installed artifact for a platform installation coordinator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadedAsset {
    pub release_version: Version,
    pub asset_name: String,
    pub staged_path: PathBuf,
    pub bytes: u64,
}

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("invalid updater configuration: {0}")]
    InvalidConfiguration(String),
    #[error("invalid artifact naming contract: {0}")]
    InvalidArtifactNaming(String),
    #[error("invalid current version: {0}")]
    InvalidCurrentVersion(String),
    #[error("malformed GitHub release: {0}")]
    MalformedRelease(String),
    #[error("release asset is unsupported; expected exactly {expected:?}")]
    UnsupportedAsset { expected: String },
    #[error("release asset selection is ambiguous: {count} assets named {expected:?}")]
    AmbiguousAsset { expected: String, count: usize },
    #[error("download exceeded {limit} bytes after receiving {observed} bytes")]
    DownloadTooLarge { limit: u64, observed: u64 },
    #[error("HTTP operation failed: {0}")]
    Http(#[source] reqwest::Error),
    #[error("staging I/O failed: {0}")]
    Io(#[source] io::Error),
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
    size: Option<u64>,
}

fn validate_slug(field: &str, value: &str) -> Result<(), UpdateError> {
    let valid = !value.is_empty()
        && value.len() <= 100
        && !value.starts_with('.')
        && !value.ends_with('.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(UpdateError::InvalidConfiguration(format!(
            "{field} is not a valid GitHub path component"
        )))
    }
}

fn classify_release(
    current: &Version,
    naming: &ArtifactNaming,
    release: GitHubRelease,
) -> Result<UpdateStatus, UpdateError> {
    if release.draft || release.prerelease {
        return Err(UpdateError::MalformedRelease(
            "latest endpoint returned a draft or prerelease".into(),
        ));
    }
    let version_text = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);
    let latest = Version::parse(version_text)
        .map_err(|error| UpdateError::MalformedRelease(format!("invalid tag version: {error}")))?;
    if !latest.pre.is_empty() {
        return Err(UpdateError::MalformedRelease(
            "release tag denotes a prerelease".into(),
        ));
    }
    match current.cmp(&latest) {
        std::cmp::Ordering::Equal => Ok(UpdateStatus::UpToDate {
            current: current.clone(),
        }),
        std::cmp::Ordering::Greater => Ok(UpdateStatus::AheadOfLatest {
            current: current.clone(),
            latest,
        }),
        std::cmp::Ordering::Less => {
            let expected = naming.expected_name(&latest);
            let mut matches = release
                .assets
                .into_iter()
                .filter(|asset| asset.name == expected);
            let Some(asset) = matches.next() else {
                return Err(UpdateError::UnsupportedAsset { expected });
            };
            let count = 1 + matches.count();
            if count != 1 {
                return Err(UpdateError::AmbiguousAsset { expected, count });
            }
            if !asset.browser_download_url.starts_with("https://") {
                return Err(UpdateError::MalformedRelease(
                    "asset download URL is not HTTPS".into(),
                ));
            }
            Ok(UpdateStatus::Available(AvailableUpdate {
                version: latest,
                release_page: release.html_url,
                asset: ReleaseAsset {
                    name: asset.name,
                    download_url: asset.browser_download_url,
                    declared_bytes: asset.size,
                },
            }))
        }
    }
}

fn stage_reader(
    mut reader: impl Read,
    directory: &Path,
    asset_name: &str,
    limit: u64,
) -> Result<(PathBuf, u64), UpdateError> {
    let safe_name = Path::new(asset_name)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| *name == asset_name && !name.is_empty())
        .ok_or_else(|| UpdateError::MalformedRelease("asset name is not a safe filename".into()))?;
    let nonce = STAGING_NONCE.fetch_add(1, Ordering::Relaxed);
    let path = directory.join(format!(
        ".{safe_name}.download-{}-{nonce}.part",
        std::process::id()
    ));
    let mut cleanup = PartialFile::create(path)?;
    let mut total = 0_u64;
    let mut buffer = [0_u8; 32 * 1024];
    loop {
        let remaining_with_sentinel = limit.saturating_sub(total).saturating_add(1);
        let read_capacity = usize::try_from(remaining_with_sentinel)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let read = reader
            .read(&mut buffer[..read_capacity])
            .map_err(UpdateError::Io)?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or(UpdateError::DownloadTooLarge {
                limit,
                observed: u64::MAX,
            })?;
        if total > limit {
            return Err(UpdateError::DownloadTooLarge {
                limit,
                observed: total,
            });
        }
        cleanup
            .file
            .write_all(&buffer[..read])
            .map_err(UpdateError::Io)?;
    }
    cleanup.file.flush().map_err(UpdateError::Io)?;
    cleanup.keep = true;
    Ok((cleanup.path.clone(), total))
}

struct PartialFile {
    path: PathBuf,
    file: File,
    keep: bool,
}

impl PartialFile {
    fn create(path: PathBuf) -> Result<Self, UpdateError> {
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(UpdateError::Io)?;
        Ok(Self {
            path,
            file,
            keep: false,
        })
    }
}

impl Drop for PartialFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn naming() -> ArtifactNaming {
        ArtifactNaming {
            template: "syntra-{version}-{os}-{arch}.zip".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
        }
    }

    fn release(tag: &str, names: &[&str]) -> GitHubRelease {
        GitHubRelease {
            tag_name: tag.into(),
            html_url: "https://github.com/acme/syntra/releases/tag/v2.0.0".into(),
            draft: false,
            prerelease: false,
            assets: names
                .iter()
                .map(|name| GitHubAsset {
                    name: (*name).into(),
                    browser_download_url: format!("https://example.invalid/{name}"),
                    size: Some(10),
                })
                .collect(),
        }
    }

    #[test]
    fn missing_repository_is_unconfigured() {
        let prepared =
            Updater::prepare(RepositoryConfig::default(), "1.0.0", naming(), 100).unwrap();
        assert!(matches!(prepared, Preparation::Unconfigured));
    }

    #[test]
    fn partial_or_unsafe_repository_is_rejected() {
        assert!(matches!(
            Updater::prepare(
                RepositoryConfig {
                    owner: Some("acme".into()),
                    repository: None
                },
                "1.0.0",
                naming(),
                100,
            ),
            Err(UpdateError::InvalidConfiguration(_))
        ));
        assert!(matches!(
            Updater::prepare(
                RepositoryConfig {
                    owner: Some("acme/other".into()),
                    repository: Some("syntra".into())
                },
                "1.0.0",
                naming(),
                100,
            ),
            Err(UpdateError::InvalidConfiguration(_))
        ));
    }

    #[test]
    fn malformed_release_version_is_rejected() {
        let error = classify_release(&Version::new(1, 0, 0), &naming(), release("nightly", &[]))
            .unwrap_err();
        assert!(matches!(error, UpdateError::MalformedRelease(_)));
        let error = classify_release(
            &Version::new(1, 0, 0),
            &naming(),
            release("v2.0.0-beta.1", &[]),
        )
        .unwrap_err();
        assert!(matches!(error, UpdateError::MalformedRelease(_)));
    }

    #[test]
    fn equal_and_older_releases_are_not_updates() {
        assert!(matches!(
            classify_release(&Version::new(2, 0, 0), &naming(), release("v2.0.0", &[])).unwrap(),
            UpdateStatus::UpToDate { .. }
        ));
        assert!(matches!(
            classify_release(&Version::new(3, 0, 0), &naming(), release("v2.0.0", &[])).unwrap(),
            UpdateStatus::AheadOfLatest { .. }
        ));
    }

    #[test]
    fn missing_or_duplicate_compatible_asset_is_explicit() {
        let expected = "syntra-2.0.0-linux-x86_64.zip";
        assert!(matches!(
            classify_release(
                &Version::new(1, 0, 0),
                &naming(),
                release("v2.0.0", &["other.zip"])
            ),
            Err(UpdateError::UnsupportedAsset { .. })
        ));
        assert!(matches!(
            classify_release(
                &Version::new(1, 0, 0),
                &naming(),
                release("v2.0.0", &[expected, expected])
            ),
            Err(UpdateError::AmbiguousAsset { count: 2, .. })
        ));
    }

    #[test]
    fn compatible_asset_is_selected_exactly() {
        let expected = "syntra-2.0.0-linux-x86_64.zip";
        let status = classify_release(
            &Version::new(1, 0, 0),
            &naming(),
            release("v2.0.0", &[expected, "syntra-2.0.0-linux-aarch64.zip"]),
        )
        .unwrap();
        let UpdateStatus::Available(update) = status else {
            panic!("update expected")
        };
        assert_eq!(update.asset.name, expected);
    }

    #[test]
    fn oversized_stream_removes_partial_file() {
        let directory = std::env::temp_dir().join(format!(
            "syntra-update-test-{}-{}",
            std::process::id(),
            STAGING_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let error =
            stage_reader(Cursor::new(vec![7_u8; 11]), &directory, "asset.zip", 10).unwrap_err();
        assert!(matches!(
            error,
            UpdateError::DownloadTooLarge { limit: 10, .. }
        ));
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
        fs::remove_dir(directory).unwrap();
    }

    struct FailingReader {
        first: bool,
    }

    impl Read for FailingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.first {
                self.first = false;
                buffer[..3].copy_from_slice(b"abc");
                Ok(3)
            } else {
                Err(io::Error::other("injected failure"))
            }
        }
    }

    #[test]
    fn read_failure_removes_partial_file() {
        let directory = std::env::temp_dir().join(format!(
            "syntra-update-test-{}-{}",
            std::process::id(),
            STAGING_NONCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&directory).unwrap();
        let error =
            stage_reader(FailingReader { first: true }, &directory, "asset.zip", 10).unwrap_err();
        assert!(matches!(error, UpdateError::Io(_)));
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 0);
        fs::remove_dir(directory).unwrap();
    }
}
