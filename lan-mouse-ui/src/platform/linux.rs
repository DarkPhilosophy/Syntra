use super::*;
use ksni::blocking::TrayMethods;
use ksni::{Tray, menu};
use std::process::Command;

pub struct LinuxPlatform;
impl LinuxPlatform {
    pub fn new() -> Self {
        Self
    }
}
struct TrayHandle(ksni::blocking::Handle<LanMouseTray>);
impl PlatformTray for TrayHandle {}
impl Drop for TrayHandle {
    fn drop(&mut self) {
        self.0.shutdown();
    }
}
struct LanMouseTray {
    callbacks: PlatformCallbacks,
}
impl Tray for LanMouseTray {
    fn id(&self) -> String {
        "de.feschber.LanMouse".into()
    }
    fn title(&self) -> String {
        "Syntra".into()
    }
    fn icon_name(&self) -> String {
        "syntra".into()
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.callbacks.show();
    }
    fn menu(&self) -> Vec<menu::MenuItem<Self>> {
        vec![
            menu::MenuItem::Standard(menu::StandardItem {
                label: "Open Syntra".into(),
                activate: Box::new(|tray| tray.callbacks.show()),
                ..Default::default()
            }),
            menu::MenuItem::Standard(menu::StandardItem {
                label: "Quit".into(),
                activate: Box::new(|tray| tray.callbacks.quit()),
                ..Default::default()
            }),
        ]
    }
}
impl PlatformActions for LinuxPlatform {
    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities {
            tray: true,
            close_behavior: CloseBehavior::Hide,
            flatpak_permissions: true,
        }
    }
    fn perform(&self, action: PlatformAction) -> Result<(), PlatformError> {
        match action {
            PlatformAction::GrantFlatpakFilesystemAccess { application_id } => {
                if application_id.starts_with('-') || application_id.is_empty() {
                    return Err(PlatformError::Permission(
                        "invalid Flatpak application id".into(),
                    ));
                }
                let status = Command::new("flatpak")
                    .args([
                        "override",
                        "--user",
                        "--filesystem=xdg-run/lan-mouse:ro",
                        &application_id,
                    ])
                    .status()
                    .map_err(|e| PlatformError::Permission(e.to_string()))?;
                if status.success() {
                    Ok(())
                } else {
                    Err(PlatformError::Permission(format!(
                        "flatpak exited with {status}"
                    )))
                }
            }
            PlatformAction::NotifyRunningInBackground => {
                let status = Command::new("notify-send")
                    .args([
                        "--app-name",
                        "Syntra",
                        "--icon",
                        "syntra",
                        "Syntra is running in the background",
                        "Open Syntra again to restore the window.",
                    ])
                    .status()
                    .map_err(|e| PlatformError::Tray(e.to_string()))?;
                if status.success() {
                    Ok(())
                } else {
                    Err(PlatformError::Tray(format!(
                        "notify-send exited with {status}"
                    )))
                }
            }
            _ => Err(PlatformError::UnsupportedAction("unsupported Linux action")),
        }
    }
    fn installed_flatpak_applications(&self) -> Result<Vec<FlatpakApplication>, PlatformError> {
        let output = Command::new("flatpak")
            .args(["list", "--app", "--columns=application,name"])
            .output()
            .map_err(|e| PlatformError::Flatpak(e.to_string()))?;
        if !output.status.success() {
            return Err(PlatformError::Flatpak(format!(
                "flatpak list exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let stdout = String::from_utf8(output.stdout)
            .map_err(|e| PlatformError::Flatpak(format!("invalid flatpak output: {e}")))?;
        stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let mut fields = line.splitn(2, '\t');
                let application_id = fields.next().unwrap_or_default().trim();
                let name = fields.next().unwrap_or_default().trim();
                if application_id.is_empty() {
                    return Err(PlatformError::Flatpak(
                        "flatpak returned an application without an ID".into(),
                    ));
                }
                Ok(FlatpakApplication {
                    application_id: application_id.into(),
                    name: if name.is_empty() {
                        application_id.into()
                    } else {
                        name.into()
                    },
                })
            })
            .collect()
    }
    fn start_tray(
        &self,
        callbacks: PlatformCallbacks,
    ) -> Result<Box<dyn PlatformTray>, PlatformError> {
        let handle = LanMouseTray { callbacks }
            .assume_sni_available(true)
            .spawn()
            .map_err(|e| PlatformError::Tray(e.to_string()))?;
        Ok(Box::new(TrayHandle(handle)))
    }
}
