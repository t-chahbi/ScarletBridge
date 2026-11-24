//! Lecture des métadonnées de gestion d'exceptions C++ MSVC x64 : `FuncInfo` (FH3, tables
//! `ehdata.h` avec RVA relatives à l'ImageBase) et `FuncInfo4` (FH4, VS2019+ : champs compressés).
//!
//! Aucune allocation : les tables sont décodées à la demande depuis la mémoire de l'image. Le
//! dispatcher abandonne des trames Rust sans exécuter leurs destructeurs (`RtlRestoreContext`),
//! toute allocation tenue par ces trames serait perdue.
//!
//! **Sources** : `ehdata.h` / `ehdata_values.h` du SDK (FH3), `dlls/msvcrt/cxx.h` et
//! `except_x86_64.c` du projet Wine, documentation Microsoft « x64 exception handling ».
//! **Inférence** : le format FH4 n'est pas documenté officiellement ; le décodage ci-dessous suit
//! la reconstruction publique (entiers à longueur variable par bits de poids faible, en-tête
//! d'octets de drapeaux, états = décalages dans la table de déroulage). Il n'a été validé que par
//! des tables synthétiques produites par l'encodeur de test (`cxx/tests.rs`) : toute divergence
//! avec un compilateur réel est à corriger ici, sans toucher à l'algorithme du handler.

use core::ptr::read_unaligned;

/// Lecture non alignée de la mémoire de l'image.
///
/// # Safety
/// `addr` doit être lisible.
#[inline]
pub(super) unsafe fn rd_u8(addr: usize) -> u8 {
    // SAFETY: contrat de l'appelant.
    unsafe { read_unaligned(addr as *const u8) }
}

/// # Safety
/// `addr` doit être lisible sur 4 octets.
#[inline]
pub(super) unsafe fn rd_i32(addr: usize) -> i32 {
    // SAFETY: contrat de l'appelant.
    unsafe { read_unaligned(addr as *const i32) }
}

/// # Safety
/// `addr` doit être lisible sur 4 octets.
#[inline]
pub(super) unsafe fn rd_u32(addr: usize) -> u32 {
    // SAFETY: contrat de l'appelant.
    unsafe { read_unaligned(addr as *const u32) }
}

/// Entrée de la table de déroulage.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnwindEntry {
    /// État parent (-1 : aucun).
    pub to_state: i32,
    /// RVA du funclet de destruction (0 : aucune action).
    pub action: u32,
}

/// Bloc catch (`HandlerType`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Handler {
    pub adjectives: u32,
    /// RVA du `TypeDescriptor` (0 : `catch (...)`).
    pub type_rva: i32,
    /// Déplacement de la variable d'exception dans la trame (0 : aucune).
    pub catch_obj: i32,
    /// RVA du funclet catch.
    pub handler_rva: u32,
    /// Déplacement (dans la trame du funclet) du pointeur vers la trame parente.
    pub frame_disp: i32,
    /// FH4 : adresses de continuation (RVA), 0 entrée en FH3 (le funclet renvoie l'adresse).
    pub cont: [u32; 2],
    pub cont_count: u8,
}

pub const HT_CONST: u32 = 0x01;
pub const HT_VOLATILE: u32 = 0x02;
pub const HT_REFERENCE: u32 = 0x08;

/// Bloc `try` (`TryBlockMapEntry`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TryBlock {
    pub low: i32,
    pub high: i32,
    pub catch_high: i32,
    pub handler_count: u32,
    /// Adresse du tableau de handlers (format dépendant de la version).
    pub handlers: usize,
}

/// Métadonnées d'une fonction (`FuncInfo` / `FuncInfo4`).
#[derive(Clone, Copy)]
pub enum FuncDesc {
    V3 { base: usize, info: usize },
    V4(Fh4),
}

/// `FuncInfo4` décodé (seulement les en-têtes ; les tables restent compressées en mémoire).
#[derive(Clone, Copy)]
pub struct Fh4 {
    pub base: usize,
    pub header: u8,
    pub unwind_map: usize,
    pub try_map: usize,
    pub ip2state: usize,
    pub frame_offset: i32,
}

const FH4_IS_CATCH: u8 = 0x01;
const FH4_IS_SEPARATED: u8 = 0x02;
const FH4_BBT: u8 = 0x04;
const FH4_UNWIND_MAP: u8 = 0x08;
const FH4_TRY_MAP: u8 = 0x10;
const FH4_EHS: u8 = 0x20;

/// Flux d'octets avec les entiers compressés FH4.
pub struct Cursor {
    pub pos: usize,
}

impl Cursor {
    /// `ReadUnsigned` : la longueur (1 à 5 octets) est donnée par le nombre de bits à 1 de poids
    /// faible du premier octet ; 5 octets : valeur brute sur les 4 octets suivants.
    ///
    /// # Safety
    /// La mémoire lue doit être valide.
    pub unsafe fn uint(&mut self) -> u32 {
        // SAFETY: contrat de l'appelant.
        unsafe {
            let b = u32::from(rd_u8(self.pos));
            if b & 1 == 0 {
                self.pos += 1;
                b >> 1
            } else if b & 3 == 1 {
                let v = u32::from(rd_u8(self.pos)) | u32::from(rd_u8(self.pos + 1)) << 8;
                self.pos += 2;
                v >> 2
            } else if b & 7 == 3 {
                let v = u32::from(rd_u8(self.pos))
                    | u32::from(rd_u8(self.pos + 1)) << 8
                    | u32::from(rd_u8(self.pos + 2)) << 16;
                self.pos += 3;
                v >> 3
            } else if b & 15 == 7 {
                let v = rd_u32(self.pos);
                self.pos += 4;
                v >> 4
            } else {
                let v = rd_u32(self.pos + 1);
                self.pos += 5;
                v
            }
        }
    }

    /// Entier signé (le 5ᵉ cas porte la valeur complément à deux sur 32 bits).
    ///
    /// # Safety
    /// Voir [`Cursor::uint`].
    pub unsafe fn int(&mut self) -> i32 {
        // SAFETY: contrat de l'appelant.
        unsafe { self.uint() as i32 }
    }

    /// RVA fixe de 4 octets.
    ///
    /// # Safety
    /// Voir [`Cursor::uint`].
    pub unsafe fn rva(&mut self) -> u32 {
        // SAFETY: contrat de l'appelant.
        let v = unsafe { rd_u32(self.pos) };
        self.pos += 4;
        v
    }

    /// # Safety
    /// Voir [`Cursor::uint`].
    pub unsafe fn byte(&mut self) -> u8 {
        // SAFETY: contrat de l'appelant.
        let v = unsafe { rd_u8(self.pos) };
        self.pos += 1;
        v
    }
}

impl FuncDesc {
    /// FH3 : `info` = adresse du `FuncInfo`.
    pub fn v3(base: usize, info: usize) -> Self {
        Self::V3 { base, info }
    }

    /// FH4 : décode l'en-tête de `FuncInfo4` situé à `info`.
    ///
    /// # Safety
    /// `info` doit désigner un `FuncInfo4` valide de l'image `base`.
    pub unsafe fn v4(base: usize, info: usize) -> Self {
        let mut c = Cursor { pos: info };
        // SAFETY: contrat de l'appelant.
        unsafe {
            let header = c.byte();
            if header & FH4_BBT != 0 {
                let _bbt = c.uint();
            }
            let unwind_map = if header & FH4_UNWIND_MAP != 0 {
                base + c.rva() as usize
            } else {
                0
            };
            let try_map = if header & FH4_TRY_MAP != 0 {
                base + c.rva() as usize
            } else {
                0
            };
            if header & FH4_IS_SEPARATED != 0 {
                let _sep = c.rva();
            }
            let ip2state = base + c.rva() as usize;
            let frame_offset = if header & FH4_IS_CATCH != 0 {
                c.int()
            } else {
                0
            };
            Self::V4(Fh4 {
                base,
                header,
                unwind_map,
                try_map,
                ip2state,
                frame_offset,
            })
        }
    }

    /// `/EHs` : les exceptions non C++ ne sont pas attrapées par `catch (...)`.
    ///
    /// # Safety
    /// Métadonnées valides.
    pub unsafe fn synchronous_only(&self) -> bool {
        match self {
            // SAFETY: `EHFlags` à l'offset 36 de `FuncInfo`.
            Self::V3 { info, .. } => (unsafe { rd_u32(info + 36) } & 1) != 0,
            Self::V4(f) => f.header & FH4_EHS != 0,
        }
    }

    /// Nombre d'états (FH3) ; en FH4 : 1 si une table de déroulage existe, sinon 0.
    ///
    /// # Safety
    /// Métadonnées valides.
    pub unsafe fn max_state(&self) -> i32 {
        match self {
            // SAFETY: `maxState` à l'offset 4.
            Self::V3 { info, .. } => unsafe { rd_i32(info + 4) },
            Self::V4(f) => i32::from(f.unwind_map != 0),
        }
    }

    /// État à `rva` (`IPtoStateMap`) : dernière entrée dont l'IP ≤ `rva`, -1 avant la première.
    ///
    /// # Safety
    /// Métadonnées valides.
    pub unsafe fn ip_to_state(&self, rva: u32) -> i32 {
        // SAFETY: contrat de l'appelant (tables de l'image).
        unsafe {
            match self {
                Self::V3 { base, info } => {
                    let n = rd_u32(info + 20) as usize;
                    let map = base + rd_u32(info + 24) as usize;
                    let mut state = -1;
                    for i in 0..n {
                        let e = map + i * 8;
                        if rd_u32(e) > rva {
                            break;
                        }
                        state = rd_i32(e + 4);
                    }
                    state
                }
                Self::V4(f) => {
                    let mut c = Cursor { pos: f.ip2state };
                    let n = c.uint();
                    let mut ip = 0u32;
                    let mut state = -1;
                    for _ in 0..n {
                        ip = ip.wrapping_add(c.uint());
                        let s = c.uint() as i32 - 1;
                        if ip > rva {
                            break;
                        }
                        state = s;
                    }
                    state
                }
            }
        }
    }

    /// Entrée de déroulage de `state` (`None` pour -1 ou un état hors table).
    ///
    /// # Safety
    /// Métadonnées valides.
    pub unsafe fn unwind_entry(&self, state: i32) -> Option<UnwindEntry> {
        if state < 0 {
            return None;
        }
        // SAFETY: contrat de l'appelant.
        unsafe {
            match self {
                Self::V3 { base, info } => {
                    if state >= rd_i32(info + 4) {
                        return None;
                    }
                    let map = base + rd_u32(info + 8) as usize;
                    let e = map + state as usize * 8;
                    Some(UnwindEntry {
                        to_state: rd_i32(e),
                        action: rd_u32(e + 4),
                    })
                }
                Self::V4(f) => {
                    if f.unwind_map == 0 {
                        return None;
                    }
                    let mut c = Cursor {
                        pos: f.unwind_map + state as usize,
                    };
                    let v = c.uint();
                    let kind = v & 3;
                    let next = (v >> 2) as i32;
                    let to_state = if next == 0 { -1 } else { state - next };
                    let action = match kind {
                        0 => 0,
                        1 | 2 => {
                            let a = c.rva();
                            let _obj = c.uint();
                            a
                        }
                        _ => c.rva(),
                    };
                    Some(UnwindEntry { to_state, action })
                }
            }
        }
    }

    /// Nombre de blocs `try`.
    ///
    /// # Safety
    /// Métadonnées valides.
    pub unsafe fn try_count(&self) -> u32 {
        // SAFETY: contrat de l'appelant.
        unsafe {
            match self {
                Self::V3 { info, .. } => rd_u32(info + 12),
                Self::V4(f) => {
                    if f.try_map == 0 {
                        0
                    } else {
                        Cursor { pos: f.try_map }.uint()
                    }
                }
            }
        }
    }

    /// Bloc `try` n° `idx`.
    ///
    /// # Safety
    /// Métadonnées valides, `idx < try_count()`.
    pub unsafe fn try_block(&self, idx: u32) -> TryBlock {
        // SAFETY: contrat de l'appelant.
        unsafe {
            match self {
                Self::V3 { base, info } => {
                    let map = base + rd_u32(info + 16) as usize;
                    let e = map + idx as usize * 20;
                    TryBlock {
                        low: rd_i32(e),
                        high: rd_i32(e + 4),
                        catch_high: rd_i32(e + 8),
                        handler_count: rd_u32(e + 12),
                        handlers: base + rd_u32(e + 16) as usize,
                    }
                }
                Self::V4(f) => {
                    let mut c = Cursor { pos: f.try_map };
                    let n = c.uint();
                    let mut out = TryBlock {
                        low: 0,
                        high: 0,
                        catch_high: 0,
                        handler_count: 0,
                        handlers: 0,
                    };
                    for i in 0..n.min(idx + 1) {
                        let low = c.uint() as i32;
                        let high = low + c.uint() as i32;
                        let catch_high = high + c.uint() as i32;
                        let handlers = f.base + c.rva() as usize;
                        if i == idx {
                            let count = Cursor { pos: handlers }.uint();
                            out = TryBlock {
                                low,
                                high,
                                catch_high,
                                handler_count: count,
                                handlers,
                            };
                        }
                    }
                    out
                }
            }
        }
    }

    /// Handler n° `j` du bloc `tb`.
    ///
    /// # Safety
    /// Métadonnées valides, `j < tb.handler_count`.
    pub unsafe fn handler(&self, tb: &TryBlock, j: u32) -> Handler {
        // SAFETY: contrat de l'appelant.
        unsafe {
            match self {
                Self::V3 { .. } => {
                    let e = tb.handlers + j as usize * 20;
                    Handler {
                        adjectives: rd_u32(e),
                        type_rva: rd_i32(e + 4),
                        catch_obj: rd_i32(e + 8),
                        handler_rva: rd_u32(e + 12),
                        frame_disp: rd_i32(e + 16),
                        cont: [0; 2],
                        cont_count: 0,
                    }
                }
                Self::V4(f) => {
                    let mut c = Cursor { pos: tb.handlers };
                    let _count = c.uint();
                    let mut h = Handler {
                        adjectives: 0,
                        type_rva: 0,
                        catch_obj: 0,
                        handler_rva: 0,
                        frame_disp: 0,
                        cont: [0; 2],
                        cont_count: 0,
                    };
                    for i in 0..=j {
                        let header = c.byte();
                        h = Handler {
                            adjectives: 0,
                            type_rva: 0,
                            catch_obj: 0,
                            handler_rva: 0,
                            frame_disp: 0,
                            cont: [0; 2],
                            cont_count: 0,
                        };
                        if header & 0x01 != 0 {
                            h.adjectives = c.uint();
                        }
                        if header & 0x02 != 0 {
                            h.type_rva = c.rva() as i32;
                        }
                        if header & 0x04 != 0 {
                            h.catch_obj = c.uint() as i32;
                        }
                        h.handler_rva = c.rva();
                        let n = (header >> 4) & 3;
                        h.cont_count = n.min(2);
                        for slot in h.cont.iter_mut().take(usize::from(n.min(2))) {
                            *slot = if header & 0x08 != 0 {
                                c.rva()
                            } else {
                                // décalage depuis le début de la fonction : résolu par l'appelant
                                // (stocké tel quel, marqué par le bit de poids fort).
                                c.uint() | 0x8000_0000
                            };
                        }
                        let _ = (i, f);
                    }
                    h
                }
            }
        }
    }

    /// Déplacement du pointeur de trame parente dans un funclet (FH4 : `frame_offset`).
    pub fn funclet_frame_disp(&self, h: &Handler) -> i32 {
        match self {
            Self::V3 { .. } => h.frame_disp,
            Self::V4(f) => f.frame_offset,
        }
    }

    pub fn is_v4(&self) -> bool {
        matches!(self, Self::V4(_))
    }
}
