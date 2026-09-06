use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BaseMode {
    White,
    Black,
}

impl Default for BaseMode {
    fn default() -> Self {
        Self::Black
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Palette {
    Army,
    Forest,
    Ocean,
    Slate,
    Sand,
    Graphite,
    Aubergine,
    Copper,
    Rose,
    Ice,
}

impl Default for Palette {
    fn default() -> Self {
        Self::Army
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Density {
    Comfortable,
    Compact,
}

impl Default for Density {
    fn default() -> Self {
        Self::Comfortable
    }
}

use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DevicePresentation {
    #[serde(default)]
    pub friendly_name: String,
    #[serde(default)]
    pub image_path: String,
}

fn default_update_artifact_template() -> String {
    "syntra-{version}-{os}-{arch}.tar.gz".into()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdatePreferences {
    pub repository: String,
    pub artifact_template: String,
    pub automatic_check: bool,
}

impl Default for UpdatePreferences {
    fn default() -> Self {
        Self {
            repository: String::new(),
            artifact_template: default_update_artifact_template(),
            automatic_check: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PresentationSettings {
    pub locale: String,
    pub mode: BaseMode,
    pub palette: Palette,
    pub accent: String,
    pub titlebar_color: String,
    pub density: Density,
    pub zoom_percent: u16,
    pub reduced_motion: bool,
    pub sidebar_open: bool,
    pub local_device: DevicePresentation,
    pub devices: BTreeMap<String, DevicePresentation>,
    pub updates: UpdatePreferences,
}

#[derive(Deserialize)]
struct PresentationSettingsWire {
    #[serde(default = "default_locale")]
    locale: String,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default = "default_palette_name")]
    palette: String,
    #[serde(default = "default_accent")]
    accent: String,
    #[serde(default = "default_titlebar_color")]
    titlebar_color: String,
    #[serde(default = "default_density_name")]
    density: String,
    #[serde(default = "default_zoom_percent")]
    zoom_percent: u16,
    #[serde(default)]
    reduced_motion: bool,
    #[serde(default = "default_sidebar")]
    sidebar_open: bool,
    #[serde(default)]
    local_device: DevicePresentation,
    #[serde(default)]
    devices: BTreeMap<String, DevicePresentation>,
    #[serde(default)]
    updates: UpdatePreferences,
    #[serde(default)]
    theme: Option<String>,
}

fn default_zoom_percent() -> u16 {
    100
}

fn default_palette_name() -> String {
    "army".into()
}
fn default_density_name() -> String {
    "comfortable".into()
}
fn parse_palette(value: &str) -> Palette {
    match value.to_ascii_lowercase().as_str() {
        "forest" => Palette::Forest,
        "ocean" => Palette::Ocean,
        "slate" => Palette::Slate,
        "sand" => Palette::Sand,
        "graphite" => Palette::Graphite,
        "aubergine" => Palette::Aubergine,
        "copper" => Palette::Copper,
        "rose" => Palette::Rose,
        "ice" => Palette::Ice,
        _ => Palette::Army,
    }
}
fn parse_density(value: &str) -> Density {
    if value.eq_ignore_ascii_case("compact") {
        Density::Compact
    } else {
        Density::Comfortable
    }
}
fn parse_mode(value: Option<&str>, legacy: Option<&str>) -> BaseMode {
    match value
        .or(legacy)
        .unwrap_or("black")
        .to_ascii_lowercase()
        .as_str()
    {
        "white" | "light" => BaseMode::White,
        _ => BaseMode::Black,
    }
}
impl<'de> Deserialize<'de> for PresentationSettings {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = PresentationSettingsWire::deserialize(deserializer)?;
        Ok(Self {
            locale: wire.locale,
            mode: parse_mode(wire.mode.as_deref(), wire.theme.as_deref()),
            palette: parse_palette(&wire.palette),
            accent: normalize_color(wire.accent, default_accent()),
            titlebar_color: normalize_color(wire.titlebar_color, default_titlebar_color()),
            density: parse_density(&wire.density),
            zoom_percent: wire.zoom_percent.clamp(75, 200),
            reduced_motion: wire.reduced_motion,
            sidebar_open: wire.sidebar_open,
            local_device: wire.local_device,
            devices: wire.devices,
            updates: wire.updates,
        })
    }
}

fn default_locale() -> String {
    "en-US".into()
}
fn default_accent() -> String {
    "#718355".into()
}
fn default_titlebar_color() -> String {
    String::new()
}
fn default_sidebar() -> bool {
    true
}

fn normalize_color(value: String, fallback: String) -> String {
    if value.is_empty() {
        return String::new();
    }
    let bytes = value.as_bytes();
    if bytes.len() == 7 && bytes[0] == b'#' && bytes[1..].iter().all(u8::is_ascii_hexdigit) {
        value.to_ascii_lowercase()
    } else {
        fallback
    }
}

impl Default for PresentationSettings {
    fn default() -> Self {
        Self {
            locale: default_locale(),
            mode: BaseMode::Black,
            palette: Palette::Army,
            accent: default_accent(),
            titlebar_color: default_titlebar_color(),
            density: Density::Comfortable,
            zoom_percent: default_zoom_percent(),
            reduced_motion: false,
            sidebar_open: true,
            local_device: DevicePresentation::default(),
            devices: BTreeMap::new(),
            updates: UpdatePreferences::default(),
        }
    }
}
pub fn presentation_settings_path() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA").map(PathBuf::from)?;
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join("Library").join("Application Support"))?;
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    return Some(base.join("LanMouse").join("presentation.json"));
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    Some(base.join("lan-mouse").join("presentation.json"))
}

pub fn identity_images_path() -> Option<PathBuf> {
    presentation_settings_path()?
        .parent()
        .map(|parent| parent.join("device-images"))
}

impl PresentationSettings {
    pub fn load(path: &Path) -> io::Result<Self> {
        match fs::read_to_string(path) {
            Ok(content) => serde_json::from_str(&content)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("tmp");
        let content = serde_json::to_vec_pretty(self)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        fs::write(&temporary, content)?;
        fs::rename(temporary, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_presentation_settings_use_backward_compatible_defaults() {
        let settings: PresentationSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings, PresentationSettings::default());
    }

    #[test]
    fn legacy_light_theme_migrates_to_white_mode() {
        let settings: PresentationSettings =
            serde_json::from_str(r#"{"locale":"ro-RO","theme":"Light","density":"Compact"}"#)
                .unwrap();
        assert_eq!(settings.locale, "ro-RO");
        assert_eq!(settings.mode, BaseMode::White);
        assert_eq!(settings.palette, Palette::Army);
        assert_eq!(settings.density, Density::Compact);
    }

    #[test]
    fn old_settings_retain_preferences_when_updates_are_missing() {
        let settings: PresentationSettings = serde_json::from_str(
            r##"{
                "locale":"ro-RO",
                "mode":"white",
                "palette":"ocean",
                "accent":"#123abc",
                "titlebar_color":"#abcdef",
                "density":"compact",
                "zoom_percent":125,
                "reduced_motion":true,
                "sidebar_open":false,
                "local_device":{"friendly_name":"This device","image_path":"/local.png"},
                "devices":{"peer":{"friendly_name":"Peer","image_path":"/peer.png"}}
            }"##,
        )
        .unwrap();

        assert_eq!(settings.locale, "ro-RO");
        assert_eq!(settings.mode, BaseMode::White);
        assert_eq!(settings.palette, Palette::Ocean);
        assert_eq!(settings.accent, "#123abc");
        assert_eq!(settings.titlebar_color, "#abcdef");
        assert_eq!(settings.density, Density::Compact);
        assert_eq!(settings.zoom_percent, 125);
        assert!(settings.reduced_motion);
        assert!(!settings.sidebar_open);
        assert_eq!(settings.local_device.friendly_name, "This device");
        assert_eq!(settings.devices["peer"].friendly_name, "Peer");
        assert_eq!(settings.updates, UpdatePreferences::default());
        assert!(settings.updates.repository.is_empty());
        assert!(!settings.updates.automatic_check);
    }

    #[test]
    fn configured_updates_roundtrip() {
        let partially_configured: PresentationSettings =
            serde_json::from_str(r#"{"updates":{"repository":"syntra-app/syntra"}}"#).unwrap();
        assert_eq!(
            partially_configured.updates.artifact_template,
            "syntra-{version}-{os}-{arch}.tar.gz"
        );
        assert!(!partially_configured.updates.automatic_check);

        let settings: PresentationSettings = serde_json::from_str(
            r#"{
                "updates":{
                    "repository":"syntra-app/syntra",
                    "artifact_template":"syntra-{version}-{os}-{arch}.zip",
                    "automatic_check":true
                }
            }"#,
        )
        .unwrap();

        assert_eq!(settings.updates.repository, "syntra-app/syntra");
        assert_eq!(
            settings.updates.artifact_template,
            "syntra-{version}-{os}-{arch}.zip"
        );
        assert!(settings.updates.automatic_check);

        let restored: PresentationSettings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(restored, settings);
    }

    #[test]
    fn zoom_is_bounded_without_discarding_other_preferences() {
        let settings: PresentationSettings =
            serde_json::from_str(r#"{"locale":"ro-RO","zoom_percent":250}"#).unwrap();
        assert_eq!(settings.zoom_percent, 200);
        assert_eq!(settings.locale, "ro-RO");
        let restored: PresentationSettings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(restored, settings);
    }
}
