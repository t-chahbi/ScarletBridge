//! Gestion locale hors-ligne des profils utilisateur (`XUser*`) et des succès.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProfileError {
    #[error("erreur I/O profil sur {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("erreur JSON profil : {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalUserProfile {
    /// XUID local fictif stable (hors-ligne).
    pub xuid: u64,
    pub gamertag: String,
    pub locale: String,
    pub achievements: BTreeMap<String, AchievementProgress>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AchievementProgress {
    pub percent: u8,
    pub unlocked: bool,
}

impl Default for LocalUserProfile {
    fn default() -> Self {
        Self {
            xuid: 0x0009_0000_0000_0001,
            gamertag: "ScarletPlayer".to_string(),
            locale: "fr-FR".to_string(),
            achievements: BTreeMap::new(),
        }
    }
}

impl LocalUserProfile {
    pub fn load_or_create(path: &Path) -> Result<Self, ProfileError> {
        if path.exists() {
            let text = fs::read_to_string(path).map_err(|source| ProfileError::Io {
                path: path.to_path_buf(),
                source,
            })?;
            Ok(serde_json::from_str(&text)?)
        } else {
            let profile = Self::default();
            profile.save(path)?;
            Ok(profile)
        }
    }

    pub fn save(&self, path: &Path) -> Result<(), ProfileError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|source| ProfileError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let json = serde_json::to_string_pretty(self)?;
        fs::write(path, json).map_err(|source| ProfileError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Met à jour la progression d'un succès local (monotone : ne régresse jamais).
    pub fn update_achievement(&mut self, id: &str, percent: u8) -> bool {
        let clamped = percent.min(100);
        let entry = self
            .achievements
            .entry(id.to_string())
            .or_insert(AchievementProgress {
                percent: 0,
                unlocked: false,
            });
        if clamped > entry.percent {
            entry.percent = clamped;
            if clamped == 100 {
                entry.unlocked = true;
            }
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn creates_updates_and_reloads_offline_profile() {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir()
            .join(format!("scarletbridge_profile_{nanos}"))
            .join("profile.json");

        let mut profile = LocalUserProfile::load_or_create(&path).expect("create");
        assert_eq!(profile.gamertag, "ScarletPlayer");
        assert!(profile.update_achievement("ACH_FIRST_BOOT", 50));
        assert!(!profile.update_achievement("ACH_FIRST_BOOT", 25));
        assert!(profile.update_achievement("ACH_FIRST_BOOT", 100));
        profile.save(&path).expect("save");

        let reloaded = LocalUserProfile::load_or_create(&path).expect("reload");
        assert_eq!(reloaded, profile);
        assert!(reloaded.achievements["ACH_FIRST_BOOT"].unlocked);

        if let Some(parent) = path.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }
}
