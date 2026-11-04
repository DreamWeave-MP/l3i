//! The Windows half of the durable namespace operations.
//!
//! Windows documents no `fsync` of a directory entry, so this is the closest thing it offers, and
//! a weaker contract than Unix's: a complete, flushed file is moved into place with
//! `MOVEFILE_WRITE_THROUGH`. Microsoft says that call does not return until the move is on the
//! disk, but its explicit flush guarantee describes a move done as copy and delete, not a rename
//! on one volume. Leaving out `MOVEFILE_REPLACE_EXISTING` makes the move exclusive. The parent
//! directory's handle is then flushed as well, which is not among the operations documented for
//! directory handles, so the move never depends on it. Success here does not prove the name
//! survives a power cut; a consumer's recovery has to stay correct if the last one is lost.
//!
//! A volume none of this can be expected of (not `NTFS` or `ReFS`, or remote) is refused as
//! `unsupported` before anything changes.

use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
const VOLUME_NAME_NT: u32 = 0x2;
/// `ERROR_INVALID_FUNCTION`, `ERROR_ACCESS_DENIED`, `ERROR_NOT_SUPPORTED`, `ERROR_INVALID_PARAMETER`.
const FLUSH_REFUSALS: [i32; 4] = [1, 5, 50, 87];
/// `MAX_PATH + 1`, the longest name `GetVolumeInformationByHandleW` documents.
const NAME_CAPACITY: u32 = 261;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    fn GetFinalPathNameByHandleW(file: *mut c_void, path: *mut u16, capacity: u32, flags: u32) -> u32;
    fn GetVolumeInformationByHandleW(
        file: *mut c_void,
        volume_name: *mut u16,
        volume_name_capacity: u32,
        serial: *mut u32,
        maximum_component: *mut u32,
        flags: *mut u32,
        filesystem_name: *mut u16,
        filesystem_name_capacity: u32,
    ) -> i32;
}

/// `path` as the null-terminated UTF-16 a Win32 call takes.
fn wide(path: &Path) -> io::Result<Vec<u16>> {
    let mut encoded: Vec<u16> = path.as_os_str().encode_wide().collect();
    if encoded.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL"));
    }
    encoded.push(0);
    Ok(encoded)
}

/// A read-only directory handle, which is all the volume queries need. Backup semantics is what
/// lets `CreateFileW` open a directory at all; it opens a file too, so that is refused here.
fn open_directory(path: &Path) -> io::Result<File> {
    let directory = OpenOptions::new().read(true).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path)?;
    if !directory.metadata()?.is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotADirectory, "not a directory"));
    }
    Ok(directory)
}

/// `FlushFileBuffers` on a directory, which takes a handle with write access.
fn flush_directory(path: &Path) -> io::Result<()> {
    OpenOptions::new().read(true).write(true).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path)?.sync_all()
}

/// Whether an NT device path is the network redirector's, under which mapped drives land too.
fn is_remote_device(path: &str) -> bool {
    ["\\device\\mup\\", "\\device\\lanmanredirector\\"]
        .iter()
        .any(|device| path.get(..device.len()).is_some_and(|prefix| prefix.eq_ignore_ascii_case(device)))
}

/// Asks the handle rather than the path, so a relative path, a junction or a mount point answers
/// for the volume the directory is really on.
fn is_remote(directory: &File) -> io::Result<bool> {
    let mut path = vec![0_u16; 1024];
    loop {
        let capacity = u32::try_from(path.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        // SAFETY: the handle is open for the call and `path` holds `capacity` elements.
        let length = unsafe {
            GetFinalPathNameByHandleW(directory.as_raw_handle(), path.as_mut_ptr(), capacity, VOLUME_NAME_NT)
        };
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        // Too small answers the size needed, counting the terminator; success answers without it.
        if length < capacity {
            return Ok(is_remote_device(&String::from_utf16_lossy(&path[..length as usize])));
        }
        path.resize(length as usize, 0);
    }
}

fn filesystem_name(directory: &File) -> io::Result<String> {
    let mut name = [0_u16; NAME_CAPACITY as usize];
    // SAFETY: the handle is open for the call, `name` holds the capacity passed, and every other
    // out pointer may be null.
    let status = unsafe {
        GetVolumeInformationByHandleW(
            directory.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            name.as_mut_ptr(),
            NAME_CAPACITY,
        )
    };
    if status == 0 {
        return Err(io::Error::last_os_error());
    }
    let length = name.iter().position(|&unit| unit == 0).unwrap_or(name.len());
    Ok(String::from_utf16_lossy(&name[..length]))
}

fn is_journaled(filesystem: &str) -> bool {
    filesystem.eq_ignore_ascii_case("NTFS") || filesystem.eq_ignore_ascii_case("ReFS")
}

/// Refuses a volume the durable operations promise nothing on, before anything changes.
fn require_durable_volume(path: &Path) -> io::Result<()> {
    let directory = open_directory(path)?;
    if is_remote(&directory)? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "durable operations are unsupported on a remote volume",
        ));
    }
    let filesystem = filesystem_name(&directory)?;
    if !is_journaled(&filesystem) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("durable operations are unsupported on {filesystem}"),
        ));
    }
    Ok(())
}

fn move_through(from: &Path, to: &Path, flags: u32) -> io::Result<()> {
    let (from, to) = (wide(from)?, wide(to)?);
    // SAFETY: both paths are null terminated and outlive the call.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), flags | MOVEFILE_WRITE_THROUGH) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reports every failure: asking for this flush and nothing else, the caller needs to know.
pub(super) fn sync_directory(path: &Path) -> io::Result<()> {
    require_durable_volume(path)?;
    flush_directory(path)
}

pub(super) fn replace(from: &Path, to: &Path) -> io::Result<()> {
    require_durable_volume(super::io::parent_directory(to))?;
    move_through(from, to, MOVEFILE_REPLACE_EXISTING)
}

/// Unlike the Unix path, a failure before the move leaves no file at `path`. A directory flush
/// Windows refuses outright changes nothing, because the move has already been written through;
/// one that fails is an I/O error like any other.
pub(super) fn create_durable(path: &Path, data: &[u8]) -> io::Result<()> {
    let parent = super::io::parent_directory(path);
    require_durable_volume(parent)?;
    let (temporary, mut file) = super::io::create_sibling(path)?;
    let written = file.write_all(data).and_then(|()| file.sync_all());
    drop(file);
    let moved = written.and_then(|()| move_through(&temporary, path, 0));
    if moved.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    moved?;
    match flush_directory(parent) {
        Err(error) if error.raw_os_error().is_some_and(|code| FLUSH_REFUSALS.contains(&code)) => Ok(()),
        flushed => flushed,
    }
}

#[cfg(test)]
mod tests {
    use super::{is_journaled, is_remote_device};

    #[test]
    fn only_local_device_paths_count_as_local() {
        assert!(is_remote_device(r"\Device\Mup\server\share\folder"));
        assert!(is_remote_device(r"\device\LanmanRedirector\server\share"));
        assert!(!is_remote_device(r"\Device\HarddiskVolume3\Users\me"));
        assert!(!is_remote_device(r"\Device\Mup"));
        assert!(!is_remote_device(""));
    }

    #[test]
    fn only_journaled_filesystems_are_accepted() {
        assert!(is_journaled("NTFS") && is_journaled("ReFS") && is_journaled("ntfs"));
        assert!(!is_journaled("FAT32") && !is_journaled("exFAT") && !is_journaled(""));
    }
}
