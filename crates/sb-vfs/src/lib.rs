//! Parseur de manifestes Xbox GDK (`MicrosoftGame.Config`, `appxmanifest.xml`)
//! et système de fichiers virtuel (VFS) strictement isolé.

pub mod manifest;
pub mod vfs;

pub use manifest::{AppManifest, ExecutableEntry, ManifestError, ManifestKind};
pub use vfs::{ResolvedPath, VfsError, VfsMount, VirtualFs};
