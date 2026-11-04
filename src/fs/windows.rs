//! The Windows half of the durable namespace operations.
//!
//! Windows has no documented `fsync` of a directory entry, so a new name is published the way its
//! documentation does promise: a complete, flushed file is moved into place with
//! `MOVEFILE_WRITE_THROUGH`, which does not return until the move is on the disk. Leaving out
//! `MOVEFILE_REPLACE_EXISTING` makes that move exclusive. The parent directory's handle is then
//! flushed as well, which Windows permits without saying what it covers. Every failure is
//! reported, and a volume these promises cannot be expected of (not `NTFS` or `ReFS`, or remote)
//! is refused as `unsupported` before anything changes.

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
const DRIVE_REMOTE: u32 = 4;
/// `MAX_PATH + 1`, the longest name `GetVolumeInformationByHandleW` documents.
const NAME_CAPACITY: u32 = 261;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    fn GetVolumePathNameW(path: *const u16, volume: *mut u16, capacity: u32) -> i32;
    fn GetDriveTypeW(root: *const u16) -> u32;
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

/// A directory handle `FlushFileBuffers` accepts, which takes write access. Backup semantics is
/// what lets `CreateFileW` open a directory at all; it opens a file too, so that is refused here.
fn open_directory(path: &Path) -> io::Result<File> {
    let directory = OpenOptions::new().read(true).write(true).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(path)?;
    if !directory.metadata()?.is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotADirectory, "not a directory"));
    }
    Ok(directory)
}

fn is_remote(path: &Path) -> io::Result<bool> {
    let path = wide(path)?;
    // A volume's root is never longer than a path on it plus a trailing separator.
    let mut root = vec![0_u16; path.len().max(NAME_CAPACITY as usize) + 1];
    let capacity = u32::try_from(root.len()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `path` is null terminated and `root` holds `capacity` elements.
    if unsafe { GetVolumePathNameW(path.as_ptr(), root.as_mut_ptr(), capacity) } == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: on success `root` is a null-terminated path.
    Ok(unsafe { GetDriveTypeW(root.as_ptr()) } == DRIVE_REMOTE)
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
fn require_durable_volume(directory: &File, path: &Path) -> io::Result<()> {
    if is_remote(path)? {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "durable operations are unsupported on a remote volume",
        ));
    }
    let filesystem = filesystem_name(directory)?;
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

pub(super) fn sync_directory(path: &Path) -> io::Result<()> {
    let directory = open_directory(path)?;
    require_durable_volume(&directory, path)?;
    directory.sync_all()
}

pub(super) fn replace(from: &Path, to: &Path) -> io::Result<()> {
    let parent = super::io::parent_directory(to);
    require_durable_volume(&open_directory(parent)?, parent)?;
    move_through(from, to, MOVEFILE_REPLACE_EXISTING)
}

/// Unlike the Unix path, a failure before the move leaves no file at `path`.
pub(super) fn create_durable(path: &Path, data: &[u8]) -> io::Result<()> {
    let parent = super::io::parent_directory(path);
    let directory = open_directory(parent)?;
    require_durable_volume(&directory, parent)?;
    let (temporary, mut file) = super::io::create_sibling(path)?;
    let written = file.write_all(data).and_then(|()| file.sync_all());
    drop(file);
    let moved = written.and_then(|()| move_through(&temporary, path, 0));
    if moved.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    moved?;
    directory.sync_all()
}

#[cfg(test)]
mod tests {
    use super::is_journaled;

    #[test]
    fn only_journaled_filesystems_are_accepted() {
        assert!(is_journaled("NTFS") && is_journaled("ReFS") && is_journaled("ntfs"));
        assert!(!is_journaled("FAT32") && !is_journaled("exFAT") && !is_journaled(""));
    }
}
