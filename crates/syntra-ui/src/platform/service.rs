use std::fmt;
use std::path::Path;

const UNIT_NAME: &str = "lan-mouse.service";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceStatus {
    pub installed: bool,
    pub running: bool,
    pub autostart: bool,
}

#[derive(Debug)]
pub enum ServiceError {
    Unsupported(&'static str),
    InvalidBinary(String),
    Environment(String),
    Io(std::io::Error),
    ManagerUnavailable(String),
    CommandFailed {
        operation: &'static str,
        status: Option<i32>,
        message: String,
    },
    InvalidResponse(String),
}

impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(message) => f.write_str(message),
            Self::InvalidBinary(message) => write!(f, "invalid service binary: {message}"),
            Self::Environment(message) => {
                write!(f, "cannot locate user systemd configuration: {message}")
            }
            Self::Io(error) => write!(f, "service filesystem operation failed: {error}"),
            Self::ManagerUnavailable(message) => {
                write!(f, "systemd user manager is unavailable: {message}")
            }
            Self::CommandFailed {
                operation,
                status,
                message,
            } => write!(
                f,
                "systemctl --user {operation} failed (status {}): {message}",
                status.map_or_else(|| "unknown".into(), |status| status.to_string())
            ),
            Self::InvalidResponse(message) => {
                write!(f, "systemd returned an invalid service status: {message}")
            }
        }
    }
}

impl std::error::Error for ServiceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for ServiceError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

pub fn query() -> Result<ServiceStatus, ServiceError> {
    imp::query()
}

pub fn install(binary: &Path) -> Result<(), ServiceError> {
    imp::install(binary)
}

pub fn uninstall() -> Result<(), ServiceError> {
    imp::uninstall()
}

pub fn start() -> Result<(), ServiceError> {
    imp::run_action("start")
}

pub fn stop() -> Result<(), ServiceError> {
    imp::run_action("stop")
}

pub fn enable_autostart() -> Result<(), ServiceError> {
    imp::run_action("enable")
}

pub fn disable_autostart() -> Result<(), ServiceError> {
    imp::run_action("disable")
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{ServiceError, ServiceStatus, UNIT_NAME};
    use std::collections::HashMap;
    use std::env;
    use std::ffi::OsStr;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};
    use std::path::{Component, Path, PathBuf};
    use std::process::{Command, Output};

    pub(super) fn query() -> Result<ServiceStatus, ServiceError> {
        let output = Command::new("systemctl")
            .args([
                "--user",
                "show",
                UNIT_NAME,
                "--property=LoadState",
                "--property=ActiveState",
                "--property=UnitFileState",
                "--no-pager",
            ])
            .output()
            .map_err(|error| command_start_error("show", error))?;
        ensure_success("show", &output)?;
        parse_status(&String::from_utf8(output.stdout).map_err(|error| {
            ServiceError::InvalidResponse(format!("status was not UTF-8: {error}"))
        })?)
    }

    pub(super) fn install(binary: &Path) -> Result<(), ServiceError> {
        let appimage = env::var_os("APPIMAGE");
        let binary = resolve_install_executable(binary, appimage.as_deref())?;
        let unit_path = unit_path()?;
        let parent = unit_path
            .parent()
            .ok_or_else(|| ServiceError::Environment("unit path has no parent directory".into()))?;
        fs::create_dir_all(parent)?;
        let escaped_binary = escape_exec_start_path(&binary)?;
        let contents = format!(
            "[Unit]\nDescription=Syntra background service\nAfter=graphical-session.target\nBindsTo=graphical-session.target\n\n[Service]\nExecStart={escaped_binary} daemon\nRestart=on-failure\nKillSignal=SIGINT\nTimeoutStopSec=15\n\n[Install]\nWantedBy=graphical-session.target\n"
        );
        write_unit_atomically(&unit_path, contents.as_bytes())?;
        run_manager_action("daemon-reload")
    }

    pub(super) fn uninstall() -> Result<(), ServiceError> {
        run_action("disable")?;
        match fs::remove_file(unit_path()?) {
            Ok(()) => run_manager_action("daemon-reload"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                run_manager_action("daemon-reload")
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn run_action(action: &'static str) -> Result<(), ServiceError> {
        let output = Command::new("systemctl")
            .args(["--user", action, UNIT_NAME])
            .output()
            .map_err(|error| command_start_error(action, error))?;
        ensure_success(action, &output)
    }

    fn run_manager_action(action: &'static str) -> Result<(), ServiceError> {
        let output = Command::new("systemctl")
            .args(["--user", action])
            .output()
            .map_err(|error| command_start_error(action, error))?;
        ensure_success(action, &output)
    }

    fn resolve_install_executable(
        binary: &Path,
        appimage: Option<&OsStr>,
    ) -> Result<PathBuf, ServiceError> {
        if !is_ephemeral_appimage_mount(binary) {
            return validate_binary(binary);
        }

        let original = appimage.filter(|path| !path.is_empty()).ok_or_else(|| {
            ServiceError::InvalidBinary(
                "the application is running from a temporary AppImage mount, but APPIMAGE does \
                 not identify the original file; relaunch the AppImage and install the service \
                 again"
                    .into(),
            )
        })?;
        let original = Path::new(original);
        if is_ephemeral_appimage_mount(original) {
            return Err(ServiceError::InvalidBinary(
                "APPIMAGE points into a temporary .mount_ directory; relaunch the original \
                 AppImage file and install the service again"
                    .into(),
            ));
        }

        let original = validate_binary(original).map_err(|error| match error {
            ServiceError::InvalidBinary(message) => ServiceError::InvalidBinary(format!(
                "APPIMAGE does not identify a usable original AppImage ({message}); relaunch the \
                 original file and install the service again"
            )),
            error => error,
        })?;
        let original = original.canonicalize().map_err(|error| {
            ServiceError::InvalidBinary(format!(
                "cannot resolve original AppImage {}: {error}",
                original.display()
            ))
        })?;
        if is_ephemeral_appimage_mount(&original) {
            return Err(ServiceError::InvalidBinary(
                "the original AppImage resolves into a temporary .mount_ directory; move it to a \
                 stable location, relaunch it, and install the service again"
                    .into(),
            ));
        }
        Ok(original)
    }

    fn is_ephemeral_appimage_mount(path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix("/tmp") else {
            return false;
        };
        matches!(
            relative.components().next(),
            Some(Component::Normal(directory))
                if directory.as_bytes().starts_with(b".mount_")
        )
    }

    fn validate_binary(binary: &Path) -> Result<PathBuf, ServiceError> {
        let file_name = binary
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if file_name == "live_preview"
            || file_name == "launch_smoke"
            || file_name.contains("preview")
        {
            return Err(ServiceError::InvalidBinary(
                "preview/example binaries cannot own the production service".into(),
            ));
        }
        if !binary.is_absolute() {
            return Err(ServiceError::InvalidBinary("path must be absolute".into()));
        }
        let metadata = fs::metadata(binary).map_err(|error| {
            ServiceError::InvalidBinary(format!("{}: {error}", binary.display()))
        })?;
        if !metadata.is_file() {
            return Err(ServiceError::InvalidBinary(format!(
                "{} is not a regular file",
                binary.display()
            )));
        }
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(ServiceError::InvalidBinary(format!(
                "{} is not executable",
                binary.display()
            )));
        }
        Ok(binary.to_path_buf())
    }

    fn unit_path() -> Result<PathBuf, ServiceError> {
        let config_home = match env::var_os("XDG_CONFIG_HOME") {
            Some(path) if !path.is_empty() => PathBuf::from(path),
            _ => env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .map(PathBuf::from)
                .map(|home| home.join(".config"))
                .ok_or_else(|| {
                    ServiceError::Environment("neither XDG_CONFIG_HOME nor HOME is set".into())
                })?,
        };
        Ok(config_home.join("systemd/user").join(UNIT_NAME))
    }

    fn write_unit_atomically(path: &Path, contents: &[u8]) -> Result<(), ServiceError> {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| ServiceError::Environment("unit filename is not valid UTF-8".into()))?;
        let temporary = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            file.write_all(contents)?;
            file.sync_all()?;
            fs::rename(&temporary, path)?;
            Ok::<(), std::io::Error>(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result.map_err(ServiceError::Io)
    }

    fn escape_exec_start_path(path: &Path) -> Result<String, ServiceError> {
        let value = path
            .to_str()
            .ok_or_else(|| ServiceError::InvalidBinary("path is not valid UTF-8".into()))?;
        if value.chars().any(|character| character.is_control()) {
            return Err(ServiceError::InvalidBinary(
                "path contains a control character".into(),
            ));
        }
        let mut escaped = String::with_capacity(value.len() + 2);
        escaped.push('"');
        for character in value.chars() {
            match character {
                '\\' => escaped.push_str("\\\\"),
                '"' => escaped.push_str("\\\""),
                '%' => escaped.push_str("%%"),
                _ => escaped.push(character),
            }
        }
        escaped.push('"');
        Ok(escaped)
    }

    fn parse_status(stdout: &str) -> Result<ServiceStatus, ServiceError> {
        let properties: HashMap<_, _> = stdout
            .lines()
            .filter_map(|line| line.split_once('='))
            .collect();
        let load = required_property(&properties, "LoadState")?;

        if load == "not-found" {
            return Ok(ServiceStatus {
                installed: false,
                running: false,
                autostart: false,
            });
        }
        let active = required_property(&properties, "ActiveState")?;
        let unit_file = required_property(&properties, "UnitFileState")?;
        if matches!(load, "error" | "bad-setting") {
            return Err(ServiceError::InvalidResponse(format!("LoadState={load}")));
        }
        Ok(ServiceStatus {
            installed: true,
            running: matches!(active, "active" | "reloading"),
            autostart: matches!(unit_file, "enabled" | "enabled-runtime"),
        })
    }

    fn required_property<'a>(
        properties: &'a HashMap<&str, &str>,
        name: &str,
    ) -> Result<&'a str, ServiceError> {
        properties
            .get(name)
            .copied()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ServiceError::InvalidResponse(format!("missing {name}")))
    }

    fn command_start_error(operation: &'static str, error: std::io::Error) -> ServiceError {
        if error.kind() == std::io::ErrorKind::NotFound {
            ServiceError::ManagerUnavailable("systemctl was not found".into())
        } else {
            ServiceError::CommandFailed {
                operation,
                status: None,
                message: error.to_string(),
            }
        }
    }

    fn ensure_success(operation: &'static str, output: &Output) -> Result<(), ServiceError> {
        if output.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let message = if stderr.is_empty() {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        } else {
            stderr
        };
        if manager_unavailable(&message) {
            Err(ServiceError::ManagerUnavailable(message))
        } else {
            Err(ServiceError::CommandFailed {
                operation,
                status: output.status.code(),
                message,
            })
        }
    }

    fn manager_unavailable(message: &str) -> bool {
        let message = message.to_ascii_lowercase();
        message.contains("failed to connect to bus")
            || message.contains("user manager") && message.contains("not running")
            || message.contains("no medium found")
    }

    #[cfg(test)]
    mod tests {
        use super::{escape_exec_start_path, parse_status, resolve_install_executable};
        use std::path::Path;

        #[test]
        fn parses_independent_service_dimensions() {
            let status =
                parse_status("LoadState=loaded\nActiveState=inactive\nUnitFileState=enabled\n")
                    .unwrap();
            assert!(status.installed);
            assert!(!status.running);
            assert!(status.autostart);
        }

        #[test]
        fn missing_unit_is_not_a_manager_failure() {
            let status =
                parse_status("LoadState=not-found\nActiveState=inactive\nUnitFileState=\n")
                    .unwrap();
            assert!(!status.installed);
            assert!(!status.running);
            assert!(!status.autostart);
        }

        #[test]
        fn escapes_systemd_exec_start_metacharacters() {
            let escaped =
                escape_exec_start_path(Path::new("/opt/Lan Mouse/100%/say\"hi\\now")).unwrap();
            assert_eq!(escaped, "\"/opt/Lan Mouse/100%%/say\\\"hi\\\\now\"");
        }

        #[test]
        fn appimage_service_uses_stable_original_executable() {
            let original = std::env::current_exe().unwrap();
            let resolved = resolve_install_executable(
                Path::new("/tmp/.mount_Syntra123/usr/bin/syntra"),
                Some(original.as_os_str()),
            )
            .unwrap();

            assert_eq!(resolved, original.canonicalize().unwrap());
        }

        #[test]
        fn appimage_service_rejects_unresolved_temporary_executable() {
            let mounted = Path::new("/tmp/.mount_Syntra123/usr/bin/syntra");

            let missing = resolve_install_executable(mounted, None).unwrap_err();
            assert!(missing.to_string().contains("APPIMAGE"));

            let still_mounted =
                resolve_install_executable(mounted, Some(mounted.as_os_str())).unwrap_err();
            assert!(
                still_mounted
                    .to_string()
                    .contains("temporary .mount_ directory")
            );
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::{ServiceError, ServiceStatus};
    use std::path::Path;

    const UNSUPPORTED: &str = "user systemd service management is supported only on Linux";

    pub(super) fn query() -> Result<ServiceStatus, ServiceError> {
        Err(ServiceError::Unsupported(UNSUPPORTED))
    }

    pub(super) fn install(_binary: &Path) -> Result<(), ServiceError> {
        Err(ServiceError::Unsupported(UNSUPPORTED))
    }

    pub(super) fn uninstall() -> Result<(), ServiceError> {
        Err(ServiceError::Unsupported(UNSUPPORTED))
    }

    pub(super) fn run_action(_action: &'static str) -> Result<(), ServiceError> {
        Err(ServiceError::Unsupported(UNSUPPORTED))
    }
}
