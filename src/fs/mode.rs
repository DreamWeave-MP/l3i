//! Unix permission bits: `fs.chmod`, the `mode` a call gives what it creates, and the `mode` a
//! `stat` reports.
//!
//! A `mode` is exact, and nobody but the owner is given more on the way there. `open(2)` and
//! `mkdir(2)` are handed the bits, so the process umask can only narrow what they create; the bits
//! are then set through the handle just opened, which undoes the umask and nothing else. A file
//! meant for its owner alone is at no moment readable by anyone else, and what it holds is written
//! after.
//!
//! A file's mode is its nine permission bits and no more: the system clears set-user-ID and
//! set-group-ID from a file as it is written, so a mode carrying them could not be kept. A
//! directory's may carry those and sticky, and `fs.chmod` sets any of them on anything.
//!
//! A `mode` is for what the call creates. A file that was already there keeps its own bits, as it
//! does under `open(2)`: `fs.chmod` changes those.
//!
//! Only Unix has these bits. Elsewhere a `mode` is read and checked and then changes nothing,
//! `fs.chmod` answers `unsupported`, and a `stat` has no `mode`.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use crate::convert::Exact;
use crate::error::{Error, Result};

/// The nine permission bits: all a file created with a mode may carry.
const PERMISSIONS: u32 = 0o777;
/// Every bit a mode has: the permission bits, set-user-ID, set-group-ID and sticky.
const EVERY_BIT: u32 = 0o7777;

fn checked(what: &str, Exact(mode): Exact<i64>, most: u32, kind: &str) -> Result<u32> {
    match u32::try_from(mode) {
        Ok(bits) if bits <= most => Ok(bits),
        _ => Err(Error::runtime(format!("dream.fs.{what}: mode {mode} is not {kind} (0 to {most})"))),
    }
}

/// The `mode` option of a call that creates a file: `None` when the caller gave none.
pub(crate) fn of_file(what: &str, mode: Option<Exact<i64>>) -> Result<Option<u32>> {
    mode.map(|mode| checked(what, mode, PERMISSIONS, "a file's permission bits")).transpose()
}

/// The `mode` option of a call that creates directories: `None` when the caller gave none.
pub(crate) fn of_directory(what: &str, mode: Option<Exact<i64>>) -> Result<Option<u32>> {
    mode.map(|mode| any(what, mode)).transpose()
}

/// The mode `fs.chmod` is handed.
pub(crate) fn any(what: &str, mode: Exact<i64>) -> Result<u32> {
    checked(what, mode, EVERY_BIT, "mode bits")
}

/// The bits a `stat` reports: none, where there are none.
#[cfg_attr(unix, allow(clippy::unnecessary_wraps))] // Only Unix always has an answer.
pub(crate) fn of(metadata: &std::fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(metadata.permissions().mode() & EVERY_BIT)
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

/// `fs.chmod`: sets the bits of what `path` names, following symbolic links.
pub(crate) fn change(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Err(io::Error::new(io::ErrorKind::Unsupported, "permission bits are unsupported on this platform"))
    }
}

/// How the file that replaces `old` gets its permissions: the mode to create it with, and what to
/// set on it once it is written. A `mode` decides; without one the old file's permissions carry
/// over. On Unix its permission bits are there from creation, and only bits beyond them wait for
/// the data, which would clear them.
pub(crate) fn replacing(
    mode: Option<u32>,
    old: Option<&std::fs::Metadata>,
) -> (Option<u32>, Option<std::fs::Permissions>) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Some(kept) = old.and_then(of).filter(|_| mode.is_none()) else {
            return (mode, None);
        };
        let after = (kept & !PERMISSIONS != 0).then(|| std::fs::Permissions::from_mode(kept));
        (Some(kept & PERMISSIONS), after)
    }
    #[cfg(not(unix))]
    (mode, old.map(std::fs::Metadata::permissions))
}

/// Whether an open may create the file it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Create {
    /// The file must be there.
    Never,
    /// The file is made if it is not there.
    Missing,
    /// The file must not be there.
    New,
}

impl Create {
    /// What `create` and `exclusive` options ask for between them.
    pub(crate) const fn from_options(create: bool, exclusive: bool) -> Create {
        match (create, exclusive) {
            (_, true) => Create::New,
            (true, false) => Create::Missing,
            (false, false) => Create::Never,
        }
    }
}

/// Opens `path` as `base` says, which sets everything but whether the file is created. A file
/// this creates has exactly `mode` when one is given.
pub(crate) fn open(base: &OpenOptions, path: &Path, create: Create, mode: Option<u32>) -> io::Result<File> {
    #[cfg(unix)]
    if let Some(mode) = mode
        && create != Create::Never
    {
        return open_exact(base, path, create == Create::New, mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut options = base.clone();
    match create {
        Create::Never => {}
        Create::Missing => drop(options.create(true)),
        Create::New => drop(options.create_new(true)),
    }
    options.open(path)
}

/// How many times an open that may create looks for a file that is there for one call and gone
/// for the next. A name that keeps doing that is a dangling symbolic link, which is refused: a
/// file with a mode is not made through one.
#[cfg(unix)]
const ROUNDS: usize = 3;

/// Only an exclusive create says whether this call made the file, so that is always what creates
/// it; a file that turns out to be there is then opened as it is, its bits left alone. A file
/// whose bits cannot be set is not left behind with others.
#[cfg(unix)]
fn open_exact(base: &OpenOptions, path: &Path, new_only: bool, mode: u32) -> io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut creating = base.clone();
    creating.create_new(true).mode(mode);
    let mut exists = io::Error::from(io::ErrorKind::AlreadyExists);
    for _ in 0..ROUNDS {
        match creating.open(path) {
            Ok(file) => {
                if let Err(error) = file.set_permissions(std::fs::Permissions::from_mode(mode)) {
                    drop(file);
                    let _ = std::fs::remove_file(path);
                    return Err(error);
                }
                return Ok(file);
            }
            Err(error) if !new_only && error.kind() == io::ErrorKind::AlreadyExists => exists = error,
            Err(error) => return Err(error),
        }
        match base.open(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            opened => return opened,
        }
    }
    Err(exists)
}

/// Creates the directory `path`, and with `recursive` its missing parents, accepting one that is
/// there. With a `mode`, `path` has exactly that when this creates it, and a parent this creates
/// has it with the owner's write and search bits, without which nothing could be made inside.
pub(crate) fn create_dir(path: &Path, recursive: bool, mode: Option<u32>) -> io::Result<()> {
    #[cfg(unix)]
    if let Some(mode) = mode {
        return if recursive { create_dir_all_exact(path, mode) } else { create_dir_exact(path, mode) };
    }
    #[cfg(not(unix))]
    let _ = mode;
    if recursive { std::fs::create_dir_all(path) } else { std::fs::create_dir(path) }
}

/// The owner's read bit, which a directory is made with so that it can be opened to have its bits
/// set, and the write and search bits a parent needs for something to be made inside it.
#[cfg(unix)]
const OWNER_READ: u32 = 0o400;
#[cfg(unix)]
const OWNER_WRITE_SEARCH: u32 = 0o300;

/// Makes one directory with exactly `mode`. One whose bits cannot be set is removed again.
#[cfg(unix)]
fn create_dir_exact(path: &Path, mode: u32) -> io::Result<()> {
    make_dir(path, mode)?;
    set_dir(path, mode).inspect_err(|_| drop(std::fs::remove_dir(path)))
}

#[cfg(unix)]
fn make_dir(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(mode | OWNER_READ).create(path)
}

/// `std::fs::create_dir_all`, with the mode: a directory that is there, or that another process
/// makes first, is accepted and keeps its bits. Only a directory that could not be made is
/// forgiven for being there; one whose bits could not be set is an error.
#[cfg(unix)]
fn create_dir_all_exact(path: &Path, mode: u32) -> io::Result<()> {
    fn all(path: &Path, mode: u32, leaf: u32) -> io::Result<()> {
        if path.as_os_str().is_empty() {
            return Ok(());
        }
        match make_dir(path, leaf) {
            Ok(()) => return set_dir(path, leaf).inspect_err(|_| drop(std::fs::remove_dir(path))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(_) if path.is_dir() => return Ok(()),
            Err(error) => return Err(error),
        }
        let Some(parent) = path.parent() else {
            return Err(io::Error::other("failed to create the whole directory tree"));
        };
        all(parent, mode, mode | OWNER_WRITE_SEARCH)?;
        match make_dir(path, leaf) {
            Ok(()) => set_dir(path, leaf).inspect_err(|_| drop(std::fs::remove_dir(path))),
            Err(_) if path.is_dir() => Ok(()),
            Err(error) => Err(error),
        }
    }
    all(path, mode, mode)
}

/// Undoes the umask on a directory this call just made, and the owner's read bit it was made
/// with, through a handle that is never a symbolic link put in its place. The set-group-ID bit a
/// directory takes from its parent is not a permission and stays.
#[cfg(unix)]
fn set_dir(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    /// `S_ISGID`, which POSIX fixes and whose type differs between systems.
    const SET_GROUP_ID: u32 = 0o2000;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let inherited = directory.metadata()?.mode() & SET_GROUP_ID;
    directory.set_permissions(std::fs::Permissions::from_mode(mode | inherited))
}
