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
//! The disk refusing is an answer, not an exception: an operation the OS fails returns `nil`, the
//! message `dream.fs.<function>: <path>: <the OS's message>` and the error's kind
//! (`dream_fs_ErrorKind`, such as `notFound` or `permissionDenied`), the way `io.open` does:
//!
//! ```lua
//! local reader, message, kind = fs.open('Data Files/x.bsa')
//! if not reader then
//!     print(message) -- dream.fs.open: Data Files/x.bsa: No such file or directory (os error 2)
//!     return kind == 'notFound'
//! end
//! ```
//!
//! An operation with nothing to return returns `true`. `stat`, `lstat` and `readLink` answer a
//! lone nil for a path where nothing is; `exists` answers false. Calling a function wrongly (an
//! argument of the wrong type, an unknown option, a negative offset, a position past the end, a
//! closed handle) is the script's mistake, and raises.

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

pub(crate) use crate::outcome::{Failure, Outcome, done};

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

/// `stat` or `lstat`: the table, or a lone nil when nothing is there.
fn stat(call: &Call<'_>, what: &str, path: &[u8], follow: bool) -> Result<Outcome<StackResults>> {
    let host = host_path(what, path)?;
    let metadata = if follow { std::fs::metadata(&host) } else { std::fs::symlink_metadata(&host) };
    match metadata {
        Ok(metadata) => push_stat(call, &metadata).map(Outcome::Done),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            call.push(&())?;
            Ok(Outcome::Done(StackResults))
        }
        Err(error) => Ok(Outcome::Failed(Failure::new(what, path, &error))),
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

fn read_file(call: &Call<'_>, path: &[u8]) -> Result<Outcome<StackResults>> {
    let host = host_path("readFile", path)?;
    let bytes = done!(Outcome::of(std::fs::read(&host), "readFile", path));
    let mut buffer = new_buffer(call, bytes.len())?;
    // SAFETY: the buffer was created by this call and no other view of it exists.
    unsafe { buffer.bytes_mut_unchecked() }.copy_from_slice(&bytes);
    Ok(Outcome::Done(StackResults))
}

fn list(call: &Call<'_>, path: &[u8]) -> Result<Outcome<StackResults>> {
    let host = host_path("list", path)?;
    let names = std::fs::read_dir(&host).and_then(|entries| {
        entries
            .map(|entry| entry.map(|entry| entry.file_name().as_encoded_bytes().to_vec()))
            .collect::<std::io::Result<Vec<_>>>()
    });
    let mut names = done!(Outcome::of(names, "list", path));
    names.sort_unstable();
    push_list(call, &names, |frame, name| frame.push(name.as_slice()).map(drop))?;
    Ok(Outcome::Done(StackResults))
}

/// `{ recursive? }`.
fn recursive(scope: &impl Scope, options: Option<ValueView<'_>>, context: &str) -> Result<bool> {
    let Some(options) = options.filter(|view| !view.is_nil()) else {
        return Ok(false);
    };
    Options::read(scope, options, context, |o| o.or("recursive", false))
}

fn mkdir(call: &Call<'_>, path: &[u8], options: Option<ValueView<'_>>) -> Result<Outcome<bool>> {
    let host = host_path("mkdir", path)?;
    let result = if recursive(call, options, "dream.fs.mkdir")? {
        std::fs::create_dir_all(&host)
    } else {
        std::fs::create_dir(&host)
    };
    Ok(Outcome::of(result.map(|()| true), "mkdir", path))
}

/// Removes a file, a link (never what it points at), or a directory: empty, or with everything in
/// it when `recursive`.
fn remove(call: &Call<'_>, path: &[u8], options: Option<ValueView<'_>>) -> Result<Outcome<bool>> {
    let recursive = recursive(call, options, "dream.fs.remove")?;
    let host = host_path("remove", path)?;
    let metadata = done!(Outcome::of(std::fs::symlink_metadata(&host), "remove", path));
    let result = if !metadata.is_dir() {
        remove_link_or_file(&host, &metadata)
    } else if recursive {
        std::fs::remove_dir_all(&host)
    } else {
        std::fs::remove_dir(&host)
    };
    Ok(Outcome::of(result.map(|()| true), "remove", path))
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

fn symlink(call: &Call<'_>, target: &[u8], link: &[u8], options: Option<ValueView<'_>>) -> Result<Outcome<bool>> {
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
    Ok(Outcome::of(result.map(|()| true), "symlink", link))
}

fn read_link(path: &[u8]) -> Result<Outcome<Option<Vec<u8>>>> {
    let host = host_path("readLink", path)?;
    Ok(match std::fs::read_link(&host) {
        Ok(target) => Outcome::Done(Some(path_bytes(&target).to_vec())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Outcome::Done(None),
        Err(error) => Outcome::Failed(Failure::new("readLink", path, &error)),
    })
}

/// Whether two paths name the same file (the same device and file number, so a hard link or a
/// symbolic link to it counts); false when either does not exist.
fn same_file(a: &[u8], b: &[u8]) -> Result<Outcome<bool>> {
    let (left, right) = (host_path("sameFile", a)?, host_path("sameFile", b)?);
    Ok(match same_file::is_same_file(&left, &right) {
        Ok(same) => Outcome::Done(same),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Outcome::Done(false),
        Err(error) => Outcome::Failed(Failure::new("sameFile", a, &error)),
    })
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
    member(
        "readFile",
        "(path: string) -> (buffer?, string?, dream_fs_ErrorKind?)",
        "The whole file as a new buffer.",
        false,
    ),
    member(
        "readFileString",
        "(path: string) -> (string?, string?, dream_fs_ErrorKind?)",
        "The whole file as a string.",
        false,
    ),
    member(
        "readAt",
        "(path: string, offset: number, length: number) -> (buffer?, string?, dream_fs_ErrorKind?)",
        "length bytes from offset as a new buffer, fewer at the end; an offset past the end is an error.",
        false,
    ),
    member(
        "open",
        "(path: string) -> (dream_fs_Reader?, string?, dream_fs_ErrorKind?)",
        "A reader over a memory map of the file.",
        false,
    ),
    member(
        "stat",
        "(path: string) -> (dream_fs_Stat?, string?, dream_fs_ErrorKind?)",
        "The metadata of what path names, following symbolic links, or nil when nothing is there.",
        false,
    ),
    member(
        "lstat",
        "(path: string) -> (dream_fs_Stat?, string?, dream_fs_ErrorKind?)",
        "The metadata of path itself: a symbolic link is described, not followed. Nil when nothing is there.",
        false,
    ),
    member(
        "exists",
        "(path: string) -> (boolean?, string?, dream_fs_ErrorKind?)",
        "Whether something is there, following symbolic links.",
        false,
    ),
    member(
        "list",
        "(path: string) -> ({ string }?, string?, dream_fs_ErrorKind?)",
        "The names in a directory, sorted by their bytes.",
        false,
    ),
    member(
        "walk",
        "(root: string, options: dream_fs_WalkOptions?) -> (dream_fs_Walk?, string?, dream_fs_ErrorKind?)",
        "Everything under root, depth first, as parallel arrays: paths relative to root, kinds, and with metadata = true sizes and modification times.",
        false,
    ),
    member(
        "canonicalize",
        "(path: string) -> (string?, string?, dream_fs_ErrorKind?)",
        "The absolute path with every symbolic link, '.' and '..' resolved; the file must exist.",
        false,
    ),
    member(
        "absolute",
        "(path: string) -> (string?, string?, dream_fs_ErrorKind?)",
        "path made absolute against the working directory, without touching the disk or resolving links.",
        false,
    ),
    member(
        "readLink",
        "(path: string) -> (string?, string?, dream_fs_ErrorKind?)",
        "What a symbolic link points at, as written in the link; nil when nothing is there.",
        false,
    ),
    member(
        "sameFile",
        "(a: string, b: string) -> (boolean?, string?, dream_fs_ErrorKind?)",
        "Whether a and b are the same file (hard links and symbolic links to it included); false when either is missing.",
        false,
    ),
    member("cwd", "() -> (string?, string?, dream_fs_ErrorKind?)", "The working directory.", false),
    member(
        "writeFile",
        "(path: string, data: buffer | string, options: { offset: number?, append: boolean?, create: boolean? }?) -> (number?, string?, dream_fs_ErrorKind?)",
        "Writes data to path, truncating it, and returns the count. offset writes in place without truncating; append adds at the end; create = false refuses a file that does not exist.",
        true,
    ),
    member(
        "openWrite",
        "(path: string, options: { append: boolean?, truncate: boolean?, create: boolean? }?) -> (dream_fs_Writer?, string?, dream_fs_ErrorKind?)",
        "A writer over path, truncated unless append or truncate = false; created unless create = false.",
        true,
    ),
    member(
        "mkdir",
        "(path: string, options: { recursive: boolean? }?) -> (boolean?, string?, dream_fs_ErrorKind?)",
        "Creates a directory; recursive creates its missing parents and accepts one that exists.",
        true,
    ),
    member(
        "remove",
        "(path: string, options: { recursive: boolean? }?) -> (boolean?, string?, dream_fs_ErrorKind?)",
        "Removes a file, a symbolic link (never what it points at), or an empty directory; recursive removes a directory with everything in it.",
        true,
    ),
    member(
        "rename",
        "(from: string, to: string) -> (boolean?, string?, dream_fs_ErrorKind?)",
        "Moves a file or directory, replacing a file at to.",
        true,
    ),
    member(
        "copy",
        "(from: string, to: string) -> (number?, string?, dream_fs_ErrorKind?)",
        "Copies a file's contents and permissions; returns the byte count.",
        true,
    ),
    member(
        "hardLink",
        "(source: string, link: string) -> (boolean?, string?, dream_fs_ErrorKind?)",
        "Creates link as another name for the file source.",
        true,
    ),
    member(
        "symlink",
        "(target: string, link: string, options: { directory: boolean? }?) -> (boolean?, string?, dream_fs_ErrorKind?)",
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
        d.type_alias("dream_fs_ErrorKind", crate::outcome::ERROR_KIND_TYPE);
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
        bind!("readFileString", |path: &[u8]| -> Result<Outcome<Vec<u8>>> {
            Ok(Outcome::of(std::fs::read(host_path("readFileString", path)?), "readFileString", path))
        });
        bind!("readAt", io::read_at);
        bind!("open", |path: &[u8]| -> Result<Outcome<Owned<Reader>>> {
            Ok(match Reader::open(path)? {
                Outcome::Done(reader) => Outcome::Done(Owned(reader)),
                Outcome::Failed(failure) => Outcome::Failed(failure),
            })
        });
        bind!("stat", |call: &Call<'_>, path: &[u8]| stat(call, "stat", path, true));
        bind!("lstat", |call: &Call<'_>, path: &[u8]| stat(call, "lstat", path, false));
        bind!("exists", |path: &[u8]| -> Result<Outcome<bool>> {
            Ok(Outcome::of(std::fs::exists(host_path("exists", path)?), "exists", path))
        });
        bind!("list", list);
        bind!("walk", walk::walk);
        bind!("canonicalize", |path: &[u8]| -> Result<Outcome<Vec<u8>>> {
            let resolved = canonical(&host_path("canonicalize", path)?);
            Ok(Outcome::of(resolved.map(|resolved| path_bytes(&resolved).to_vec()), "canonicalize", path))
        });
        bind!("absolute", |path: &[u8]| -> Result<Outcome<Vec<u8>>> {
            let absolute = std::path::absolute(host_path("absolute", path)?);
            Ok(Outcome::of(absolute.map(|absolute| path_bytes(&absolute).to_vec()), "absolute", path))
        });
        bind!("readLink", read_link);
        bind!("sameFile", same_file);
        bind!("cwd", || -> Outcome<Vec<u8>> {
            match std::env::current_dir() {
                Ok(dir) => Outcome::Done(path_bytes(&dir).to_vec()),
                Err(error) => Outcome::Failed(Failure::message(format!("dream.fs.cwd: {error}"), &error)),
            }
        });
        bind!("writeFile", io::write_file);
        bind!("openWrite", |call: &Call<'_>,
                            path: &[u8],
                            options: Option<ValueView<'_>>|
         -> Result<Outcome<Owned<Writer>>> {
            let options = io::OpenWrite::read(call, options)?;
            Ok(match Writer::open(path, options)? {
                Outcome::Done(writer) => Outcome::Done(Owned(writer)),
                Outcome::Failed(failure) => Outcome::Failed(failure),
            })
        });
        bind!("mkdir", mkdir);
        bind!("remove", remove);
        bind!("rename", |from: &[u8], to: &[u8]| -> Result<Outcome<bool>> {
            let result = std::fs::rename(host_path("rename", from)?, host_path("rename", to)?);
            Ok(Outcome::of(result.map(|()| true), "rename", from))
        });
        bind!("copy", |from: &[u8], to: &[u8]| -> Result<Outcome<f64>> {
            let result = std::fs::copy(host_path("copy", from)?, host_path("copy", to)?);
            Ok(Outcome::of(result.map(|count| count as f64), "copy", from))
        });
        bind!("hardLink", |source: &[u8], link: &[u8]| -> Result<Outcome<bool>> {
            let result = std::fs::hard_link(host_path("hardLink", source)?, host_path("hardLink", link)?);
            Ok(Outcome::of(result.map(|()| true), "hardLink", link))
        });
        bind!("symlink", symlink);
        Ok(())
    }
}
