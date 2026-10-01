//! The host filesystem for scripts: the `dream.fs` extension, module `@dream/fs`.
//!
//! Everything a tool that works on files on disk needs and Luau lacks: whole and positional
//! reads, readers over a memory map, writers with positions and truncation, metadata with and
//! without following symbolic links, directory listings and a fast recursive walk with metadata,
//! directories made and removed (recursively too), renames, copies, hard and symbolic links,
//! canonical paths and file identity.
//!
//! ```lua
//! local fs = require('@dream/fs')
//! local reader = fs.open('Data Files/Morrowind.bsa')
//! local header = reader:readAt(0, 12)
//! local walk = fs.walk('Data Files', { followLinks = true, include = 'files' })
//! for index, path in walk.paths do
//!     print(path, walk.kinds[index])
//! end
//! ```
//!
//! # Paths are bytes
//!
//! Every path argument is a Luau string read as bytes, and every path or name the module returns
//! is the exact bytes the OS gave it: a file name that is not UTF-8 round-trips on Unix. Windows
//! paths are Unicode, so there a path must be UTF-8.
//!
//! # Capabilities
//!
//! Reading needs the `filesystem.read` capability ([`READ_CAPABILITY`]) and anything that
//! changes the disk needs `filesystem.write` ([`WRITE_CAPABILITY`]). Both are optional: a plan
//! that grants neither still has the module and its types, and each function raises a permission
//! error naming the capability it lacks.
//!
//! # Errors
//!
//! A failed operation raises `dream.fs.<function>: <path>: <the OS's message>`, for example
//! `dream.fs.open: Data Files/x.bsa: No such file or directory (os error 2)`. `stat`, `lstat`
//! and `readLink` answer nil for a path where nothing is; `exists` answers false.

mod io;
mod walk;

use std::path::{Path, PathBuf};

pub use io::{Reader, Writer};

use crate::bind::{ArgView, Call, StackResults};
use crate::convert::new_buffer;
use crate::error::{Error, Result};
use crate::extension::{Extension, ExtensionDescriptor, InstallContext};
use crate::options::Options;
use crate::stack::{Frame, Scope, ValueView};
use crate::userdata::Owned;

/// The extension id.
pub const EXTENSION_ID: &str = "dream.fs";
/// The module path.
pub const MODULE: &str = "@dream/fs";
/// The capability every read needs.
pub const READ_CAPABILITY: &str = "filesystem.read";
/// The capability every change to the disk needs.
pub const WRITE_CAPABILITY: &str = "filesystem.write";

/// The host path for `bytes`: the bytes themselves on Unix, UTF-8 elsewhere.
// Infallible on Unix, where any bytes are a path; elsewhere a path must be UTF-8.
#[cfg_attr(unix, allow(clippy::unnecessary_wraps))]
pub(crate) fn host_path(what: &str, bytes: &[u8]) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        let _ = what;
        use std::os::unix::ffi::OsStrExt;
        Ok(PathBuf::from(std::ffi::OsStr::from_bytes(bytes)))
    }
    #[cfg(not(unix))]
    {
        std::str::from_utf8(bytes)
            .map(PathBuf::from)
            .map_err(|_| Error::runtime(format!("dream.fs.{what}: {} is not UTF-8", display(bytes))))
    }
}

/// The bytes a host path or name is returned as.
pub(crate) fn path_bytes(path: &Path) -> &[u8] {
    path.as_os_str().as_encoded_bytes()
}

/// A path in a message.
pub(crate) fn display(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `dream.fs.<what>: <path>: <error>`.
pub(crate) fn io_error(what: &str, path: &[u8], error: &std::io::Error) -> Error {
    Error::runtime(format!("dream.fs.{what}: {}: {error}", display(path)))
}

/// `fs::canonicalize` as a script sees it again: on Windows the plain drive or UNC spelling,
/// not the verbatim `\\?\` one the Win32 API will not join with `/`.
fn canonical(path: &Path) -> std::io::Result<PathBuf> {
    let path = std::fs::canonicalize(path)?;
    #[cfg(windows)]
    {
        if let Some(text) = path.to_str() {
            if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
                return Ok(PathBuf::from(format!(r"\\{rest}")));
            }
            if let Some(rest) = text.strip_prefix(r"\\?\")
                && rest.as_bytes().get(1) == Some(&b':')
            {
                return Ok(PathBuf::from(rest));
            }
        }
    }
    Ok(path)
}

/// A file type's name: `file`, `dir`, `symlink` or `other`.
pub(crate) fn kind_name(file_type: std::fs::FileType) -> &'static str {
    if file_type.is_symlink() {
        "symlink"
    } else if file_type.is_dir() {
        "dir"
    } else if file_type.is_file() {
        "file"
    } else {
        "other"
    }
}

/// Seconds and nanoseconds since 1970 of a time after it, or the signed seconds of one before.
pub(crate) fn since_epoch(time: std::time::SystemTime) -> (f64, Option<(u64, u32)>) {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(since) => (since.as_secs_f64(), Some((since.as_secs(), since.subsec_nanos()))),
        Err(before) => (-before.duration().as_secs_f64(), None),
    }
}

fn push_stat(call: &Call<'_>, metadata: &std::fs::Metadata) -> Result<StackResults> {
    let file_type = metadata.file_type();
    let (modified, exact) = match metadata.modified() {
        Ok(time) => {
            let (seconds, exact) = since_epoch(time);
            (Some(seconds), exact)
        }
        Err(_) => (None, None),
    };
    let mut frame = call.frame();
    let table = frame.push_table(0, 9)?;
    table.raw_set_value(&frame, "kind", kind_name(file_type))?;
    table.raw_set_value(&frame, "size", &(if file_type.is_dir() { 0.0 } else { metadata.len() as f64 }))?;
    table.raw_set_value(&frame, "isFile", &file_type.is_file())?;
    table.raw_set_value(&frame, "isDir", &file_type.is_dir())?;
    table.raw_set_value(&frame, "isSymlink", &file_type.is_symlink())?;
    table.raw_set_value(&frame, "readonly", &metadata.permissions().readonly())?;
    table.raw_set_value(&frame, "modified", &modified)?;
    table.raw_set_value(&frame, "modifiedSeconds", &exact.map(|(seconds, _)| seconds as f64))?;
    table.raw_set_value(&frame, "modifiedNanoseconds", &exact.map(|(_, nanos)| f64::from(nanos)))?;
    frame.release();
    Ok(StackResults)
}

/// `stat` or `lstat`: the table, or nil when nothing is there.
fn stat(call: &Call<'_>, what: &str, path: &[u8], follow: bool) -> Result<StackResults> {
    let host = host_path(what, path)?;
    let metadata = if follow { std::fs::metadata(&host) } else { std::fs::symlink_metadata(&host) };
    match metadata {
        Ok(metadata) => push_stat(call, &metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            call.push(&())?;
            Ok(StackResults)
        }
        Err(error) => Err(io_error(what, path, &error)),
    }
}

/// Pushes `items` as a sequence table.
fn push_list<T>(scope: &impl Scope, items: &[T], mut push: impl FnMut(&Frame<'_>, &T) -> Result<()>) -> Result<()> {
    let mut frame = scope.frame();
    let table = frame.push_table(items.len(), 0)?;
    for (index, item) in items.iter().enumerate() {
        push(&frame, item)?;
        table.raw_set_index(&frame, index as i64 + 1)?;
    }
    frame.release();
    Ok(())
}

fn read_file(call: &Call<'_>, path: &[u8]) -> Result<StackResults> {
    let host = host_path("readFile", path)?;
    let bytes = std::fs::read(&host).map_err(|error| io_error("readFile", path, &error))?;
    let mut buffer = new_buffer(call, bytes.len())?;
    // SAFETY: the buffer was created by this call and no other view of it exists.
    unsafe { buffer.bytes_mut_unchecked() }.copy_from_slice(&bytes);
    Ok(StackResults)
}

fn list(call: &Call<'_>, path: &[u8]) -> Result<StackResults> {
    let host = host_path("list", path)?;
    let mut names = std::fs::read_dir(&host)
        .and_then(|entries| {
            entries
                .map(|entry| entry.map(|entry| entry.file_name().as_encoded_bytes().to_vec()))
                .collect::<std::io::Result<Vec<_>>>()
        })
        .map_err(|error| io_error("list", path, &error))?;
    names.sort_unstable();
    push_list(call, &names, |frame, name| frame.push(name.as_slice()).map(drop))?;
    Ok(StackResults)
}

/// `{ recursive? }`.
fn recursive(scope: &impl Scope, options: Option<ValueView<'_>>, context: &str) -> Result<bool> {
    let Some(options) = options.filter(|view| !view.is_nil()) else {
        return Ok(false);
    };
    Options::read(scope, options, context, |o| o.or("recursive", false))
}

fn mkdir(call: &Call<'_>, path: &[u8], options: Option<ValueView<'_>>) -> Result<()> {
    let host = host_path("mkdir", path)?;
    let result = if recursive(call, options, "dream.fs.mkdir")? {
        std::fs::create_dir_all(&host)
    } else {
        std::fs::create_dir(&host)
    };
    result.map_err(|error| io_error("mkdir", path, &error))
}

/// Removes a file, a link (never what it points at), or a directory: empty, or with everything in
/// it when `recursive`.
fn remove(call: &Call<'_>, path: &[u8], options: Option<ValueView<'_>>) -> Result<()> {
    let recursive = recursive(call, options, "dream.fs.remove")?;
    let host = host_path("remove", path)?;
    let metadata = std::fs::symlink_metadata(&host).map_err(|error| io_error("remove", path, &error))?;
    let result = if !metadata.is_dir() {
        remove_link_or_file(&host, &metadata)
    } else if recursive {
        std::fs::remove_dir_all(&host)
    } else {
        std::fs::remove_dir(&host)
    };
    result.map_err(|error| io_error("remove", path, &error))
}

/// A file, or a link: on Windows a link to a directory is removed as a directory.
fn remove_link_or_file(path: &Path, metadata: &std::fs::Metadata) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        if metadata.file_type().is_symlink_dir() {
            return std::fs::remove_dir(path);
        }
    }
    let _ = metadata;
    std::fs::remove_file(path)
}

fn symlink(call: &Call<'_>, target: &[u8], link: &[u8], options: Option<ValueView<'_>>) -> Result<()> {
    let directory = match options.filter(|view| !view.is_nil()) {
        Some(options) => Options::read(call, options, "dream.fs.symlink", |o| o.or("directory", false))?,
        None => false,
    };
    let target_path = host_path("symlink", target)?;
    let link_path = host_path("symlink", link)?;
    #[cfg(unix)]
    let result = {
        let _ = directory;
        std::os::unix::fs::symlink(&target_path, &link_path)
    };
    #[cfg(windows)]
    let result = if directory {
        std::os::windows::fs::symlink_dir(&target_path, &link_path)
    } else {
        std::os::windows::fs::symlink_file(&target_path, &link_path)
    };
    #[cfg(not(any(unix, windows)))]
    let result: std::io::Result<()> = {
        let _ = (directory, &target_path, &link_path);
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "symbolic links are not supported on this platform"))
    };
    result.map_err(|error| io_error("symlink", link, &error))
}

fn read_link(path: &[u8]) -> Result<Option<Vec<u8>>> {
    let host = host_path("readLink", path)?;
    match std::fs::read_link(&host) {
        Ok(target) => Ok(Some(path_bytes(&target).to_vec())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error("readLink", path, &error)),
    }
}

/// Whether two paths name the same file (the same device and file number, so a hard link or a
/// symbolic link to it counts); false when either does not exist.
fn same_file(a: &[u8], b: &[u8]) -> Result<bool> {
    let (left, right) = (host_path("sameFile", a)?, host_path("sameFile", b)?);
    match same_file::is_same_file(&left, &right) {
        Ok(same) => Ok(same),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_error("sameFile", a, &error)),
    }
}

/// A module member: its name, signature, doc, and whether it changes the disk.
struct Member {
    name: &'static str,
    signature: &'static str,
    doc: &'static str,
    writes: bool,
}

const fn member(name: &'static str, signature: &'static str, doc: &'static str, writes: bool) -> Member {
    Member { name, signature, doc, writes }
}

/// The module's functions, in declaration order.
const MEMBERS: &[Member] = &[
    member("readFile", "(path: string) -> buffer", "The whole file as a new buffer.", false),
    member("readFileString", "(path: string) -> string", "The whole file as a string.", false),
    member(
        "readAt",
        "(path: string, offset: number, length: number) -> buffer",
        "length bytes from offset as a new buffer, fewer at the end; an offset past the end is an error.",
        false,
    ),
    member("open", "(path: string) -> dream_fs_Reader", "A reader over a memory map of the file.", false),
    member(
        "stat",
        "(path: string) -> dream_fs_Stat?",
        "The metadata of what path names, following symbolic links, or nil when nothing is there.",
        false,
    ),
    member(
        "lstat",
        "(path: string) -> dream_fs_Stat?",
        "The metadata of path itself: a symbolic link is described, not followed. Nil when nothing is there.",
        false,
    ),
    member("exists", "(path: string) -> boolean", "Whether something is there, following symbolic links.", false),
    member("list", "(path: string) -> { string }", "The names in a directory, sorted by their bytes.", false),
    member(
        "walk",
        "(root: string, options: dream_fs_WalkOptions?) -> dream_fs_Walk",
        "Everything under root, depth first, as parallel arrays: paths relative to root, kinds, and with metadata = true sizes and modification times.",
        false,
    ),
    member(
        "canonicalize",
        "(path: string) -> string",
        "The absolute path with every symbolic link, '.' and '..' resolved; the file must exist.",
        false,
    ),
    member(
        "absolute",
        "(path: string) -> string",
        "path made absolute against the working directory, without touching the disk or resolving links.",
        false,
    ),
    member(
        "readLink",
        "(path: string) -> string?",
        "What a symbolic link points at, as written in the link; nil when nothing is there.",
        false,
    ),
    member(
        "sameFile",
        "(a: string, b: string) -> boolean",
        "Whether a and b are the same file (hard links and symbolic links to it included); false when either is missing.",
        false,
    ),
    member("cwd", "() -> string", "The working directory.", false),
    member(
        "writeFile",
        "(path: string, data: buffer | string, options: { offset: number?, append: boolean?, create: boolean? }?) -> number",
        "Writes data to path, truncating it, and returns the count. offset writes in place without truncating; append adds at the end; create = false refuses a file that does not exist.",
        true,
    ),
    member(
        "openWrite",
        "(path: string, options: { append: boolean?, truncate: boolean?, create: boolean? }?) -> dream_fs_Writer",
        "A writer over path, truncated unless append or truncate = false; created unless create = false.",
        true,
    ),
    member(
        "mkdir",
        "(path: string, options: { recursive: boolean? }?) -> ()",
        "Creates a directory; recursive creates its missing parents and accepts one that exists.",
        true,
    ),
    member(
        "remove",
        "(path: string, options: { recursive: boolean? }?) -> ()",
        "Removes a file, a symbolic link (never what it points at), or an empty directory; recursive removes a directory with everything in it.",
        true,
    ),
    member("rename", "(from: string, to: string) -> ()", "Moves a file or directory, replacing a file at to.", true),
    member(
        "copy",
        "(from: string, to: string) -> number",
        "Copies a file's contents and permissions; returns the byte count.",
        true,
    ),
    member(
        "hardLink",
        "(source: string, link: string) -> ()",
        "Creates link as another name for the file source.",
        true,
    ),
    member(
        "symlink",
        "(target: string, link: string, options: { directory: boolean? }?) -> ()",
        "Creates link as a symbolic link to target, stored as written. directory marks a link to a directory, which Windows needs.",
        true,
    ),
];

const STAT_TYPE: &str = "{ kind: dream_fs_FileKind, size: number, isFile: boolean, isDir: boolean, isSymlink: boolean, \
    readonly: boolean, modified: number?, modifiedSeconds: number?, modifiedNanoseconds: number? }";
const WALK_OPTIONS_TYPE: &str = "{ followLinks: boolean?, include: (\"all\" | \"files\" | \"dirs\")?, metadata: boolean?, \
    sort: boolean?, maxDepth: number?, skipErrors: boolean? }";
const WALK_TYPE: &str = "{ paths: { string }, kinds: { dream_fs_FileKind }, sizes: { number }?, \
    modifiedSeconds: { number }?, modifiedNanoseconds: { number }?, errors: { { path: string, message: string } } }";

/// The `dream.fs` extension.
#[derive(Clone, Copy, Debug, Default)]
pub struct FsExtension;

impl Extension for FsExtension {
    fn id(&self) -> &'static str {
        EXTENSION_ID
    }

    fn describe(&self, d: &mut ExtensionDescriptor) -> Result<()> {
        d.type_alias("dream_fs_FileKind", "\"file\" | \"dir\" | \"symlink\" | \"other\"");
        d.type_alias("dream_fs_Stat", STAT_TYPE);
        d.type_alias("dream_fs_WalkOptions", WALK_OPTIONS_TYPE);
        d.type_alias("dream_fs_Walk", WALK_TYPE);
        io::describe_reader(d);
        io::describe_writer(d);
        d.optional_capability(READ_CAPABILITY);
        d.optional_capability(WRITE_CAPABILITY);
        let module = d.module(MODULE);
        module.doc(
            "The host filesystem: reads, readers and writers, metadata, walks, links, and identity, over byte paths.",
        );
        for member in MEMBERS {
            module.installed(member.name).signature(member.signature).doc(member.doc);
        }
        Ok(())
    }

    /// Every function depends on the runtime's capabilities, so each binds here: the real one
    /// when the policy grants what it needs, otherwise one that raises a permission error.
    fn install(&self, cx: &mut InstallContext<'_>) -> Result<()> {
        let read = cx.has_capability(READ_CAPABILITY)?;
        let write = cx.has_capability(WRITE_CAPABILITY)?;
        let module = cx.module(MODULE)?;
        macro_rules! bind {
            ($name:literal, $callable:expr) => {{
                let writes = MEMBERS.iter().any(|member| member.name == $name && member.writes);
                let (granted, capability) = if writes { (write, WRITE_CAPABILITY) } else { (read, READ_CAPABILITY) };
                if granted {
                    module.function($name, $callable)?;
                } else {
                    let message = format!(
                        "dream.fs.{}: needs the '{capability}' capability, which this runtime does not grant",
                        $name
                    );
                    module.function($name, move |_: ArgView<'_>| -> Result<()> {
                        Err(Error::permission(message.clone()))
                    })?;
                }
            }};
        }
        bind!("readFile", read_file);
        bind!("readFileString", |path: &[u8]| -> Result<Vec<u8>> {
            std::fs::read(host_path("readFileString", path)?).map_err(|error| io_error("readFileString", path, &error))
        });
        bind!("readAt", io::read_at);
        bind!("open", |path: &[u8]| Reader::open(path).map(Owned));
        bind!("stat", |call: &Call<'_>, path: &[u8]| stat(call, "stat", path, true));
        bind!("lstat", |call: &Call<'_>, path: &[u8]| stat(call, "lstat", path, false));
        bind!("exists", |path: &[u8]| -> Result<bool> {
            std::fs::exists(host_path("exists", path)?).map_err(|error| io_error("exists", path, &error))
        });
        bind!("list", list);
        bind!("walk", walk::walk);
        bind!("canonicalize", |path: &[u8]| -> Result<Vec<u8>> {
            canonical(&host_path("canonicalize", path)?)
                .map(|resolved| path_bytes(&resolved).to_vec())
                .map_err(|error| io_error("canonicalize", path, &error))
        });
        bind!("absolute", |path: &[u8]| -> Result<Vec<u8>> {
            std::path::absolute(host_path("absolute", path)?)
                .map(|absolute| path_bytes(&absolute).to_vec())
                .map_err(|error| io_error("absolute", path, &error))
        });
        bind!("readLink", read_link);
        bind!("sameFile", same_file);
        bind!("cwd", || -> Result<Vec<u8>> {
            std::env::current_dir()
                .map(|dir| path_bytes(&dir).to_vec())
                .map_err(|error| Error::runtime(format!("dream.fs.cwd: {error}")))
        });
        bind!("writeFile", io::write_file);
        bind!("openWrite", |call: &Call<'_>, path: &[u8], options: Option<ValueView<'_>>| {
            let options = io::OpenWrite::read(call, options)?;
            Writer::open(path, options).map(Owned)
        });
        bind!("mkdir", mkdir);
        bind!("remove", remove);
        bind!("rename", |from: &[u8], to: &[u8]| -> Result<()> {
            std::fs::rename(host_path("rename", from)?, host_path("rename", to)?)
                .map_err(|error| io_error("rename", from, &error))
        });
        bind!("copy", |from: &[u8], to: &[u8]| -> Result<f64> {
            std::fs::copy(host_path("copy", from)?, host_path("copy", to)?)
                .map(|count| count as f64)
                .map_err(|error| io_error("copy", from, &error))
        });
        bind!("hardLink", |source: &[u8], link: &[u8]| -> Result<()> {
            std::fs::hard_link(host_path("hardLink", source)?, host_path("hardLink", link)?)
                .map_err(|error| io_error("hardLink", link, &error))
        });
        bind!("symlink", symlink);
        Ok(())
    }
}
