//! The plugin's own settings: what it lets the user change, the values in
//! force and where they are kept.
//!
//! Syntra draws the controls and passes the user's changes on, but the plugin
//! decides what is valid and keeps the values itself, in its own file. A first
//! run adopts what the daemon stored for it before plugins kept their own.

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use syntra_plugin_api::{Message, SettingControl, SettingControlKind, SettingOption};

use crate::{
    glow,
    state::{DEFAULT_WIDTH, Settings, WIDTH_RANGE},
};

/// Every setting: key, label, control and default, in display order.
fn declared() -> Vec<(&'static str, &'static str, SettingControlKind, String)> {
    let option = |value: &str, label: &str| SettingOption {
        value: value.to_owned(),
        label: label.to_owned(),
    };
    vec![
        // No on/off here: switching the plugin off in Syntra stops it.
        (
            "show_pressure",
            "Glow while pushing against an edge",
            SettingControlKind::Toggle,
            "true".into(),
        ),
        (
            "rainbow",
            "Rainbow colours",
            SettingControlKind::Toggle,
            "false".into(),
        ),
        (
            "colour",
            "Colour",
            SettingControlKind::Color,
            "#33ccff".into(),
        ),
        (
            "width",
            "Width in pixels",
            SettingControlKind::Number {
                min: i64::from(WIDTH_RANGE.0),
                max: i64::from(WIDTH_RANGE.1),
            },
            DEFAULT_WIDTH.to_string(),
        ),
        (
            "renderer",
            "Drawing",
            SettingControlKind::Choice {
                options: vec![
                    option("auto", "Automatic (GPU, else CPU)"),
                    option("opengl", "OpenGL (GPU)"),
                    option("software", "Software (CPU)"),
                ],
            },
            "auto".into(),
        ),
    ]
}

/// Whether `value` is one this plugin understands for `key`.
fn accepts(key: &str, value: &str) -> bool {
    match key {
        "enabled" | "show_pressure" | "rainbow" => value == "true" || value == "false",
        "colour" => glow::parse_hex(value).is_some(),
        "width" => value
            .parse::<u32>()
            .is_ok_and(|width| (WIDTH_RANGE.0..=WIDTH_RANGE.1).contains(&width)),
        "renderer" => matches!(value, "auto" | "opengl" | "software"),
        _ => false,
    }
}

/// The settings this plugin owns.
#[derive(Debug)]
pub struct Owned {
    values: BTreeMap<String, String>,
    path: Option<PathBuf>,
    /// The values came from this plugin's own file, not the daemon's.
    kept: bool,
}

impl Owned {
    /// Reads the plugin's file at `path`, if there is one.
    pub fn load(path: Option<PathBuf>) -> Self {
        let mut owned = Self {
            values: BTreeMap::new(),
            path,
            kept: false,
        };
        if let Some(text) = owned
            .path
            .as_deref()
            .and_then(|path| fs::read_to_string(path).ok())
        {
            for line in text.lines() {
                if let Some((key, value)) = line.split_once('=') {
                    if accepts(key, value) {
                        owned.values.insert(key.to_owned(), value.to_owned());
                    }
                }
            }
            owned.kept = true;
        }
        owned
    }

    /// Where the plugin keeps its file by default.
    pub fn default_path() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))?;
        Some(base.join("syntra/plugins/edge-glow/settings.conf"))
    }

    /// The values in force, as the glow applies them.
    pub fn settings(&self) -> Settings {
        let mut settings = Settings::default();
        settings.apply(&self.values());
        settings
    }

    /// Every chosen value, as the glow state takes them.
    pub fn values(&self) -> Vec<(String, String)> {
        self.values
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }

    /// Takes the values the daemon kept for this plugin, once, when the
    /// plugin has no file of its own yet. Returns whether anything changed.
    pub fn adopt(&mut self, values: &[(String, String)]) -> bool {
        if self.kept {
            return false;
        }
        for (key, value) in values {
            if accepts(key, value) {
                self.values.insert(key.clone(), value.clone());
            }
        }
        self.kept = true;
        self.save();
        true
    }

    /// Applies the user's change when the value is valid. Returns whether it
    /// was taken; either way the plugin answers with what is in force.
    pub fn change(&mut self, key: &str, value: &str) -> bool {
        if !accepts(key, value) {
            return false;
        }
        self.values.insert(key.to_owned(), value.to_owned());
        self.kept = true;
        self.save();
        true
    }

    /// What Syntra is told to draw.
    pub fn declaration(&self) -> Message {
        Message::DeclareSettings {
            settings: declared()
                .into_iter()
                .map(|(key, label, kind, default)| SettingControl {
                    key: key.to_owned(),
                    label: label.to_owned(),
                    kind,
                    value: self
                        .values
                        .get(key)
                        .cloned()
                        .unwrap_or_else(|| default.clone()),
                    default,
                })
                .collect(),
        }
    }

    fn save(&self) {
        let Some(path) = &self.path else { return };
        let text: String = self
            .values
            .iter()
            .map(|(key, value)| format!("{key}={value}\n"))
            .collect();
        let write = || -> io::Result<()> {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let staged = path.with_extension("conf.new");
            fs::write(&staged, text)?;
            fs::rename(&staged, path)
        };
        if let Err(error) = write() {
            eprintln!("edge-glow: cannot keep settings: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("edge-glow-owned-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir.join("settings.conf")
    }

    fn value_of(owned: &Owned, key: &str) -> String {
        let Message::DeclareSettings { settings } = owned.declaration() else {
            panic!("not a declaration")
        };
        settings.into_iter().find(|s| s.key == key).unwrap().value
    }

    #[test]
    fn a_first_run_adopts_the_daemons_values_and_then_keeps_its_own() {
        let path = temp("adopt");
        let mut owned = Owned::load(Some(path.clone()));
        assert!(owned.adopt(&[
            ("width".into(), "90".into()),
            ("colour".into(), "nope".into())
        ]));
        assert_eq!(value_of(&owned, "width"), "90");
        assert_eq!(
            value_of(&owned, "colour"),
            "#33ccff",
            "invalid values are not taken"
        );

        let mut again = Owned::load(Some(path));
        assert_eq!(
            value_of(&again, "width"),
            "90",
            "kept in the plugin's own file"
        );
        assert!(
            !again.adopt(&[("width".into(), "20".into())]),
            "the daemon no longer decides"
        );
        assert_eq!(value_of(&again, "width"), "90");
    }

    #[test]
    fn the_plugin_refuses_values_it_cannot_use() {
        let mut owned = Owned::load(Some(temp("refuse")));
        assert!(!owned.change("width", "500"));
        assert!(!owned.change("renderer", "vulkan"));
        assert!(!owned.change("unknown", "x"));
        assert!(owned.change("rainbow", "true"));
        assert!(owned.settings().rainbow);
    }
}
