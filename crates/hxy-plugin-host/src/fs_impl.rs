//! `vfs::FileSystem` implementation that routes calls through a
//! plugin's `mount` resource. Read-only -- writes and metadata setters
//! return `NotSupported`.

use std::fmt;
use std::io::Cursor;
use std::time::SystemTime;

use vfs::FileSystem;
use vfs::SeekAndRead;
use vfs::SeekAndWrite;
use vfs::VfsError;
use vfs::VfsFileType;
use vfs::VfsMetadata;
use vfs::VfsResult;
use vfs::error::VfsErrorKind;

use crate::bindings::handler_world::exports::hxy::vfs::handler::FileType as WitFileType;
use crate::handler::PluginFileSystem;

impl fmt::Debug for PluginFileSystem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PluginFileSystem").field("plugin", &self.plugin_name).finish()
    }
}

impl FileSystem for PluginFileSystem {
    fn read_dir(&self, path: &str) -> VfsResult<Box<dyn Iterator<Item = String> + Send>> {
        let mut g = self.inner.lock().map_err(poisoned)?;
        let g = &mut *g;
        let mount_guest = g.plugin.hxy_vfs_handler().mount();
        let entries = mount_guest
            .call_read_dir(&mut g.store, g.mount, path)
            .map_err(|e| other(format!("plugin read-dir call trap: {e}")))?
            .map_err(|e| other(format!("plugin read-dir: {e}")))?;
        Ok(Box::new(entries.into_iter()))
    }

    fn create_dir(&self, _path: &str) -> VfsResult<()> {
        Err(VfsError::from(VfsErrorKind::NotSupported))
    }

    fn open_file(&self, path: &str) -> VfsResult<Box<dyn SeekAndRead + Send>> {
        let mut g = self.inner.lock().map_err(poisoned)?;
        let g = &mut *g;
        let mount_guest = g.plugin.hxy_vfs_handler().mount();
        let bytes = mount_guest
            .call_read_file(&mut g.store, g.mount, path)
            .map_err(|e| other(format!("plugin read-file call trap: {e}")))?
            .map_err(|e| other(format!("plugin read-file: {e}")))?;
        Ok(Box::new(Cursor::new(bytes)))
    }

    fn create_file(&self, _path: &str) -> VfsResult<Box<dyn SeekAndWrite + Send>> {
        Err(VfsError::from(VfsErrorKind::NotSupported))
    }

    fn append_file(&self, _path: &str) -> VfsResult<Box<dyn SeekAndWrite + Send>> {
        Err(VfsError::from(VfsErrorKind::NotSupported))
    }

    fn metadata(&self, path: &str) -> VfsResult<VfsMetadata> {
        let mut g = self.inner.lock().map_err(poisoned)?;
        let g = &mut *g;
        let mount_guest = g.plugin.hxy_vfs_handler().mount();
        let meta = mount_guest
            .call_metadata(&mut g.store, g.mount, path)
            .map_err(|e| other(format!("plugin metadata call trap: {e}")))?
            .map_err(|e| other(format!("plugin metadata: {e}")))?;
        Ok(VfsMetadata {
            file_type: match meta.file_type {
                WitFileType::RegularFile => VfsFileType::File,
                WitFileType::Directory => VfsFileType::Directory,
            },
            len: meta.length,
            created: None as Option<SystemTime>,
            modified: None,
            accessed: None,
        })
    }

    fn exists(&self, path: &str) -> VfsResult<bool> {
        match self.metadata(path) {
            Ok(_) => Ok(true),
            Err(e) => match e.kind() {
                VfsErrorKind::FileNotFound => Ok(false),
                _ => Ok(false),
            },
        }
    }

    fn remove_file(&self, _path: &str) -> VfsResult<()> {
        Err(VfsError::from(VfsErrorKind::NotSupported))
    }

    fn remove_dir(&self, _path: &str) -> VfsResult<()> {
        Err(VfsError::from(VfsErrorKind::NotSupported))
    }
}

fn other(msg: String) -> VfsError {
    VfsError::from(VfsErrorKind::Other(msg))
}

fn poisoned<T>(_e: std::sync::PoisonError<T>) -> VfsError {
    VfsError::from(VfsErrorKind::Other("plugin filesystem mutex poisoned".to_owned()))
}
