//! The plugin's own settings window, run as `syntra-plugin-edge-glow --settings`.
//!
//! A process of its own: the glow already owns this binary's only window event
//! loop, and a settings window must open even while the glow is stopped. It
//! reads and writes the plugin's settings file itself, and reports every
//! accepted change on standard output as a `change_setting` line, so a running
//! glow applies it at once. Syntra lends it its controls and, through
//! `SYNTRA_PLUGIN_THEME`, its colours and texts.

use std::io::{self, Write};

use slint::{Color, ComponentHandle, ModelRc, SharedString, VecModel};
use syntra_plugin_api::{Message, SettingControlKind};
use syntra_plugin_edge_glow::{glow, owned::Owned};

slint::include_modules!();

/// Environment variable carrying Syntra's theme as `name=value` lines.
pub const THEME_ENV: &str = "SYNTRA_PLUGIN_THEME";

pub fn run() -> Result<(), String> {
    let owned = std::rc::Rc::new(std::cell::RefCell::new(Owned::load(Owned::default_path())));
    let window = SettingsWindow::new().map_err(|error| error.to_string())?;
    let theme_entries = parse_theme(&std::env::var(THEME_ENV).unwrap_or_default());
    apply_theme(&window, &theme_entries);
    window.set_description(
        "How the edge lights up when a pointer comes in or is pushed against an edge.".into(),
    );
    refresh(&window, &owned.borrow());

    let change = {
        let owned = owned.clone();
        let weak = window.as_weak();
        move |key: String, value: String| {
            let mut settings = owned.borrow_mut();
            // An empty colour means the plugin's default.
            let value = if value.is_empty() {
                default_of(&settings, &key).unwrap_or(value)
            } else {
                value
            };
            if settings.change(&key, &value) {
                let line = Message::ChangeSetting { key, value }.encode_line();
                if let Ok(line) = line {
                    let mut out = io::stdout().lock();
                    let _ = out.write_all(line.as_bytes()).and_then(|()| out.flush());
                }
            }
            if let Some(window) = weak.upgrade() {
                refresh(&window, &settings);
            }
        }
    };
    {
        let change = change.clone();
        window.on_changed(move |key, value| change(key.to_string(), value.to_string()));
    }
    window.on_colour_chosen(move |key, colour| {
        change(
            key.to_string(),
            glow::to_hex((colour.red(), colour.green(), colour.blue())),
        )
    });
    {
        let weak = window.as_weak();
        window.on_close_requested(move || {
            if let Some(window) = weak.upgrade() {
                let _ = window.hide();
            }
        });
    }
    window.run().map_err(|error| error.to_string())
}

fn default_of(owned: &Owned, key: &str) -> Option<String> {
    let Message::DeclareSettings { settings } = owned.declaration() else {
        return None;
    };
    settings
        .into_iter()
        .find(|setting| setting.key == key)
        .map(|setting| setting.default)
}

/// Draws the plugin's settings as they stand.
fn refresh(window: &SettingsWindow, owned: &Owned) {
    let Message::DeclareSettings { settings } = owned.declaration() else {
        return;
    };
    let rows: Vec<PluginSettingEntry> = settings
        .into_iter()
        .map(|setting| {
            let (kind, values, labels, min, max) = match setting.kind {
                SettingControlKind::Toggle => ("toggle", Vec::new(), Vec::new(), 0, 0),
                SettingControlKind::Color => ("color", Vec::new(), Vec::new(), 0, 0),
                SettingControlKind::Choice { options } => (
                    "choice",
                    options.iter().map(|option| option.value.clone()).collect(),
                    options.iter().map(|option| option.label.clone()).collect(),
                    0,
                    0,
                ),
                SettingControlKind::Number { min, max } => (
                    "number",
                    Vec::new(),
                    Vec::new(),
                    min.clamp(i32::MIN.into(), i32::MAX.into()) as i32,
                    max.clamp(i32::MIN.into(), i32::MAX.into()) as i32,
                ),
            };
            let (red, green, blue) = glow::parse_hex(&setting.value)
                .or_else(|| glow::parse_hex(&setting.default))
                .unwrap_or((0.2, 0.8, 1.0));
            PluginSettingEntry {
                key: setting.key.into(),
                label: setting.label.into(),
                kind: kind.into(),
                value: setting.value.into(),
                option_values: strings(values),
                option_labels: strings(labels),
                min,
                max,
                colour: Color::from_rgb_f32(red, green, blue),
            }
        })
        .collect();
    window.set_settings(ModelRc::new(VecModel::from(rows)));
}

fn strings(values: Vec<String>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        values
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    ))
}

/// `name=value` lines into pairs; a value may contain `=`.
pub fn parse_theme(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

fn apply_theme(window: &SettingsWindow, entries: &[(String, String)]) {
    let theme = window.global::<Theme>();
    let mut texts = std::collections::HashMap::new();
    for (name, value) in entries {
        if let Some(key) = name.strip_prefix("tr:") {
            texts.insert(key.to_owned(), value.clone());
            continue;
        }
        if name == "base-mode" {
            theme.set_base_mode(value.into());
            continue;
        }
        let Some((red, green, blue)) = glow::parse_hex(value) else {
            continue;
        };
        let colour = Color::from_rgb_f32(red, green, blue);
        match name.as_str() {
            "background" => theme.set_background(colour),
            "surface" => theme.set_surface(colour),
            "elevated-surface" => theme.set_elevated_surface(colour),
            "accent" => theme.set_accent(colour),
            "text" => theme.set_text(colour),
            "muted-text" => theme.set_muted_text(colour),
            "border" => theme.set_border(colour),
            "selection" => theme.set_selection(colour),
            "hover-surface" => theme.set_hover_surface(colour),
            "pressed-surface" => theme.set_pressed_surface(colour),
            "on-accent" => theme.set_on_accent(colour),
            _ => {}
        }
    }
    window.global::<Translations>().on_translate(move |key, _| {
        texts
            .get(key.as_str())
            .cloned()
            .unwrap_or_else(|| english(&key).to_owned())
            .into()
    });
}

/// What the shared controls say when Syntra lent no text.
fn english(key: &str) -> &str {
    match key {
        "action-edit" => "Edit",
        "action-close" | "plugin-settings-close" => "Close",
        "settings-titlebar-default" => "Default",
        "color-hue" => "Hue",
        "color-saturation" => "Saturation",
        "color-brightness" => "Brightness",
        other => other,
    }
}
