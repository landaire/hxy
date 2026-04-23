use serde::Deserialize;
use serde::Serialize;

/// What a mounted VFS can do. Plugins advertise these so the UI knows
/// whether to expose edit / create / delete affordances. Read-only
/// plugins simply leave `write` and `grow` as false.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VfsCapabilities {
    /// Plugin can read entry contents. Always true in practice — a
    /// handler that can't read wouldn't be useful — but kept explicit
    /// so the struct is future-proof for formats that distinguish
    /// readable vs. metadata-only.
    pub read: bool,
    /// Plugin can overwrite bytes inside an existing entry.
    pub write: bool,
    /// Plugin can create new entries or delete existing ones.
    pub grow: bool,
}

impl VfsCapabilities {
    pub const READ_ONLY: Self = Self { read: true, write: false, grow: false };
    pub const READ_WRITE: Self = Self { read: true, write: true, grow: true };
}
