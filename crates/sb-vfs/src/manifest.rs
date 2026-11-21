//! Parseur résilient des manifestes `MicrosoftGame.Config` et `appxmanifest.xml`.

use std::fs;
use std::path::{Path, PathBuf};

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("aucun manifeste (MicrosoftGame.Config ou appxmanifest.xml) trouvé dans {0}")]
    NotFound(PathBuf),
    #[error("erreur d'entrée/sortie sur {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("erreur de parsing XML: {0}")]
    Xml(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManifestKind {
    MicrosoftGameConfig,
    AppxManifest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutableEntry {
    pub name: String,
    pub id: Option<String>,
    pub target_device_family: Option<String>,
    pub alias: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppManifest {
    pub kind: ManifestKind,
    pub source_file: Option<PathBuf>,
    pub identity_name: Option<String>,
    pub publisher: Option<String>,
    pub version: Option<String>,
    pub title_id: Option<String>,
    pub display_name: Option<String>,
    pub executables: Vec<ExecutableEntry>,
    pub persistent_local_storage_mb: Option<u64>,
    pub persistent_local_storage_growable_mb: Option<u64>,
    pub languages: Vec<String>,
}

impl AppManifest {
    /// Recherche `MicrosoftGame.Config` (prioritaire) puis `appxmanifest.xml` dans `dir`
    /// de manière insensible à la casse.
    pub fn discover_in_dir(dir: &Path) -> Result<Self, ManifestError> {
        let entries = fs::read_dir(dir).map_err(|source| ManifestError::Io {
            path: dir.to_path_buf(),
            source,
        })?;

        let mut game_config: Option<PathBuf> = None;
        let mut appx: Option<PathBuf> = None;

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if file_name.eq_ignore_ascii_case("MicrosoftGame.Config") {
                game_config = Some(path);
            } else if file_name.eq_ignore_ascii_case("appxmanifest.xml") {
                appx = Some(path);
            }
        }

        if let Some(path) = game_config {
            Self::from_file(&path, ManifestKind::MicrosoftGameConfig)
        } else if let Some(path) = appx {
            Self::from_file(&path, ManifestKind::AppxManifest)
        } else {
            Err(ManifestError::NotFound(dir.to_path_buf()))
        }
    }

    pub fn from_file(path: &Path, kind: ManifestKind) -> Result<Self, ManifestError> {
        let content = fs::read_to_string(path).map_err(|source| ManifestError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut manifest = match kind {
            ManifestKind::MicrosoftGameConfig => Self::parse_microsoft_game_config(&content)?,
            ManifestKind::AppxManifest => Self::parse_appx_manifest(&content)?,
        };
        manifest.source_file = Some(path.to_path_buf());
        Ok(manifest)
    }

    /// Parse le contenu XML d'un fichier `MicrosoftGame.Config`.
    pub fn parse_microsoft_game_config(xml: &str) -> Result<Self, ManifestError> {
        let clean = xml.strip_prefix('\u{feff}').unwrap_or(xml);
        let mut reader = Reader::from_str(clean);
        reader.config_mut().trim_text(true);

        let mut manifest = Self {
            kind: ManifestKind::MicrosoftGameConfig,
            source_file: None,
            identity_name: None,
            publisher: None,
            version: None,
            title_id: None,
            display_name: None,
            executables: Vec::new(),
            persistent_local_storage_mb: None,
            persistent_local_storage_growable_mb: None,
            languages: Vec::new(),
        };

        let mut stack: Vec<String> = Vec::new();
        let mut buf = Vec::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) => {
                    let tag = local_tag_name(e);
                    handle_game_config_element(&mut manifest, &tag, e);
                    stack.push(tag);
                }
                Ok(Event::Empty(ref e)) => {
                    let tag = local_tag_name(e);
                    handle_game_config_element(&mut manifest, &tag, e);
                }
                Ok(Event::Text(ref t)) => {
                    let text = t
                        .unescape()
                        .map_err(|err| ManifestError::Xml(err.to_string()))?
                        .trim()
                        .to_string();
                    if text.is_empty() {
                        continue;
                    }
                    let current = stack.last().map(String::as_str);
                    let parent = stack
                        .len()
                        .checked_sub(2)
                        .and_then(|i| stack.get(i))
                        .map(String::as_str);

                    match (parent, current) {
                        (_, Some("TitleId")) => manifest.title_id = Some(text),
                        (Some("PersistentLocalStorage"), Some("SizeMB")) => {
                            manifest.persistent_local_storage_mb = text.parse::<u64>().ok();
                        }
                        (Some("PersistentLocalStorage"), Some("GrowableToMB")) => {
                            manifest.persistent_local_storage_growable_mb =
                                text.parse::<u64>().ok();
                        }
                        _ => {}
                    }
                }
                Ok(Event::End(_)) => {
                    stack.pop();
                }
                Ok(Event::Eof) => break,
                Err(err) => return Err(ManifestError::Xml(err.to_string())),
                _ => {}
            }
            buf.clear();
        }

        Ok(manifest)
    }

    /// Parse le contenu XML d'un fichier `appxmanifest.xml`.
    pub fn parse_appx_manifest(xml: &str) -> Result<Self, ManifestError> {
        let clean = xml.strip_prefix('\u{feff}').unwrap_or(xml);
        let mut reader = Reader::from_str(clean);
        reader.config_mut().trim_text(true);

        let mut manifest = Self {
            kind: ManifestKind::AppxManifest,
            source_file: None,
            identity_name: None,
            publisher: None,
            version: None,
            title_id: None,
            display_name: None,
            executables: Vec::new(),
            persistent_local_storage_mb: None,
            persistent_local_storage_growable_mb: None,
            languages: Vec::new(),
        };

        let mut stack: Vec<String> = Vec::new();
        let mut buf = Vec::new();

        loop {
            match reader.read_event_into(&mut buf) {
                Ok(Event::Start(ref e)) => {
                    let tag = local_tag_name(e);
                    handle_appx_element(&mut manifest, &tag, e);
                    stack.push(tag);
                }
                Ok(Event::Empty(ref e)) => {
                    let tag = local_tag_name(e);
                    handle_appx_element(&mut manifest, &tag, e);
                }
                Ok(Event::Text(ref t)) => {
                    let text = t
                        .unescape()
                        .map_err(|err| ManifestError::Xml(err.to_string()))?
                        .trim()
                        .to_string();
                    if text.is_empty() {
                        continue;
                    }
                    if stack.last().map(String::as_str) == Some("DisplayName") {
                        manifest.display_name = Some(text);
                    }
                }
                Ok(Event::End(_)) => {
                    stack.pop();
                }
                Ok(Event::Eof) => break,
                Err(err) => return Err(ManifestError::Xml(err.to_string())),
                _ => {}
            }
            buf.clear();
        }

        Ok(manifest)
    }

    /// Retourne l'identifiant stable utilisé pour isoler les sauvegardes/temp du titre.
    pub fn sandbox_id(&self) -> String {
        let raw = self
            .title_id
            .as_deref()
            .or(self.identity_name.as_deref())
            .or(self.display_name.as_deref())
            .unwrap_or("default-title");

        let sanitized: String = raw
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                    c
                } else {
                    '_'
                }
            })
            .collect();

        if sanitized.is_empty() || sanitized.chars().all(|c| c == '.') {
            "default-title".to_string()
        } else {
            sanitized
        }
    }

    /// Retourne l'exécutable principal (en privilégiant une cible Scarlett/Xbox si présente).
    pub fn primary_executable(&self) -> Option<&ExecutableEntry> {
        self.executables
            .iter()
            .find(|e| {
                e.target_device_family
                    .as_deref()
                    .is_some_and(|fam| fam.eq_ignore_ascii_case("Scarlett"))
            })
            .or_else(|| self.executables.first())
    }
}

fn local_tag_name(e: &BytesStart<'_>) -> String {
    let qname = e.name();
    let raw = String::from_utf8_lossy(qname.as_ref());
    raw.rsplit(':').next().unwrap_or(&raw).to_string()
}

fn attr_value(e: &BytesStart<'_>, wanted: &str) -> Option<String> {
    for attr in e.attributes().flatten() {
        let key = String::from_utf8_lossy(attr.key.as_ref());
        let local = key.rsplit(':').next().unwrap_or(&key);
        if local.eq_ignore_ascii_case(wanted) {
            return attr.unescape_value().ok().map(|v| v.into_owned());
        }
    }
    None
}

fn handle_game_config_element(manifest: &mut AppManifest, tag: &str, e: &BytesStart<'_>) {
    match tag {
        "Identity" => {
            manifest.identity_name = attr_value(e, "Name");
            manifest.publisher = attr_value(e, "Publisher");
            manifest.version = attr_value(e, "Version");
        }
        "ShellVisuals" => {
            if let Some(name) = attr_value(e, "DefaultDisplayName") {
                manifest.display_name = Some(name);
            }
        }
        "Executable" => {
            if let Some(name) = attr_value(e, "Name") {
                manifest.executables.push(ExecutableEntry {
                    name,
                    id: attr_value(e, "Id"),
                    target_device_family: attr_value(e, "TargetDeviceFamily"),
                    alias: attr_value(e, "Alias"),
                });
            }
        }
        "Resource" => {
            if let Some(lang) = attr_value(e, "Language")
                && !manifest.languages.contains(&lang)
            {
                manifest.languages.push(lang);
            }
        }
        _ => {}
    }
}

fn handle_appx_element(manifest: &mut AppManifest, tag: &str, e: &BytesStart<'_>) {
    match tag {
        "Identity" => {
            manifest.identity_name = attr_value(e, "Name");
            manifest.publisher = attr_value(e, "Publisher");
            manifest.version = attr_value(e, "Version");
        }
        "Application" => {
            if let Some(exec) = attr_value(e, "Executable") {
                manifest.executables.push(ExecutableEntry {
                    name: exec,
                    id: attr_value(e, "Id"),
                    target_device_family: None,
                    alias: None,
                });
            }
        }
        "Resource" => {
            if let Some(lang) = attr_value(e, "Language")
                && !manifest.languages.contains(&lang)
            {
                manifest.languages.push(lang);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_microsoft_game_config() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<Game configVersion="1">
  <Identity Name="ScarletBridge.SampleApp" Publisher="CN=ScarletTest" Version="1.2.3.0"/>
  <TitleId>7A1B2C3D</TitleId>
  <ExecutableList>
    <Executable Name="SamplePC.exe" Id="GamePC" TargetDeviceFamily="PC"/>
    <Executable Name="SampleScarlett.exe" Id="GameXS" TargetDeviceFamily="Scarlett" Alias="Game"/>
  </ExecutableList>
  <ShellVisuals DefaultDisplayName="Scarlet Sample"/>
  <PersistentLocalStorage>
    <SizeMB>256</SizeMB>
    <GrowableToMB>1024</GrowableToMB>
  </PersistentLocalStorage>
  <Resources>
    <Resource Language="fr-FR"/>
    <Resource Language="en-US"/>
  </Resources>
</Game>"#;

        let m = AppManifest::parse_microsoft_game_config(xml).expect("valid MicrosoftGame.Config");
        assert_eq!(m.kind, ManifestKind::MicrosoftGameConfig);
        assert_eq!(m.identity_name.as_deref(), Some("ScarletBridge.SampleApp"));
        assert_eq!(m.title_id.as_deref(), Some("7A1B2C3D"));
        assert_eq!(m.display_name.as_deref(), Some("Scarlet Sample"));
        assert_eq!(m.persistent_local_storage_mb, Some(256));
        assert_eq!(m.persistent_local_storage_growable_mb, Some(1024));
        assert_eq!(m.languages, vec!["fr-FR", "en-US"]);
        assert_eq!(
            m.primary_executable().map(|e| e.name.as_str()),
            Some("SampleScarlett.exe")
        );
        assert_eq!(m.sandbox_id(), "7A1B2C3D");
    }

    #[test]
    fn parses_appx_manifest() {
        let xml = r#"<?xml version="1.0" encoding="utf-8"?>
<Package xmlns="http://schemas.microsoft.com/appx/manifest/foundation/windows10"
         xmlns:uap="http://schemas.microsoft.com/appx/manifest/uap/windows10">
  <Identity Name="ScarletBridge.AppxSample" Publisher="CN=Test" Version="2.0.0.0"/>
  <Properties>
    <DisplayName>Appx Homebrew</DisplayName>
  </Properties>
  <Resources>
    <Resource Language="en-us"/>
  </Resources>
  <Applications>
    <Application Id="App" Executable="Homebrew.exe"/>
  </Applications>
</Package>"#;

        let m = AppManifest::parse_appx_manifest(xml).expect("valid appxmanifest");
        assert_eq!(m.kind, ManifestKind::AppxManifest);
        assert_eq!(m.display_name.as_deref(), Some("Appx Homebrew"));
        assert_eq!(
            m.primary_executable().map(|e| e.name.as_str()),
            Some("Homebrew.exe")
        );
        assert_eq!(m.languages, vec!["en-us"]);
    }
}
