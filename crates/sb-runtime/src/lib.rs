//! Services runtime GameOS locaux (Phase 2) :
//! - Profil local offline (`XUser*`),
//! - Succès locaux persistés en JSON,
//! - Machine à états de cycle de vie console (focus, suspend/resume, mode offline strict).

pub mod lifecycle;
pub mod profile;
