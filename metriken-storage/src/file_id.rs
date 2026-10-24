//! Which file is at a path, so a reader opened from a path can tell when
//! another file has been put there.

use std::path::Path;

/// The device and inode of a file. A file renamed over another, as
/// `recording filter` does to its input, has a different one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileId(u64, u64);

impl FileId {
    /// The id of the file at `path`; `None` when it cannot be read, and on a
    /// platform without inodes.
    pub fn of(path: &Path) -> Option<FileId> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let m = std::fs::metadata(path).ok()?;
            Some(FileId(m.dev(), m.ino()))
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            None
        }
    }
}

/// Run `open` on the file at `path`, and return its result with the file's
/// id when the file at `path` was the same before and after the open; `None`
/// when it changed, or cannot be told. A reader opened from a path reads its
/// tables from the path again on their first query, and refuses to once
/// another file is there (see `Container::reopen`), so this is the only file
/// the reader reads.
pub fn open_file<T>(path: &Path, open: impl FnOnce() -> T) -> (T, Option<FileId>) {
    let before = FileId::of(path);
    let opened = open();
    let after = FileId::of(path);
    (opened, before.filter(|b| Some(*b) == after))
}
