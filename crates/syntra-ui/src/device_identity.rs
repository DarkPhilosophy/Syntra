use std::fs;
use std::io;
use std::net::IpAddr;
use std::path::PathBuf;

use syntra_api::Position;

#[cfg(not(target_os = "android"))]
pub async fn pick_image() -> io::Result<Option<PathBuf>> {
    Ok(rfd::AsyncFileDialog::new()
        .add_filter("Images", &["png", "jpg", "jpeg", "svg", "webp"])
        .pick_file()
        .await
        .map(|file| file.path().to_path_buf()))
}

#[cfg(target_os = "android")]
pub async fn pick_image() -> io::Result<Option<PathBuf>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "image picker unavailable on Android",
    ))
}
use crate::settings::{
    DevicePresentation, PresentationSettings, identity_images_path, presentation_settings_path,
};

/// Deterministic presentation key for a configured peer. Never a runtime handle.
pub fn client_identity_key(
    hostname: Option<&str>,
    fixed_ips: &[IpAddr],
    _port: u16,
    _position: Position,
) -> String {
    let host = hostname.unwrap_or_default().trim().to_ascii_lowercase();
    if !host.is_empty() {
        return format!("cfg:host:{host}");
    }
    let mut ips: Vec<String> = fixed_ips.iter().map(ToString::to_string).collect();
    ips.sort();
    if ips.is_empty() {
        String::new()
    } else {
        format!("cfg:ip:{}", ips.join(","))
    }
}
#[derive(Clone, Debug)]
pub struct IdentityStore {
    settings: PresentationSettings,
    settings_path: Option<PathBuf>,
    images_path: Option<PathBuf>,
}

impl IdentityStore {
    pub fn new(settings: PresentationSettings) -> Self {
        Self {
            settings,
            settings_path: presentation_settings_path(),
            images_path: identity_images_path(),
        }
    }

    #[cfg(test)]
    fn with_paths(
        settings: PresentationSettings,
        settings_path: PathBuf,
        images_path: PathBuf,
    ) -> Self {
        Self {
            settings,
            settings_path: Some(settings_path),
            images_path: Some(images_path),
        }
    }

    pub fn settings(&self) -> &PresentationSettings {
        &self.settings
    }
    pub fn local(&self) -> &DevicePresentation {
        &self.settings.local_device
    }
    pub fn peer(&self, key: &str) -> Option<&DevicePresentation> {
        self.settings.devices.get(key)
    }

    pub fn set_local_name(&mut self, name: impl Into<String>) -> io::Result<()> {
        let mut next = self.settings.clone();
        next.local_device.friendly_name = name.into();
        self.commit(next)
    }

    pub fn set_peer_name(
        &mut self,
        key: impl Into<String>,
        name: impl Into<String>,
    ) -> io::Result<()> {
        let key = checked_peer_key(key.into())?;
        let mut next = self.settings.clone();
        next.devices.entry(key).or_default().friendly_name = name.into();
        self.commit(next)
    }

    pub fn set_local_image(&mut self, source: PathBuf) -> io::Result<()> {
        let owned = self.copy_image(source)?;
        let previous = self.settings.local_device.image_path.clone();
        let mut next = self.settings.clone();
        next.local_device.image_path = owned.to_string_lossy().into_owned();
        if let Err(error) = self.commit(next) {
            let _ = remove_owned(
                owned.to_string_lossy().as_ref(),
                self.images_path.as_deref(),
            );
            return Err(error);
        }
        self.cleanup_previous(&previous);
        Ok(())
    }

    pub fn set_peer_image(&mut self, key: impl Into<String>, source: PathBuf) -> io::Result<()> {
        let key = checked_peer_key(key.into())?;
        let owned = self.copy_image(source)?;
        let previous = self
            .settings
            .devices
            .get(&key)
            .map(|item| item.image_path.clone())
            .unwrap_or_default();
        let mut next = self.settings.clone();
        next.devices.entry(key).or_default().image_path = owned.to_string_lossy().into_owned();
        if let Err(error) = self.commit(next) {
            let _ = remove_owned(
                owned.to_string_lossy().as_ref(),
                self.images_path.as_deref(),
            );
            return Err(error);
        }
        self.cleanup_previous(&previous);
        Ok(())
    }

    pub fn migrate_key(&mut self, old: &str, new: &str) -> io::Result<()> {
        if old.is_empty() || new.is_empty() || old == new {
            return Ok(());
        }
        let mut next = self.settings.clone();
        if let Some(value) = next.devices.remove(old) {
            next.devices.entry(new.to_string()).or_insert(value);
            self.commit(next)?;
        }
        Ok(())
    }

    pub fn clear_local_image(&mut self) -> io::Result<()> {
        let previous = self.settings.local_device.image_path.clone();
        let mut next = self.settings.clone();
        next.local_device.image_path.clear();
        self.commit(next)?;
        self.cleanup_previous(&previous);
        Ok(())
    }

    pub fn clear_peer_image(&mut self, key: &str) -> io::Result<()> {
        let key = checked_peer_key(key.to_string())?;
        let previous = self
            .settings
            .devices
            .get(&key)
            .map(|item| item.image_path.clone())
            .unwrap_or_default();
        let mut next = self.settings.clone();
        if let Some(item) = next.devices.get_mut(&key) {
            item.image_path.clear();
        }
        self.commit(next)?;
        self.cleanup_previous(&previous);
        Ok(())
    }

    fn copy_image(&self, source: PathBuf) -> io::Result<PathBuf> {
        let dir = self.images_path.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "config directory unavailable")
        })?;
        fs::create_dir_all(dir)?;
        let ext = source.extension().and_then(|x| x.to_str()).unwrap_or("img");
        let target = dir.join(format!("identity-{}.{ext}", unique_suffix()));
        fs::copy(source, &target)?;
        Ok(target)
    }

    fn commit(&mut self, next: PresentationSettings) -> io::Result<()> {
        let path = self.settings_path.as_ref().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "config directory unavailable")
        })?;
        next.save(path)?;
        self.settings = next;
        Ok(())
    }

    fn cleanup_previous(&self, previous: &str) {
        if let Err(error) = remove_owned(previous, self.images_path.as_deref()) {
            log::warn!(
                "identity settings were saved, but the previous image could not be removed: {error}"
            );
        }
    }
}

fn checked_peer_key(key: String) -> io::Result<String> {
    if key.trim().is_empty() {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "peer has no stable identity",
        ))
    } else {
        Ok(key)
    }
}

fn unique_suffix() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

fn remove_owned(path: &str, images_path: Option<&std::path::Path>) -> io::Result<()> {
    if path.is_empty() {
        return Ok(());
    }
    let Some(root) = images_path else {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "config directory unavailable",
        ));
    };
    let root = match root.canonicalize() {
        Ok(root) => root,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let candidate = match PathBuf::from(path).canonicalize() {
        Ok(candidate) => candidate,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if candidate.starts_with(root) {
        fs::remove_file(candidate)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestPaths {
        root: PathBuf,
        settings: PathBuf,
        images: PathBuf,
    }

    impl TestPaths {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("lan-mouse-identity-{label}-{}", unique_suffix()));
            let settings = root.join("presentation.json");
            let images = root.join("device-images");
            fs::create_dir_all(&images).unwrap();
            Self {
                root,
                settings,
                images,
            }
        }

        fn store(&self, settings: PresentationSettings) -> IdentityStore {
            IdentityStore::with_paths(settings, self.settings.clone(), self.images.clone())
        }

        fn old_image(&self, name: &str) -> PathBuf {
            let path = self.images.join(name);
            fs::write(&path, b"old").unwrap();
            path
        }

        fn source(&self) -> PathBuf {
            let path = self.root.join("source.png");
            fs::write(&path, b"new").unwrap();
            path
        }
    }

    impl Drop for TestPaths {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn replacing_an_image_commits_new_path_before_removing_previous_file() {
        let paths = TestPaths::new("replace");
        let previous = paths.old_image("old.png");
        let mut settings = PresentationSettings::default();
        settings.local_device.image_path = previous.to_string_lossy().into_owned();
        let mut store = paths.store(settings);

        store.set_local_image(paths.source()).unwrap();

        let saved = PresentationSettings::load(&paths.settings).unwrap();
        let replacement = PathBuf::from(&saved.local_device.image_path);
        assert_eq!(store.settings(), &saved);
        assert_eq!(fs::read(replacement).unwrap(), b"new");
        assert!(!previous.exists());
    }

    #[test]
    fn clearing_an_image_commits_empty_path_and_removes_previous_file() {
        let paths = TestPaths::new("clear");
        let previous = paths.old_image("old.png");
        let mut settings = PresentationSettings::default();
        settings.local_device.image_path = previous.to_string_lossy().into_owned();
        let mut store = paths.store(settings);

        store.clear_local_image().unwrap();

        let saved = PresentationSettings::load(&paths.settings).unwrap();
        assert!(saved.local_device.image_path.is_empty());
        assert_eq!(store.settings(), &saved);
        assert!(!previous.exists());
    }

    #[test]
    fn failed_commit_preserves_previous_settings_and_image() {
        let paths = TestPaths::new("rollback");
        let previous = paths.old_image("old.png");
        let mut settings = PresentationSettings::default();
        settings.local_device.image_path = previous.to_string_lossy().into_owned();
        settings.save(&paths.settings).unwrap();
        fs::create_dir(paths.settings.with_extension("tmp")).unwrap();
        let mut store = paths.store(settings.clone());

        assert!(store.set_local_image(paths.source()).is_err());

        assert_eq!(store.settings(), &settings);
        assert_eq!(
            PresentationSettings::load(&paths.settings).unwrap(),
            settings
        );
        assert_eq!(fs::read(previous).unwrap(), b"old");
        assert_eq!(fs::read_dir(&paths.images).unwrap().count(), 1);
    }

    #[test]
    fn cleanup_failure_after_commit_does_not_report_persistence_failure() {
        let paths = TestPaths::new("cleanup-failure");
        let previous = paths.images.join("old-image");
        fs::create_dir(&previous).unwrap();
        let mut settings = PresentationSettings::default();
        settings.local_device.image_path = previous.to_string_lossy().into_owned();
        let mut store = paths.store(settings);

        store.clear_local_image().unwrap();

        assert!(store.settings().local_device.image_path.is_empty());
        assert!(
            PresentationSettings::load(&paths.settings)
                .unwrap()
                .local_device
                .image_path
                .is_empty()
        );
        assert!(previous.is_dir());
    }
}
