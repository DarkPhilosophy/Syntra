use fluent_bundle::{
    FluentArgs, FluentResource, concurrent::FluentBundle as ConcurrentFluentBundle,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use unic_langid::LanguageIdentifier;

pub const REQUIRED_KEYS: &[&str] = &[
    "navigation-settings",
    "device-status-connected",
    "app-title",
];
const MAX_CATALOG_BYTES: u64 = 1024 * 1024;
const EN_US: &str = include_str!("../locales/en-US.ftl");
const RO_RO: &str = include_str!("../locales/ro-RO.ftl");
const DE_DE: &str = include_str!("../locales/de-DE.ftl");
const EN_XA: &str = include_str!("../locales/en-XA.ftl");

type Bundle = ConcurrentFluentBundle<FluentResource>;

pub fn embedded_resources() -> BTreeMap<&'static str, &'static str> {
    [
        ("en-US", EN_US),
        ("ro-RO", RO_RO),
        ("de-DE", DE_DE),
        ("en-XA", EN_XA),
    ]
    .into_iter()
    .collect()
}

pub fn normalize_locale(input: &str) -> String {
    let clean = input
        .split(['.', '@'])
        .next()
        .unwrap_or(input)
        .replace('_', "-");
    let mut parts = clean.split('-');
    let language = parts.next().unwrap_or("en").to_ascii_lowercase();
    let region = parts.next().map(|s| s.to_ascii_uppercase());
    region
        .map(|r| format!("{language}-{r}"))
        .unwrap_or(language)
}

fn bundle_for(locale: &str, source: String) -> Result<Bundle, String> {
    let language: LanguageIdentifier = locale
        .parse()
        .map_err(|error| format!("invalid locale {locale:?}: {error}"))?;
    let resource = FluentResource::try_new(source)
        .map_err(|(_, errors)| format!("invalid Fluent catalog for {locale}: {errors:?}"))?;
    let mut bundle = Bundle::new_concurrent(vec![language]);
    bundle.set_use_isolating(false);
    bundle
        .add_resource(resource)
        .map_err(|errors| format!("invalid Fluent catalog for {locale}: {errors:?}"))?;
    Ok(bundle)
}

fn locale_from_path(path: &Path) -> Result<String, String> {
    if path.extension().and_then(|extension| extension.to_str()) != Some("ftl") {
        return Err("custom dictionaries must use the .ftl extension".into());
    }
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or_else(|| "custom dictionary filename must be valid UTF-8".to_string())?;
    let locale: LanguageIdentifier = stem
        .parse()
        .map_err(|error| format!("invalid language tag in filename {stem:?}: {error}"))?;
    Ok(locale.to_string())
}

fn read_catalog(path: &Path) -> Result<String, String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("could not inspect {}: {error}", path.display()))?;
    if !metadata.is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if metadata.len() > MAX_CATALOG_BYTES {
        return Err(format!(
            "{} exceeds the 1 MiB custom dictionary limit",
            path.display()
        ));
    }
    let bytes =
        fs::read(path).map_err(|error| format!("could not read {}: {error}", path.display()))?;
    if bytes.len() as u64 > MAX_CATALOG_BYTES {
        return Err(format!(
            "{} exceeds the 1 MiB custom dictionary limit",
            path.display()
        ));
    }
    String::from_utf8(bytes)
        .map_err(|error| format!("{} is not valid UTF-8: {error}", path.display()))
}

fn parse_catalog(path: &Path) -> Result<(String, String, Bundle), String> {
    let locale = locale_from_path(path)?;
    let source = read_catalog(path)?;
    let bundle = bundle_for(&locale, source.clone())?;
    Ok((locale, source, bundle))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalizationSnapshot {
    pub locale: String,
    pub revision: u64,
}

pub struct Localizer {
    locale: String,
    revision: u64,
    observers: Vec<Sender<LocalizationSnapshot>>,
    embedded: BTreeMap<String, Bundle>,
    custom: BTreeMap<String, Bundle>,
}

impl Localizer {
    pub fn new(locale: &str) -> Result<Self, String> {
        let embedded = embedded_resources()
            .into_iter()
            .map(|(locale, source)| {
                bundle_for(locale, source.to_owned()).map(|bundle| (locale.to_owned(), bundle))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            locale: normalize_locale(locale),
            revision: 0,
            observers: Vec::new(),
            embedded,
            custom: BTreeMap::new(),
        })
    }

    pub fn snapshot(&self) -> LocalizationSnapshot {
        LocalizationSnapshot {
            locale: self.locale.clone(),
            revision: self.revision,
        }
    }

    pub fn observe(&mut self) -> Receiver<LocalizationSnapshot> {
        let (tx, rx) = mpsc::channel();
        self.observers.push(tx);
        rx
    }

    pub fn set_locale(&mut self, locale: &str) -> Result<bool, String> {
        let normalized = normalize_locale(locale);
        if normalized == self.locale {
            return Ok(false);
        }
        self.locale = normalized;
        self.notify_changed();
        Ok(true)
    }

    pub fn load_directory(&mut self, directory: &Path) -> Result<Vec<String>, String> {
        let entries = match fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let active_changed = self.custom_affects_current();
                self.custom.clear();
                if active_changed {
                    self.notify_changed();
                }
                return Ok(Vec::new());
            }
            Err(error) => {
                return Err(format!(
                    "could not read custom dictionary directory {}: {error}",
                    directory.display()
                ));
            }
        };
        let mut paths = entries
            .map(|entry| {
                entry
                    .map(|entry| entry.path())
                    .map_err(|error| format!("could not read directory entry: {error}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        paths.sort();

        let mut loaded = BTreeMap::new();
        for path in paths {
            if path.extension().and_then(|extension| extension.to_str()) != Some("ftl") {
                continue;
            }
            let (locale, _, bundle) = parse_catalog(&path)?;
            loaded.insert(locale, bundle);
        }

        let active_changed =
            self.custom_affects_current() || Self::catalogs_affect_locale(&loaded, &self.locale);
        let locales = loaded.keys().cloned().collect();
        self.custom = loaded;
        if active_changed {
            self.notify_changed();
        }
        Ok(locales)
    }

    pub fn import_file(&mut self, source: &Path, directory: &Path) -> Result<String, String> {
        let (locale, source, bundle) = parse_catalog(source)?;
        fs::create_dir_all(directory).map_err(|error| {
            format!(
                "could not create custom dictionary directory {}: {error}",
                directory.display()
            )
        })?;
        let destination = directory.join(format!("{locale}.ftl"));
        Self::atomic_replace(&destination, &source)?;

        let affects_current = self.locale_candidates().contains(&locale);
        self.custom.insert(locale.clone(), bundle);
        if affects_current {
            self.notify_changed();
        }
        Ok(locale)
    }

    pub fn available_locales(&self) -> Vec<String> {
        self.embedded
            .keys()
            .chain(self.custom.keys())
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn format(&self, key: &str, args: Option<&FluentArgs>) -> String {
        let candidates = self.locale_candidates();
        for locale in &candidates {
            if let Some(value) = self.format_from(self.custom.get(locale), key, args) {
                return value;
            }
            if let Some(value) = self.format_from(self.embedded.get(locale), key, args) {
                return value;
            }
        }
        if !candidates.iter().any(|locale| locale == "en-US") {
            if let Some(value) = self.format_from(self.embedded.get("en-US"), key, args) {
                return value;
            }
        }
        key.to_string()
    }

    fn locale_candidates(&self) -> Vec<String> {
        let mut candidates = vec![self.locale.clone()];
        let base = self.locale.split('-').next().unwrap_or("en");
        if base != self.locale {
            candidates.push(base.to_owned());
        }
        let embedded_base = match base {
            "de" => "de-DE",
            "ro" => "ro-RO",
            _ => "en-US",
        };
        if !candidates
            .iter()
            .any(|candidate| candidate == embedded_base)
        {
            candidates.push(embedded_base.to_owned());
        }
        candidates
    }

    fn format_from(
        &self,
        bundle: Option<&Bundle>,
        key: &str,
        args: Option<&FluentArgs>,
    ) -> Option<String> {
        let bundle = bundle?;
        let pattern = bundle.get_message(key)?.value()?;
        let mut errors = Vec::new();
        let value = bundle.format_pattern(pattern, args, &mut errors);
        errors.is_empty().then(|| value.into_owned())
    }

    fn custom_affects_current(&self) -> bool {
        Self::catalogs_affect_locale(&self.custom, &self.locale)
    }

    fn catalogs_affect_locale(catalogs: &BTreeMap<String, Bundle>, locale: &str) -> bool {
        catalogs.contains_key(locale)
            || locale
                .split('-')
                .next()
                .is_some_and(|base| catalogs.contains_key(base))
    }

    fn notify_changed(&mut self) {
        self.revision += 1;
        let snapshot = self.snapshot();
        self.observers
            .retain(|observer| observer.send(snapshot.clone()).is_ok());
    }

    fn atomic_replace(destination: &Path, contents: &str) -> Result<(), String> {
        let directory = destination
            .parent()
            .ok_or_else(|| "custom dictionary destination has no parent directory".to_string())?;
        let mut temporary = PathBuf::new();
        let mut file = None;
        for attempt in 0..100 {
            let candidate = directory.join(format!(
                ".syntra-language-{}-{attempt}.tmp",
                std::process::id()
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(created) => {
                    temporary = candidate;
                    file = Some(created);
                    break;
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(format!(
                        "could not create temporary dictionary in {}: {error}",
                        directory.display()
                    ));
                }
            }
        }
        let mut file = file.ok_or_else(|| {
            format!(
                "could not reserve a temporary dictionary in {}",
                directory.display()
            )
        })?;
        let result = (|| {
            file.write_all(contents.as_bytes())
                .map_err(|error| format!("could not write temporary dictionary: {error}"))?;
            file.sync_all()
                .map_err(|error| format!("could not sync temporary dictionary: {error}"))?;
            drop(file);
            fs::rename(&temporary, destination).map_err(|error| {
                format!(
                    "could not replace custom dictionary {}: {error}",
                    destination.display()
                )
            })
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn normalizes_locale_forms() {
        assert_eq!(normalize_locale("DE_de.UTF-8@euro"), "de-DE");
        assert_eq!(normalize_locale("ro_ro"), "ro-RO");
        assert_eq!(normalize_locale("en-xa"), "en-XA");
    }
    #[test]
    fn parses_and_contains_all_required_keys() {
        for (_, source) in embedded_resources() {
            let resource = FluentResource::try_new(source.to_owned()).unwrap();
            let text = format!("{resource:?}");
            for key in REQUIRED_KEYS {
                assert!(source.contains(key), "missing {key}");
            }
            let _ = text;
        }
    }
    #[test]
    fn interpolates() {
        let l = Localizer::new("en-US").unwrap();
        let mut a = FluentArgs::new();
        a.set("name", "office-pc");
        assert_eq!(
            l.format("device-status-connected", Some(&a)),
            "Connected to office-pc"
        );
    }
    #[test]
    fn fallback_and_pseudo() {
        assert_eq!(
            Localizer::new("de-AT")
                .unwrap()
                .format("navigation-settings", None),
            "Einstellungen"
        );
        assert_eq!(
            Localizer::new("fr")
                .unwrap()
                .format("navigation-settings", None),
            "Settings"
        );
        assert_eq!(
            Localizer::new("fr").unwrap().format("missing", None),
            "missing"
        );
        assert!(
            Localizer::new("en-XA")
                .unwrap()
                .format("navigation-settings", None)
                .len()
                > 8
        );
    }
    #[test]
    fn revision_observer() {
        let mut l = Localizer::new("en-US").unwrap();
        let r = l.observe();
        l.set_locale("de-DE").unwrap();
        assert_eq!(r.recv().unwrap().revision, 1);
    }
    fn temp_directory(name: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "syntra-localization-{name}-{}-{unique}",
            std::process::id()
        ))
    }

    #[test]
    fn custom_catalog_falls_back_to_embedded_english() {
        let directory = temp_directory("fallback");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("fr-FR.ftl"),
            "navigation-settings = Paramètres personnalisés\n",
        )
        .unwrap();

        let mut localizer = Localizer::new("fr-FR").unwrap();
        assert_eq!(localizer.load_directory(&directory).unwrap(), vec!["fr-FR"]);
        assert_eq!(
            localizer.format("navigation-settings", None),
            "Paramètres personnalisés"
        );
        assert_eq!(
            localizer.format("app-title", None),
            Localizer::new("en-US").unwrap().format("app-title", None)
        );
        assert!(localizer.available_locales().contains(&"fr-FR".to_string()));

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn invalid_import_preserves_existing_catalog_and_destination() {
        let directory = temp_directory("invalid-import");
        let valid_directory = directory.join("valid");
        let invalid_directory = directory.join("invalid");
        std::fs::create_dir_all(&valid_directory).unwrap();
        std::fs::create_dir_all(&invalid_directory).unwrap();
        let valid = valid_directory.join("fr-FR.ftl");
        let invalid = invalid_directory.join("fr-FR.ftl");
        let destination = directory.join("catalogs");
        std::fs::write(&valid, "navigation-settings = Valide\n").unwrap();
        std::fs::write(&invalid, "navigation-settings = { broken\n").unwrap();

        let mut localizer = Localizer::new("fr-FR").unwrap();
        assert_eq!(
            localizer.import_file(&valid, &destination).unwrap(),
            "fr-FR"
        );
        let prior = std::fs::read(destination.join("fr-FR.ftl")).unwrap();
        assert!(localizer.import_file(&invalid, &destination).is_err());
        assert_eq!(std::fs::read(destination.join("fr-FR.ftl")).unwrap(), prior);
        assert_eq!(localizer.format("navigation-settings", None), "Valide");

        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn reimporting_current_locale_notifies_live_revision() {
        let directory = temp_directory("reimport");
        std::fs::create_dir_all(&directory).unwrap();
        let source = directory.join("fr-FR.ftl");
        let destination = directory.join("catalogs");
        std::fs::write(&source, "navigation-settings = Première\n").unwrap();

        let mut localizer = Localizer::new("fr-FR").unwrap();
        localizer.import_file(&source, &destination).unwrap();
        let receiver = localizer.observe();
        std::fs::write(&source, "navigation-settings = Deuxième\n").unwrap();
        localizer.import_file(&source, &destination).unwrap();

        assert_eq!(receiver.recv().unwrap().revision, 2);
        assert_eq!(localizer.format("navigation-settings", None), "Deuxième");

        std::fs::remove_dir_all(directory).unwrap();
    }
}
