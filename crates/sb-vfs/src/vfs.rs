//! Système de fichiers virtuel (VFS) isolé pour ScarletBridge.
//!
//! Empêche strictement tout échappement hors des racines autorisées (racine du jeu,
//! sauvegardes `PersistentLocalStorage`, stockage temporaire) : `/home` n'est jamais exposé.

use std::fs;
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VfsMount {
    /// Racine applicative (lecture principale) : `game:/` ou `G:\`.
    Game,
    /// Stockage persistant (`PersistentLocalStorage`) : `save:/` ou `S:\`.
    Save,
    /// Stockage temporaire / scratch : `temp:/` ou `T:\`.
    Temp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPath {
    pub mount: VfsMount,
    pub host_path: PathBuf,
    pub relative_path: PathBuf,
}

#[derive(Debug, Error)]
pub enum VfsError {
    #[error("chemin invité vide ou invalide : {0}")]
    InvalidPath(String),
    #[error("tentative d'accès hors sandbox refusée : {0}")]
    EscapeAttempt(String),
    #[error("lecteur ou schéma non autorisé dans le VFS : {0}")]
    UnsupportedPrefix(String),
    #[error("quota PersistentLocalStorage dépassé ({used} + {requested} > {limit} octets)")]
    SaveQuotaExceeded {
        used: u64,
        requested: u64,
        limit: u64,
    },
    #[error("erreur d'entrée/sortie VFS sur {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone)]
pub struct VirtualFs {
    game_root: PathBuf,
    save_root: PathBuf,
    temp_root: PathBuf,
    save_quota_bytes: Option<u64>,
}

impl VirtualFs {
    /// Crée un VFS isolé avec trois racines distinctes déjà présentes ou créées à la volée.
    pub fn new(
        game_root: &Path,
        save_root: &Path,
        temp_root: &Path,
        save_quota_mb: Option<u64>,
    ) -> Result<Self, VfsError> {
        fs::create_dir_all(save_root).map_err(|source| VfsError::Io {
            path: save_root.to_path_buf(),
            source,
        })?;
        fs::create_dir_all(temp_root).map_err(|source| VfsError::Io {
            path: temp_root.to_path_buf(),
            source,
        })?;

        let game_root = fs::canonicalize(game_root).map_err(|source| VfsError::Io {
            path: game_root.to_path_buf(),
            source,
        })?;
        let save_root = fs::canonicalize(save_root).map_err(|source| VfsError::Io {
            path: save_root.to_path_buf(),
            source,
        })?;
        let temp_root = fs::canonicalize(temp_root).map_err(|source| VfsError::Io {
            path: temp_root.to_path_buf(),
            source,
        })?;

        Ok(Self {
            game_root,
            save_root,
            temp_root,
            save_quota_bytes: save_quota_mb.map(|mb| mb.saturating_mul(1024 * 1024)),
        })
    }

    /// Construit un sandbox standard sous `<state_base>/<title_id>/{save,temp}`.
    pub fn new_sandboxed(
        game_root: &Path,
        state_base: &Path,
        title_id: &str,
        save_quota_mb: Option<u64>,
    ) -> Result<Self, VfsError> {
        let safe_id: String = title_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
            .collect();
        if safe_id.is_empty() || safe_id == "." || safe_id == ".." {
            return Err(VfsError::InvalidPath(title_id.to_string()));
        }

        let title_dir = state_base.join(safe_id);
        let save_root = title_dir.join("save");
        let temp_root = title_dir.join("temp");
        Self::new(game_root, &save_root, &temp_root, save_quota_mb)
    }

    pub fn mount_root(&self, mount: VfsMount) -> &Path {
        match mount {
            VfsMount::Game => &self.game_root,
            VfsMount::Save => &self.save_root,
            VfsMount::Temp => &self.temp_root,
        }
    }

    pub const fn save_quota_bytes(&self) -> Option<u64> {
        self.save_quota_bytes
    }

    /// Résout un chemin invité (`game:/...`, `save:/...`, `temp:/...`, `G:\...`, `S:\...`, `T:\...`
    /// ou relatif à la racine du jeu) en chemin hôte garanti à l'intérieur du sandbox.
    pub fn resolve(&self, guest_path: &str) -> Result<ResolvedPath, VfsError> {
        if guest_path.is_empty() || guest_path.contains('\0') {
            return Err(VfsError::InvalidPath(guest_path.to_string()));
        }

        let trimmed = guest_path.trim();
        if trimmed.starts_with('/') || trimmed.starts_with("\\\\") || trimmed.starts_with("//") {
            return Err(VfsError::EscapeAttempt(guest_path.to_string()));
        }

        let (mount, remainder) = parse_guest_prefix(trimmed)?;
        let normalized = normalize_relative_path(remainder, guest_path)?;
        let root = self.mount_root(mount);
        let candidate = root.join(&normalized);

        // Vérification anti-symlink escape sur le plus long préfixe existant.
        verify_within_root(root, &candidate, guest_path)?;

        Ok(ResolvedPath {
            mount,
            host_path: candidate,
            relative_path: normalized,
        })
    }

    /// Calcule la taille totale occupée par les sauvegardes (`save_root`).
    pub fn save_usage_bytes(&self) -> Result<u64, VfsError> {
        dir_size_bytes(&self.save_root)
    }

    /// Vérifie qu'une écriture supplémentaire respecte le quota `PersistentLocalStorage`.
    pub fn check_save_quota(&self, additional_bytes: u64) -> Result<(), VfsError> {
        let Some(limit) = self.save_quota_bytes else {
            return Ok(());
        };
        let used = self.save_usage_bytes()?;
        if used.saturating_add(additional_bytes) > limit {
            return Err(VfsError::SaveQuotaExceeded {
                used,
                requested: additional_bytes,
                limit,
            });
        }
        Ok(())
    }
}

fn parse_guest_prefix(input: &str) -> Result<(VfsMount, &str), VfsError> {
    let lower = input.to_ascii_lowercase();

    // Montages nommés `GAME:` / `SAVE:` / `TEMP:` (insensibles à la casse), suivis d'un séparateur
    // `/` ou `\` (forme Windows / Xbox : `GAME:\Media\x.bin`) ou de rien (racine du montage).
    for (name, mount) in [
        ("game:", VfsMount::Game),
        ("save:", VfsMount::Save),
        ("temp:", VfsMount::Temp),
    ] {
        if lower.starts_with(name) {
            // Les préfixes sont ASCII : `lower` et `input` ont les mêmes offsets d'octets.
            let rest = &input[name.len()..];
            if rest.is_empty() || rest.starts_with(['/', '\\']) {
                return Ok((mount, rest.trim_start_matches(['/', '\\'])));
            }
        }
    }

    // Lecteurs virtuels Windows / Xbox : G:\, S:\, T:\ (tout autre lecteur comme C:\ ou Z:\ est refusé).
    let bytes = input.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let drive = (bytes[0] as char).to_ascii_uppercase();
        let rest = input[2..].trim_start_matches(['\\', '/']);
        return match drive {
            'G' => Ok((VfsMount::Game, rest)),
            'S' => Ok((VfsMount::Save, rest)),
            'T' => Ok((VfsMount::Temp, rest)),
            _ => Err(VfsError::UnsupportedPrefix(input.to_string())),
        };
    }

    if input.contains("://") {
        return Err(VfsError::UnsupportedPrefix(input.to_string()));
    }

    Ok((VfsMount::Game, input))
}

fn normalize_relative_path(raw: &str, original: &str) -> Result<PathBuf, VfsError> {
    let unified = raw.replace('\\', "/");
    let mut parts: Vec<String> = Vec::new();

    for comp in Path::new(&unified).components() {
        match comp {
            Component::Normal(seg) => {
                let s = seg.to_string_lossy();
                if s.contains(':') {
                    return Err(VfsError::EscapeAttempt(original.to_string()));
                }
                parts.push(s.into_owned());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if parts.pop().is_none() {
                    return Err(VfsError::EscapeAttempt(original.to_string()));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(VfsError::EscapeAttempt(original.to_string()));
            }
        }
    }

    let mut out = PathBuf::new();
    for p in parts {
        out.push(p);
    }
    Ok(out)
}

fn verify_within_root(root: &Path, candidate: &Path, original: &str) -> Result<(), VfsError> {
    // Remonte jusqu'au premier ancêtre existant pour le canonicaliser et détecter tout symlink sortant.
    let mut probe = candidate.to_path_buf();
    loop {
        if probe.exists() {
            let canonical = fs::canonicalize(&probe).map_err(|source| VfsError::Io {
                path: probe.clone(),
                source,
            })?;
            if !canonical.starts_with(root) {
                return Err(VfsError::EscapeAttempt(original.to_string()));
            }
            return Ok(());
        }
        if !probe.pop() {
            return Err(VfsError::EscapeAttempt(original.to_string()));
        }
    }
}

fn dir_size_bytes(dir: &Path) -> Result<u64, VfsError> {
    let mut total = 0u64;
    let entries = fs::read_dir(dir).map_err(|source| VfsError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    for entry in entries {
        let entry = entry.map_err(|source| VfsError::Io {
            path: dir.to_path_buf(),
            source,
        })?;
        let meta = entry.metadata().map_err(|source| VfsError::Io {
            path: entry.path(),
            source,
        })?;
        if meta.is_file() {
            total = total.saturating_add(meta.len());
        } else if meta.is_dir() {
            total = total.saturating_add(dir_size_bytes(&entry.path())?);
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn make_temp_test_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("scarletbridge_vfs_{tag}_{nanos}"));
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    #[test]
    fn resolves_valid_mounts_and_blocks_escapes() {
        let base = make_temp_test_dir("mounts");
        let game = base.join("game");
        let state = base.join("state");
        fs::create_dir_all(game.join("assets/shaders")).expect("create assets");

        let vfs = VirtualFs::new_sandboxed(&game, &state, "7A1B2C3D", Some(1)).expect("init vfs");

        let g = vfs
            .resolve(r"G:\assets\shaders\tri.dxil")
            .expect("resolve game");
        assert_eq!(g.mount, VfsMount::Game);
        assert!(g.host_path.starts_with(vfs.mount_root(VfsMount::Game)));

        let s = vfs
            .resolve("save:/slot0/profile.json")
            .expect("resolve save");
        assert_eq!(s.mount, VfsMount::Save);
        assert!(s.host_path.starts_with(vfs.mount_root(VfsMount::Save)));

        let t = vfs.resolve(r"T:\cache\pipeline.bin").expect("resolve temp");
        assert_eq!(t.mount, VfsMount::Temp);
        assert!(t.host_path.starts_with(vfs.mount_root(VfsMount::Temp)));

        // Tentatives d'évasion interdites.
        assert!(vfs.resolve("/etc/passwd").is_err());
        assert!(vfs.resolve("/home/user/.ssh/id_rsa").is_err());
        assert!(vfs.resolve(r"Z:\home\user\secret").is_err());
        assert!(vfs.resolve(r"C:\Windows\System32\cmd.exe").is_err());
        assert!(vfs.resolve(r"G:\assets\..\..\outside.txt").is_err());
        assert!(vfs.resolve("save:/../other_title/save.dat").is_err());

        let _ = fs::remove_dir_all(&base);
    }

    /// Les chemins Windows / Xbox `GAME:\...` (barre inverse, casse libre) sont résolus.
    #[test]
    fn resolves_named_mounts_with_backslashes_and_any_case() {
        let base = make_temp_test_dir("named_mounts");
        let game = base.join("game");
        let state = base.join("state");
        fs::create_dir_all(game.join("Media")).expect("create media");

        let vfs = VirtualFs::new_sandboxed(&game, &state, "NAMED", None).expect("init vfs");

        for (path, mount, rel) in [
            (r"GAME:\Media\a.bin", VfsMount::Game, "Media/a.bin"),
            (r"game:\Media/a.bin", VfsMount::Game, "Media/a.bin"),
            ("Game:/Media/a.bin", VfsMount::Game, "Media/a.bin"),
            (r"SAVE:\slot0\p.dat", VfsMount::Save, "slot0/p.dat"),
            (r"Temp:\c\x", VfsMount::Temp, "c/x"),
            ("GAME:", VfsMount::Game, ""),
            (r"GAME:\", VfsMount::Game, ""),
        ] {
            let r = vfs.resolve(path).unwrap_or_else(|e| panic!("{path}: {e}"));
            assert_eq!(r.mount, mount, "{path}");
            assert_eq!(r.relative_path, PathBuf::from(rel), "{path}");
            assert!(r.host_path.starts_with(vfs.mount_root(mount)), "{path}");
        }

        // Évasions toujours refusées, quelle que soit la forme du préfixe.
        for bad in [
            r"GAME:\..\x",
            r"game:\Media\..\..\x",
            r"SAVE:\..\other\s",
            r"gamex:\a",
            r"GAME:\C:\Windows",
        ] {
            assert!(vfs.resolve(bad).is_err(), "{bad} devrait être refusé");
        }

        // Des séparateurs multiples ne sortent pas du montage : le chemin reste sous la racine.
        let r = vfs
            .resolve("GAME://server/share")
            .expect("reste sous GAME:");
        assert!(r.host_path.starts_with(vfs.mount_root(VfsMount::Game)));

        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn blocks_symlink_escape() {
        use std::os::unix::fs::symlink;

        let base = make_temp_test_dir("symlink");
        let game = base.join("game");
        let state = base.join("state");
        let outside = base.join("outside");
        fs::create_dir_all(&game).expect("create game");
        fs::create_dir_all(&outside).expect("create outside");
        symlink(&outside, game.join("evil_link")).expect("create symlink");

        let vfs = VirtualFs::new_sandboxed(&game, &state, "SYMLINK_TEST", None).expect("init vfs");
        let err = vfs.resolve("game:/evil_link/secret.txt");
        assert!(matches!(err, Err(VfsError::EscapeAttempt(_))));

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn enforces_save_quota() {
        let base = make_temp_test_dir("quota");
        let game = base.join("game");
        let state = base.join("state");
        fs::create_dir_all(&game).expect("create game");

        let vfs = VirtualFs::new_sandboxed(&game, &state, "QUOTA", Some(1)).expect("init vfs");
        assert!(vfs.check_save_quota(512 * 1024).is_ok());
        assert!(matches!(
            vfs.check_save_quota(2 * 1024 * 1024),
            Err(VfsError::SaveQuotaExceeded { .. })
        ));

        let _ = fs::remove_dir_all(&base);
    }
}
