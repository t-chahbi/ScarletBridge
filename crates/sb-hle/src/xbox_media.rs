//! Médias Xbox / GDK en HLE : vidéo Bink 2 (`bink2scarlett`, `bink2w64`), audio matériel Xbox
//! émulé en logiciel (`acphal`, `xapu`, `chad`) et Media Foundation (`mfplat`, `mfreadwrite`).
//!
//! Les signatures proviennent de la documentation publique (GDK learn.microsoft.com/gaming,
//! MSDN Media Foundation, `bink.h` public de RAD) ; toute signature *déduite* est signalée
//! dans le sous-module concerné. Quand aucun équivalent n'existe (codec Bink 2 propriétaire,
//! puce audio Xbox), le comportement est une émulation logicielle cohérente ou un échec
//! documenté propre — jamais un faux succès sans écriture des sorties.

use std::collections::HashMap;

mod apu;
mod bink;
mod mf;

/// Exports par nom, indexés par *stem* de DLL en minuscules sans `.dll` (ex. `"ws2_32"`).
/// Prioritaires sur les familles historiques ; un stem listé ici est considéré HLE.
pub fn by_dll() -> HashMap<&'static str, HashMap<&'static str, usize>> {
    let mut m = HashMap::new();
    let bink = bink::exports();
    m.insert("bink2scarlett", bink.clone());
    m.insert("bink2w64", bink);
    m.insert("acphal", apu::acphal_exports());
    m.insert("xapu", apu::xapu_exports());
    m.insert("chad", apu::chad_exports());
    m.insert("mfplat", mf::mfplat_exports());
    m.insert("mfreadwrite", mf::mfreadwrite_exports());
    m
}

/// Exports par ordinal, indexés par *stem* de DLL (aucun ordinal public pour ces modules).
pub fn ordinals_by_dll() -> HashMap<&'static str, HashMap<u16, usize>> {
    HashMap::new()
}

/// Résolution d'un chemin invité via le VFS sandboxé du processus (jamais `/home`, pas de `..`).
pub(crate) fn resolve_guest_path(guest: &str) -> Option<std::path::PathBuf> {
    #[cfg(test)]
    if let Some(p) = tests_vfs::resolve(guest) {
        return p;
    }
    let st = crate::process::process_state().read();
    let vfs = st.vfs.as_ref()?;
    match vfs.resolve(guest) {
        Ok(r) => Some(r.host_path),
        Err(err) => {
            sb_common::sb_debug!("xbox_media", "chemin '{guest}' refusé par le VFS : {err}");
            None
        }
    }
}

/// Plausibilité d'un pointeur invité (rejette NULL et les petits entiers passés à la place
/// d'un pointeur lorsqu'une signature est déduite).
pub(crate) fn plausible_ptr<T>(p: *const T) -> bool {
    (p as usize) >= 0x1_0000
}

#[cfg(test)]
pub(crate) mod tests_vfs {
    //! VFS de test propre à chaque thread de test (évite les interférences entre tests parallèles).
    use std::cell::RefCell;
    use std::path::PathBuf;

    use sb_vfs::VirtualFs;

    thread_local! {
        static VFS: RefCell<Option<VirtualFs>> = const { RefCell::new(None) };
    }

    /// Crée un VFS temporaire dont la racine jeu est `game_dir` (créé si absent).
    pub fn install(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "sb-xbox-media-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let game = base.join("game");
        std::fs::create_dir_all(&game).expect("création racine jeu");
        let vfs =
            VirtualFs::new_sandboxed(&game, &base.join("state"), "XMEDIA", None).expect("init vfs");
        let root = vfs.mount_root(sb_vfs::VfsMount::Game).to_path_buf();
        VFS.with(|v| *v.borrow_mut() = Some(vfs));
        root
    }

    pub fn resolve(guest: &str) -> Option<Option<PathBuf>> {
        VFS.with(|v| {
            v.borrow()
                .as_ref()
                .map(|vfs| vfs.resolve(guest).ok().map(|r| r.host_path))
        })
    }
}
