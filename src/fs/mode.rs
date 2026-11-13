//! Unix permission bits: `fs.chmod`, the `mode` a call gives what it creates, and the `mode` a
//! `stat` reports.
//!
//! A `mode` is exact, and it is never exceeded on the way there. `open(2)` and `mkdir(2)` are
//! handed the bits, so the process umask can only narrow what they create; the bits are then set
//! through the handle just opened, which undoes the umask and nothing else. A file meant for its
//! owner alone is at no moment readable by anyone else, and what it holds is written after.
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

/// Every bit a mode may carry: the nine permission bits, set-user-ID, set-group-ID and sticky.
const MODE_BITS: u32 = 0o7777;

/// A `mode` argument or option, checked: `None` when the caller gave none.
pub(crate) fn checked(what: &str, mode: Option<Exact<i64>>) -> Result<Option<u32>> {
    let Some(Exact(mode)) = mode else {
        return Ok(None);
    };
    match u32::try_from(mode) {
        Ok(bits) if bits <= MODE_BITS => Ok(Some(bits)),
        _ => Err(Error::runtime(format!("dream.fs.{what}: mode {mode} is not permission bits (0 to {MODE_BITS})"))),
    }
}

/// The bits a `stat` reports: none, where there are none.
#[cfg_attr(unix, allow(clippy::unnecessary_wraps))] // Only Unix always has an answer.
pub(crate) fn of(metadata: &std::fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(metadata.permissions().mode() & MODE_BITS)
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
/// it; a file that turns out to be there is then opened as it is, its bits left alone.
#[cfg(unix)]
fn open_exact(base: &OpenOptions, path: &Path, new_only: bool, mode: u32) -> io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut creating = base.clone();
    creating.create_new(true).mode(mode);
    let mut exists = io::Error::from(io::ErrorKind::AlreadyExists);
    for _ in 0..ROUNDS {
        match creating.open(path) {
            Ok(file) => {
                file.set_permissions(std::fs::Permissions::from_mode(mode))?;
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
/// there. Every directory this creates has exactly `mode` when one is given.
pub(crate) fn create_dir(path: &Path, recursive: bool, mode: Option<u32>) -> io::Result<()> {
    #[cfg(unix)]
    if let Some(mode) = mode {
        return if recursive { create_dir_all_exact(path, mode) } else { create_dir_exact(path, mode) };
    }
    #[cfg(not(unix))]
    let _ = mode;
    if recursive { std::fs::create_dir_all(path) } else { std::fs::create_dir(path) }
}

#[cfg(unix)]
fn create_dir_exact(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(mode).create(path)?;
    set_dir(path, mode)
}

/// `std::fs::create_dir_all`, with the mode: a directory another process makes first is accepted
/// and keeps the bits it was made with.
#[cfg(unix)]
fn create_dir_all_exact(path: &Path, mode: u32) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Ok(());
    }
    match create_dir_exact(path, mode) {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) if path.is_dir() => return Ok(()),
        Err(error) => return Err(error),
    }
    let Some(parent) = path.parent() else {
        return Err(io::Error::other("failed to create the whole directory tree"));
    };
    create_dir_all_exact(parent, mode)?;
    match create_dir_exact(path, mode) {
        Err(_) if path.is_dir() => Ok(()),
        made => made,
    }
}

/// Undoes the umask on a directory this call just made, never through a symbolic link put in its
/// place. The set-group-ID bit a directory takes from its parent is not a permission and stays.
#[cfg(unix)]
fn set_dir(path: &Path, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    /// `S_ISGID`, which POSIX fixes and whose type differs between systems.
    const SET_GROUP_ID: u32 = 0o2000;
    let inherited = |metadata: &std::fs::Metadata| metadata.mode() & SET_GROUP_ID;
    let opened =
        OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path);
    match opened {
        Ok(directory) => {
            let mode = mode | inherited(&directory.metadata()?);
            directory.set_permissions(std::fs::Permissions::from_mode(mode))
        }
        // A mode its owner cannot read leaves no handle to set it through.
        Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
            use std::os::unix::ffi::OsStrExt;
            let mode = mode | inherited(&std::fs::symlink_metadata(path)?);
            let name = std::ffi::CString::new(path.as_os_str().as_bytes())?;
            // `mode_t` is 16 bits on some systems and 32 on others; a checked mode fits either.
            #[allow(clippy::unnecessary_cast, clippy::cast_possible_truncation)]
            let bits = mode as libc::mode_t;
            // SAFETY: `name` is a NUL-terminated path that outlives the call, which keeps no pointer.
            let changed = unsafe { libc::fchmodat(libc::AT_FDCWD, name.as_ptr(), bits, libc::AT_SYMLINK_NOFOLLOW) };
            if changed == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
        }
        Err(error) => Err(error),
    }
}
