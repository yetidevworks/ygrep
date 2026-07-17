//! Read-only Tantivy directory wrapper (issue #12).
//!
//! Tantivy acquires `META_LOCK` — a lockfile created inside the index
//! directory — whenever an `IndexReader` loads searchers, even for pure read
//! access.  When the index directory is readable but not writable (e.g. a
//! sandboxed coding agent consuming a centrally-maintained index), creating
//! that lockfile fails with `PermissionDenied` and the index cannot be
//! searched at all.
//!
//! `ReadOnlyDirectory` delegates all I/O to an inner `MmapDirectory` but hands
//! out no-op locks.  This is safe for read-only consumers: the locks only
//! guard against a concurrent writer garbage-collecting segment files, and in
//! the worst case (an external writer deletes segments mid-open) the open
//! fails cleanly and can be retried.

use std::path::Path;
use std::sync::Arc;

use tantivy::directory::error::{DeleteError, LockError, OpenReadError, OpenWriteError};
use tantivy::directory::{
    Directory, DirectoryLock, FileHandle, Lock, MmapDirectory, WatchCallback, WatchHandle, WritePtr,
};

/// A `Directory` that reads through to an `MmapDirectory` but never takes
/// filesystem locks, so it works on index directories without write access.
#[derive(Clone, Debug)]
pub struct ReadOnlyDirectory {
    inner: MmapDirectory,
}

impl ReadOnlyDirectory {
    pub fn new(inner: MmapDirectory) -> Self {
        Self { inner }
    }
}

impl Directory for ReadOnlyDirectory {
    fn get_file_handle(&self, path: &Path) -> Result<Arc<dyn FileHandle>, OpenReadError> {
        self.inner.get_file_handle(path)
    }

    fn delete(&self, path: &Path) -> Result<(), DeleteError> {
        self.inner.delete(path)
    }

    fn exists(&self, path: &Path) -> Result<bool, OpenReadError> {
        self.inner.exists(path)
    }

    fn open_write(&self, path: &Path) -> Result<WritePtr, OpenWriteError> {
        self.inner.open_write(path)
    }

    fn atomic_read(&self, path: &Path) -> Result<Vec<u8>, OpenReadError> {
        self.inner.atomic_read(path)
    }

    fn atomic_write(&self, path: &Path, data: &[u8]) -> std::io::Result<()> {
        self.inner.atomic_write(path, data)
    }

    fn sync_directory(&self) -> std::io::Result<()> {
        Ok(())
    }

    fn watch(&self, watch_callback: WatchCallback) -> tantivy::Result<WatchHandle> {
        self.inner.watch(watch_callback)
    }

    fn acquire_lock(&self, _lock: &Lock) -> Result<DirectoryLock, LockError> {
        Ok(DirectoryLock::from(Box::new(())))
    }
}
