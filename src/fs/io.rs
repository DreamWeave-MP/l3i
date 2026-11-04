//! Positional reads, `dream.fs.Reader` and `dream.fs.Writer`: the handles `fs.open` and
//! `fs.openWrite` return. A reader keeps a memory map of its file (or, when the file cannot be
//! mapped, the open handle for positional reads), so a parse that reads piece by piece copies
//! each piece once and allocates nothing per call.

use std::cell::{Cell, RefCell};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use crate::bind::{Call, StackResults};
use crate::convert::{BufferView, BytesView, Exact, new_buffer};
use crate::error::{Error, Result};
use crate::extension::{ExtensionDescriptor, TagPolicy};
use crate::options::Options;
use crate::stack::{Scope, ValueView};
use crate::userdata::Userdata;

use super::{Outcome, display, done};

/// What reading a FIFO, socket or device as a file fails with: such a file would block the VM
/// thread waiting for a writer, or never end.
fn not_a_regular_file() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "not a regular file")
}

/// Opens `path` for reading, refusing anything but a regular file. On Unix the open itself is
/// non-blocking, so a FIFO with no writer is refused at once instead of hanging the open; the
/// flag changes nothing for the regular file that passes.
pub(crate) fn open_regular(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NONBLOCK);
    let file = match options.open(path) {
        Ok(file) => file,
        // Windows refuses to open a directory as a file with ERROR_ACCESS_DENIED; say what it is.
        Err(_) if std::fs::metadata(path).is_ok_and(|metadata| metadata.is_dir()) => return Err(is_a_directory()),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if metadata.is_dir() {
        return Err(is_a_directory());
    }
    if !metadata.is_file() {
        return Err(not_a_regular_file());
    }
    Ok(file)
}

/// The whole regular file at `path`, as `std::fs::read` reads it.
pub(crate) fn read_regular(path: &Path) -> io::Result<Vec<u8>> {
    let mut file = open_regular(path)?;
    let mut bytes = Vec::new();
    io::Read::read_to_end(&mut file, &mut bytes)?;
    Ok(bytes)
}
/// Where a reader's bytes come from.
#[derive(Debug)]
pub(crate) enum Backing {
    /// The file, mapped.
    Mapped(memmap2::Mmap),
    /// A file that could not be mapped, read positionally through its handle.
    File { file: File, len: u64 },
    /// An empty file: nothing to map, and an empty map is an error on every platform.
    Empty,
}

/// What reading a directory as a file fails with, said when it is opened: EISDIR (21) on Linux
/// and macOS, the portable kind elsewhere.
fn is_a_directory() -> io::Error {
    if cfg!(unix) {
        io::Error::from_raw_os_error(21)
    } else {
        io::Error::new(io::ErrorKind::IsADirectory, "Is a directory")
    }
}

impl Backing {
    /// Maps the file at `path`, or opens it for positional reads when mapping fails.
    pub(crate) fn open(path: &Path) -> io::Result<Backing> {
        let file = open_regular(path)?;
        let len = file.metadata()?.len();
        if len == 0 {
            return Ok(Backing::Empty);
        }
        // SAFETY: the map is private and read-only, and its bytes are only ever copied into Luau
        // buffers while no call into Lua is in progress. A file truncated underneath the map
        // faults on the copy; it never corrupts Rust memory.
        match unsafe { memmap2::Mmap::map(&file) } {
            Ok(map) => Ok(Backing::Mapped(map)),
            Err(_) => Ok(Backing::File { file, len }),
        }
    }

    /// The size in bytes.
    pub(crate) fn len(&self) -> u64 {
        match self {
            Backing::Mapped(map) => map.len() as u64,
            Backing::File { len, .. } => *len,
            Backing::Empty => 0,
        }
    }

    /// Copies up to `dst.len()` bytes from `offset` (at most the size) into `dst`, short only
    /// at the end; returns the count copied.
    pub(crate) fn read_at(&self, offset: u64, dst: &mut [u8]) -> io::Result<usize> {
        match self {
            Backing::Mapped(map) => {
                let start = usize::try_from(offset).unwrap_or(usize::MAX).min(map.len());
                let count = dst.len().min(map.len() - start);
                dst[..count].copy_from_slice(&map[start..start + count]);
                Ok(count)
            }
            Backing::File { file, len } => {
                let want = usize::try_from(len.saturating_sub(offset)).unwrap_or(usize::MAX).min(dst.len());
                read_file_at(file, offset, &mut dst[..want])
            }
            Backing::Empty => Ok(0),
        }
    }
}

/// Fills `dst` from `offset` of `file` with positional reads, stopping at the end.
fn read_file_at(file: &File, offset: u64, dst: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < dst.len() {
        let position = offset + filled as u64;
        #[cfg(unix)]
        let read = {
            use std::os::unix::fs::FileExt;
            file.read_at(&mut dst[filled..], position)
        };
        #[cfg(windows)]
        let read = {
            use std::os::windows::fs::FileExt;
            file.seek_read(&mut dst[filled..], position)
        };
        #[cfg(not(any(unix, windows)))]
        let read: io::Result<usize> = {
            let _ = (file, position);
            Err(io::Error::new(io::ErrorKind::Unsupported, "positional reads are not supported on this platform"))
        };
        match read {
            Ok(0) => break,
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(filled)
}

/// Writes all of `data` at `offset` without moving the handle's own position (which positional
/// writes on Windows move; the caller re-seeks when it keeps one).
pub(crate) fn write_all_at(file: &File, offset: u64, data: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        file.write_all_at(data, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut written = 0;
        while written < data.len() {
            match file.seek_write(&data[written..], offset + written as u64) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(count) => written += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (file, offset, data);
        Err(io::Error::new(io::ErrorKind::Unsupported, "positional writes are not supported on this platform"))
    }
}

// ---------------------------------------------------------------------------------------------
// Argument checks, each naming the argument it refuses
// ---------------------------------------------------------------------------------------------

/// A count or offset argument that must not be negative.
pub(crate) fn non_negative(what: &str, name: &str, value: Exact<i64>) -> Result<u64> {
    u64::try_from(value.0).map_err(|_| Error::runtime(format!("dream.fs.{what}: {name} {} is negative", value.0)))
}

/// An offset into a file of `size` bytes: at most `size`, so that a read at the end is empty.
pub(crate) fn within_file(what: &str, name: &str, offset: u64, size: u64) -> Result<()> {
    if offset > size {
        return Err(Error::runtime(format!("dream.fs.{what}: {name} {offset} past the end (size {size})")));
    }
    Ok(())
}

/// The window `(offset, length)` of `len` bytes of a buffer or string, with the defaults (0, the
/// space left) and each argument checked against it.
pub(crate) fn window(
    what: &str,
    offset_name: &str,
    noun: &str,
    len: usize,
    offset: Option<Exact<i64>>,
    length: Option<Exact<i64>>,
) -> Result<(usize, usize)> {
    let offset = match offset {
        Some(offset) => usize::try_from(non_negative(what, offset_name, offset)?).unwrap_or(usize::MAX),
        None => 0,
    };
    if offset > len {
        return Err(Error::runtime(format!(
            "dream.fs.{what}: {offset_name} {offset} past the end of the {noun} (size {len})"
        )));
    }
    let space = len - offset;
    let length = match length {
        Some(length) => usize::try_from(non_negative(what, "length", length)?).unwrap_or(usize::MAX),
        None => space,
    };
    if length > space {
        return Err(Error::runtime(format!(
            "dream.fs.{what}: length {length} does not fit the {noun} (space {space} after {offset_name} {offset})"
        )));
    }
    Ok((offset, length))
}

/// The count a read of `length` bytes at `offset` of a file of `size` bytes copies.
fn clamp(offset: u64, length: u64, size: u64) -> usize {
    usize::try_from(length.min(size - offset)).unwrap_or(usize::MAX)
}

/// The slice of `data` the `offset` and `length` arguments select.
fn data_window<'d>(
    what: &str,
    data: &'d BytesView<'d>,
    offset: Option<Exact<i64>>,
    length: Option<Exact<i64>>,
) -> Result<&'d [u8]> {
    let (offset, length) = window(what, "offset", "data", data.len(), offset, length)?;
    // SAFETY: a string's bytes are immutable; a buffer's are read once here and written to a
    // file with no call into Lua in between, through this call's only view of them.
    Ok(&unsafe { data.bytes_unchecked() }[offset..offset + length])
}

/// `fs.readAt(path, offset, length)`: a new buffer of the bytes there, fewer at the end.
pub(crate) fn read_at(
    call: &Call<'_>,
    path: &[u8],
    offset: Exact<i64>,
    length: Exact<i64>,
) -> Result<Outcome<StackResults>> {
    let offset = non_negative("readAt", "offset", offset)?;
    let length = non_negative("readAt", "length", length)?;
    let host = super::host_path("readAt", path)?;
    let file = done!(Outcome::of(open_regular(&host), "readAt", path));
    read_at_file(call, &file, path, offset, length, "readAt")
}

fn read_at_file(
    call: &Call<'_>,
    file: &File,
    path: &[u8],
    offset: u64,
    length: u64,
    what: &str,
) -> Result<Outcome<StackResults>> {
    let size = done!(Outcome::of(file.metadata().map(|metadata| metadata.len()), what, path));
    within_file(what, "offset", offset, size)?;
    let expected = clamp(offset, length, size);
    let mut buffer = new_buffer(call, expected)?;
    // SAFETY: the buffer was created by this call and no other view of it exists; the read never
    // calls into Lua. A failure's results go above the buffer, which the call drops.
    let count = done!(Outcome::of(read_file_at(file, offset, unsafe { buffer.bytes_mut_unchecked() }), what, path));
    if count != expected {
        let error = io::Error::new(io::ErrorKind::UnexpectedEof, "file changed during positional read");
        return Ok(Outcome::Failed(super::Failure::new(what, path, &error)));
    }
    Ok(Outcome::Done(StackResults))
}

/// A retained file handle for positional reads; unlike `Reader`, it is never memory mapped.
#[derive(Debug)]
pub(crate) struct PositionalReader {
    file: RefCell<Option<File>>,
    name: Box<[u8]>,
}

// SAFETY: the userdata owns only a file handle and bytes; dropping it makes no Lua API calls.
unsafe impl Userdata for PositionalReader {
    const NAME: &'static str = "dream.fs.PositionalReader";
}

impl PositionalReader {
    pub(crate) fn open(path: &[u8]) -> Result<Outcome<Self>> {
        let host = super::host_path("openPositional", path)?;
        let file = done!(Outcome::of(open_regular(&host), "openPositional", path));
        Ok(Outcome::Done(Self { file: RefCell::new(Some(file)), name: path.into() }))
    }
}

fn positional_read_at(
    reader: &PositionalReader,
    call: &Call<'_>,
    offset: Exact<i64>,
    length: Exact<i64>,
) -> Result<Outcome<StackResults>> {
    let offset = non_negative("PositionalReader.readAt", "offset", offset)?;
    let length = non_negative("PositionalReader.readAt", "length", length)?;
    let guard = reader.file.borrow();
    let file = guard.as_ref().ok_or_else(|| {
        Error::runtime(format!("dream.fs.PositionalReader.readAt: the reader over {} is closed", display(&reader.name)))
    })?;
    read_at_file(call, file, &reader.name, offset, length, "PositionalReader.readAt")
}

fn positional_size(reader: &PositionalReader) -> Result<Outcome<f64>> {
    let guard = reader.file.borrow();
    let file = guard.as_ref().ok_or_else(|| {
        Error::runtime(format!("dream.fs.PositionalReader.size: the reader over {} is closed", display(&reader.name)))
    })?;
    Ok(Outcome::of(file.metadata().map(|metadata| metadata.len() as f64), "PositionalReader.size", &reader.name))
}

pub(crate) fn describe_positional_reader(d: &mut ExtensionDescriptor) {
    let mut reader = d.userdata::<PositionalReader>(PositionalReader::NAME);
    reader.tag(TagPolicy::Never).doc("A retained, unmapped file handle for positional reads.");
    reader
        .method("readAt", positional_read_at)
        .signature("(self, offset: number, length: number): (buffer?, string?, dream_fs_ErrorKind?)")
        .doc("Reads from offset without changing a shared position; concurrent truncation may return an error.");
    reader
        .method("size", positional_size)
        .signature("(self): (number?, string?, dream_fs_ErrorKind?)")
        .doc("Returns the current size of the open file.");
    reader
        .method("close", |r: &PositionalReader| {
            r.file.borrow_mut().take();
        })
        .signature("(self)")
        .doc("Closes the handle; later operations raise. Closing twice does nothing.");
    reader.metamethod("__tostring", |r: &PositionalReader| format!("dream.fs.PositionalReader({})", display(&r.name)));
}

// ---------------------------------------------------------------------------------------------
// Reader
// ---------------------------------------------------------------------------------------------

/// `dream.fs.Reader`: a position over the map (or handle) of one file.
#[derive(Debug)]
pub struct Reader {
    backing: RefCell<Option<Arc<Backing>>>,
    size: u64,
    position: Cell<u64>,
    name: Box<[u8]>,
}

// SAFETY: a map or a file handle behind an `Arc` and two cells; no Lua references, and dropping
// unmaps or closes without any Lua API.
unsafe impl Userdata for Reader {
    const NAME: &'static str = "dream.fs.Reader";
}

impl Reader {
    /// A reader at position 0 over the file at `path`.
    pub(crate) fn open(path: &[u8]) -> Result<Outcome<Reader>> {
        let host = super::host_path("open", path)?;
        let backing = done!(Outcome::of(Backing::open(&host), "open", path));
        Ok(Outcome::Done(Reader {
            size: backing.len(),
            backing: RefCell::new(Some(Arc::new(backing))),
            position: Cell::new(0),
            name: path.into(),
        }))
    }

    fn backing(&self, what: &str) -> Result<Arc<Backing>> {
        self.backing.borrow().clone().ok_or_else(|| {
            Error::runtime(format!("dream.fs.Reader.{what}: the reader over {} is closed", display(&self.name)))
        })
    }

    /// Copies up to `dst.len()` bytes from `offset`; only a reader that could not map its file
    /// reads through the OS, so only it can fail.
    fn read_into(&self, what: &str, offset: u64, dst: &mut [u8]) -> Result<Outcome<usize>> {
        let read = self.backing(what)?.read_at(offset, dst);
        Ok(Outcome::of(read, &format!("Reader.{what}"), &self.name))
    }

    /// Copies up to `dst.len()` bytes at the position and advances past them.
    fn read_next(&self, what: &str, dst: &mut [u8]) -> Result<Outcome<usize>> {
        let position = self.position.get();
        let count = done!(self.read_into(what, position, dst)?);
        self.position.set(position + count as u64);
        Ok(Outcome::Done(count))
    }

    fn remaining(&self) -> u64 {
        self.size.saturating_sub(self.position.get())
    }
}

fn reader_read(reader: &Reader, call: &Call<'_>, length: Exact<i64>) -> Result<Outcome<StackResults>> {
    let length = non_negative("Reader.read", "length", length)?;
    reader.backing("read")?;
    let mut buffer = new_buffer(call, usize::try_from(length.min(reader.remaining())).unwrap_or(usize::MAX))?;
    // SAFETY: the buffer was created by this call and no other view of it exists; the copy never
    // calls into Lua.
    done!(reader.read_next("read", unsafe { buffer.bytes_mut_unchecked() })?);
    Ok(Outcome::Done(StackResults))
}

/// A count of bytes as the number a script gets.
fn counted(outcome: Outcome<usize>) -> Outcome<f64> {
    match outcome {
        Outcome::Done(count) => Outcome::Done(count as f64),
        Outcome::Failed(failure) => Outcome::Failed(failure),
    }
}

fn reader_read_into(
    reader: &Reader,
    mut buffer: BufferView<'_>,
    buffer_offset: Option<Exact<i64>>,
    length: Option<Exact<i64>>,
) -> Result<Outcome<f64>> {
    let (buffer_offset, length) =
        window("Reader.readInto", "bufferOffset", "buffer", buffer.len(), buffer_offset, length)?;
    let want = usize::try_from((length as u64).min(reader.remaining())).unwrap_or(usize::MAX);
    // SAFETY: the window is inside the buffer, this call holds the only view, and the copy
    // never calls into Lua.
    let dst = unsafe { &mut buffer.bytes_mut_unchecked()[buffer_offset..buffer_offset + want] };
    reader.read_next("readInto", dst).map(counted)
}

fn reader_read_at(
    reader: &Reader,
    call: &Call<'_>,
    position: Exact<i64>,
    length: Exact<i64>,
) -> Result<Outcome<StackResults>> {
    let position = non_negative("Reader.readAt", "position", position)?;
    let length = non_negative("Reader.readAt", "length", length)?;
    within_file("Reader.readAt", "position", position, reader.size)?;
    reader.backing("readAt")?;
    let mut buffer = new_buffer(call, clamp(position, length, reader.size))?;
    // SAFETY: as `reader_read`.
    done!(reader.read_into("readAt", position, unsafe { buffer.bytes_mut_unchecked() })?);
    Ok(Outcome::Done(StackResults))
}

fn reader_read_at_into(
    reader: &Reader,
    mut buffer: BufferView<'_>,
    position: Exact<i64>,
    length: Option<Exact<i64>>,
    buffer_offset: Option<Exact<i64>>,
) -> Result<Outcome<f64>> {
    let (buffer_offset, length) =
        window("Reader.readAtInto", "bufferOffset", "buffer", buffer.len(), buffer_offset, length)?;
    let position = non_negative("Reader.readAtInto", "position", position)?;
    within_file("Reader.readAtInto", "position", position, reader.size)?;
    let want = clamp(position, length as u64, reader.size);
    // SAFETY: as `reader_read_into`.
    let dst = unsafe { &mut buffer.bytes_mut_unchecked()[buffer_offset..buffer_offset + want] };
    reader.read_into("readAtInto", position, dst).map(counted)
}

fn reader_seek(reader: &Reader, position: Exact<i64>) -> Result<()> {
    let position = non_negative("Reader.seek", "position", position)?;
    within_file("Reader.seek", "position", position, reader.size)?;
    reader.backing("seek")?;
    reader.position.set(position);
    Ok(())
}

fn reader_skip(reader: &Reader, count: Exact<i64>) -> Result<()> {
    let count = non_negative("Reader.skip", "count", count)?;
    reader.backing("skip")?;
    if count > reader.remaining() {
        return Err(Error::runtime(format!(
            "dream.fs.Reader.skip: count {count} past the end (position {}, size {})",
            reader.position.get(),
            reader.size
        )));
    }
    reader.position.set(reader.position.get() + count);
    Ok(())
}

pub(crate) fn describe_reader(d: &mut ExtensionDescriptor) {
    let mut reader = d.userdata::<Reader>(Reader::NAME);
    reader.tag(TagPolicy::Preferred).doc(
        "A position over the memory map of one file. A read returns nil, the message and the kind only when the file could not be mapped and the OS refuses the read.",
    );
    reader
        .method("read", reader_read)
        .signature("(self, length: number): (buffer?, string?, dream_fs_ErrorKind?)")
        .doc("The next length bytes as a new buffer, fewer at the end; advances past them.");
    reader
        .method("readInto", reader_read_into)
        .signature("(self, target: buffer, bufferOffset: number?, length: number?): (number?, string?, dream_fs_ErrorKind?)")
        .doc("Copies the next bytes into target at bufferOffset (default 0), at most length (default the space left); returns the count and advances past them.");
    reader
        .method("readAt", reader_read_at)
        .signature("(self, position: number, length: number): (buffer?, string?, dream_fs_ErrorKind?)")
        .doc("length bytes from position as a new buffer, fewer at the end; the position does not move.");
    reader
        .method("readAtInto", reader_read_at_into)
        .signature("(self, target: buffer, position: number, length: number?, bufferOffset: number?): (number?, string?, dream_fs_ErrorKind?)")
        .doc("Copies bytes from position into target at bufferOffset; returns the count. The position does not move.");
    reader
        .method("seek", reader_seek)
        .signature("(self, position: number)")
        .doc("Moves to position, 0 to size inclusive.");
    reader
        .method("skip", reader_skip)
        .signature("(self, count: number)")
        .doc("Moves count bytes forward, at most to the end.");
    reader.method("tell", |r: &Reader| r.position.get() as f64).signature("(self): number");
    reader.method("size", |r: &Reader| r.size as f64).signature("(self): number");
    reader
        .method("close", |r: &Reader| {
            r.backing.borrow_mut().take();
        })
        .signature("(self)")
        .doc("Releases the map; every later read is an error. Closing twice does nothing.");
    reader.metamethod("__tostring", |r: &Reader| format!("dream.fs.Reader({})", display(&r.name)));
}

// ---------------------------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------------------------

/// How `fs.openWrite` opens a file: `{ append?, truncate?, create? }`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OpenWrite {
    append: bool,
    /// `None` is the default: truncate unless appending.
    truncate: Option<bool>,
    create: bool,
    exclusive: bool,
    write_through: bool,
}

impl OpenWrite {
    pub(crate) fn read(scope: &impl Scope, options: Option<ValueView<'_>>) -> Result<OpenWrite> {
        let mut open =
            OpenWrite { append: false, truncate: None, create: true, exclusive: false, write_through: false };
        let Some(options) = options.filter(|view| !view.is_nil()) else {
            return Ok(open);
        };
        Options::read(scope, options, "dream.fs.openWrite", |o| {
            open.append = o.or("append", false)?;
            open.truncate = o.optional("truncate")?;
            open.create = o.or("create", true)?;
            open.exclusive = o.or("exclusive", false)?;
            open.write_through = o.or("writeThrough", false)?;
            if open.exclusive && !open.create {
                return Err(Error::runtime("dream.fs.openWrite: exclusive needs create"));
            }
            Ok(open)
        })
    }

    fn open(self, path: &Path) -> io::Result<(File, u64)> {
        let truncate = self.truncate.unwrap_or(!self.append);
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create(self.create && !self.exclusive)
            .create_new(self.exclusive)
            .append(self.append)
            .truncate(truncate && !self.append);
        if self.write_through {
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.custom_flags(libc::O_SYNC);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                const FILE_FLAG_WRITE_THROUGH: u32 = 0x8000_0000;
                options.custom_flags(FILE_FLAG_WRITE_THROUGH);
            }
            #[cfg(not(any(unix, windows)))]
            return Err(io::Error::new(io::ErrorKind::Unsupported, "write-through is unsupported on this platform"));
        }
        let file = options.open(path)?;
        let position = if self.append { file.metadata()?.len() } else { 0 };
        Ok((file, position))
    }
}

/// How `fs.writeFile` writes: `{ offset?, append?, create?, exclusive?, atomic?, createParents? }`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WriteFile {
    offset: Option<u64>,
    append: bool,
    create: bool,
    exclusive: bool,
    atomic: bool,
    create_parents: bool,
}

impl WriteFile {
    pub(crate) fn read(scope: &impl Scope, options: Option<ValueView<'_>>) -> Result<WriteFile> {
        let mut write = WriteFile {
            offset: None,
            append: false,
            create: true,
            exclusive: false,
            atomic: false,
            create_parents: false,
        };
        let Some(options) = options.filter(|view| !view.is_nil()) else {
            return Ok(write);
        };
        Options::read(scope, options, "dream.fs.writeFile", |o| {
            if let Some(offset) = o.optional::<Exact<i64>>("offset")? {
                write.offset = Some(non_negative("writeFile", "offset", offset)?);
            }
            write.append = o.or("append", false)?;
            write.create = o.or("create", true)?;
            write.exclusive = o.or("exclusive", false)?;
            write.atomic = o.or("atomic", false)?;
            write.create_parents = o.or("createParents", false)?;
            Ok(())
        })?;
        if write.offset.is_some() && write.append {
            return Err(Error::runtime("dream.fs.writeFile: offset and append cannot be combined"));
        }
        if write.exclusive && !write.create {
            return Err(Error::runtime("dream.fs.writeFile: exclusive needs create"));
        }
        if write.atomic && (write.offset.is_some() || write.append || write.exclusive || !write.create) {
            return Err(Error::runtime(
                "dream.fs.writeFile: atomic replaces the whole file, so it takes no offset, append, exclusive or create = false",
            ));
        }
        Ok(write)
    }

    /// Writes `data` to `path` as the options say.
    pub(crate) fn write(self, path: &Path, data: &[u8]) -> io::Result<()> {
        if self.create_parents
            && let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        if self.atomic {
            return write_atomic(path, data);
        }
        let mut open = OpenOptions::new();
        open.write(true);
        if self.exclusive {
            open.create_new(true);
        } else {
            open.create(self.create);
        }
        if self.append {
            open.append(true);
        } else if self.offset.is_none() {
            open.truncate(true);
        }
        let mut file = open.open(path)?;
        match self.offset {
            Some(offset) => write_all_at(&file, offset, data),
            None => write_complete(&mut file, data),
        }
    }
}

/// Replaces `path` with `data` through a sibling temporary renamed over it, so a reader sees
/// the old file or the new one and never part of either. An existing file's permissions carry
/// over; a new one gets what creating it directly would have given it.
fn write_atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let name = path.file_name().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let permissions = match std::fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let (temporary, mut file) = loop {
        let mut candidate = std::ffi::OsString::from(".");
        candidate.push(name);
        candidate.push(format!(".{}.{}.tmp", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let candidate = parent.join(candidate);
        match OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(file) => break (candidate, file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    };
    let written = file
        .write_all(data)
        .and_then(|()| permissions.map_or(Ok(()), |permissions| file.set_permissions(permissions)))
        .and_then(|()| file.sync_all());
    drop(file);
    let result = written.and_then(|()| std::fs::rename(&temporary, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

// This shared path uses Write::write_all's interrupted and short-write handling.
fn write_complete(writer: &mut impl Write, data: &[u8]) -> io::Result<()> {
    writer.write_all(data)
}

#[cfg(test)]
thread_local! {
    static INJECT_SYNC_ERROR: Cell<Option<i32>> = const { Cell::new(None) };
    static INJECT_PARTIAL_WRITE: Cell<bool> = const { Cell::new(false) };
}

fn sync_file(file: &File, data_only: bool) -> io::Result<()> {
    #[cfg(test)]
    if let Some(code) = INJECT_SYNC_ERROR.with(Cell::take) {
        return Err(io::Error::from_raw_os_error(code));
    }
    // Rust's Apple implementation uses F_FULLFSYNC for both methods, with no fsync fallback.
    if data_only { file.sync_data() } else { file.sync_all() }
}

/// `fs.writeFile(path, data, options?)`: the count written.
pub(crate) fn write_file(
    call: &Call<'_>,
    path: &[u8],
    data: BytesView<'_>,
    options: Option<ValueView<'_>>,
) -> Result<Outcome<f64>> {
    let write = WriteFile::read(call, options)?;
    let host = super::host_path("writeFile", path)?;
    // SAFETY: the bytes go straight to the file, with no call into Lua while the slice lives,
    // through this call's only view of them.
    let bytes = unsafe { data.bytes_unchecked() };
    Ok(Outcome::of(write.write(&host, bytes).map(|()| bytes.len() as f64), "writeFile", path))
}

/// A directory handle to synchronize. `O_DIRECTORY` refuses anything else with `ENOTDIR`, where a
/// plain open of a FIFO would block the thread until something opened its other end.
#[cfg(unix)]
pub(super) fn open_directory(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NONBLOCK).open(path)
}

/// Creates a new file, writes all bytes, synchronizes it and then its containing directory.
/// An error after creation leaves the path in an uncertain state for the caller to recover.
pub(crate) fn create_durable(path: &[u8], data: BytesView<'_>) -> Result<Outcome<f64>> {
    let host = super::host_path("createDurable", path)?;
    // SAFETY: the bytes are read only during this native call, without calling into Lua.
    let bytes = unsafe { data.bytes_unchecked() };
    #[cfg(unix)]
    let result = (|| -> io::Result<()> {
        let parent = host.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let directory = open_directory(parent)?;
        let mut file = OpenOptions::new().write(true).create_new(true).open(&host)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        directory.sync_all()
    })();
    #[cfg(not(unix))]
    let result: io::Result<()> = {
        let _ = &host;
        Err(io::Error::new(io::ErrorKind::Unsupported, "durable file creation is unsupported on this platform"))
    };
    Ok(Outcome::of(result.map(|()| bytes.len() as f64), "createDurable", path))
}

/// A writer's open file: `None` once closed.
#[derive(Debug)]
pub(crate) struct WriterFile(RefCell<Option<BufWriter<File>>>);

impl crate::runtime::shared::Resource for WriterFile {
    fn close_for_shutdown(&self) {
        if let Some(mut file) = self.0.borrow_mut().take()
            && file.flush().is_err()
        {
            let _ = file.into_parts();
        }
    }
}

/// `dream.fs.Writer`: a buffered writer over one file.
pub struct Writer {
    /// Shared with the runtime's resource list, so `Runtime::shutdown` can flush it.
    file: Rc<WriterFile>,
    position: Cell<u64>,
    append: bool,
    name: Box<[u8]>,
    failure: RefCell<Option<super::Failure>>,
}

impl std::fmt::Debug for Writer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Writer")
            .field("name", &display(&self.name))
            .field("position", &self.position.get())
            .field("open", &self.file.0.borrow().is_some())
            .finish_non_exhaustive()
    }
}

// SAFETY: a file handle behind a buffer and plain data; no Lua references. Dropping flushes the
// buffer and closes the handle with no Lua API.
unsafe impl Userdata for Writer {
    const NAME: &'static str = "dream.fs.Writer";
}

impl Writer {
    pub(crate) fn open(path: &[u8], options: OpenWrite) -> Result<Outcome<Writer>> {
        let host = super::host_path("openWrite", path)?;
        let (file, position) = done!(Outcome::of(options.open(&host), "openWrite", path));
        Ok(Outcome::Done(Writer {
            file: Rc::new(WriterFile(RefCell::new(Some(BufWriter::new(file))))),
            position: Cell::new(position),
            append: options.append,
            name: path.into(),
            failure: RefCell::new(None),
        }))
    }

    /// The open file, for the runtime's resource list.
    pub(crate) fn resource(&self) -> std::rc::Weak<dyn crate::runtime::shared::Resource> {
        let file: Rc<dyn crate::runtime::shared::Resource> = self.file.clone();
        Rc::downgrade(&file)
    }

    /// Runs `body` on the open file: a closed writer is the script's mistake and raises; the OS
    /// refusing is an outcome.
    fn with_file<R>(&self, what: &str, body: impl FnOnce(&mut BufWriter<File>) -> io::Result<R>) -> Result<Outcome<R>> {
        if let Some(failure) = self.failure.borrow().as_ref() {
            return Ok(Outcome::Failed(failure.clone()));
        }
        let mut slot = self.file.0.borrow_mut();
        let Some(file) = slot.as_mut() else {
            return Err(Error::runtime(format!(
                "dream.fs.Writer.{what}: the writer over {} is closed",
                display(&self.name)
            )));
        };
        match body(file) {
            Ok(value) => Ok(Outcome::Done(value)),
            Err(error) => {
                let failure = super::Failure::new(&format!("Writer.{what}"), &self.name, &error);
                // Discard buffered bytes without another write: their outcome is uncertain.
                if let Some(file) = slot.take() {
                    let _ = file.into_parts();
                }
                *self.failure.borrow_mut() = Some(failure.clone());
                Ok(Outcome::Failed(failure))
            }
        }
    }

    fn write(&self, data: &[u8]) -> Result<Outcome<f64>> {
        done!(self.with_file("write", |file| {
            #[cfg(test)]
            if INJECT_PARTIAL_WRITE.with(|slot| slot.replace(false)) {
                write_complete(file, &data[..data.len().min(1)])?;
                file.flush()?;
                return Err(io::Error::from_raw_os_error(5));
            }
            write_complete(file, data)
        })?);
        self.position.set(self.position.get() + data.len() as u64);
        Ok(Outcome::Done(data.len() as f64))
    }

    fn write_at(&self, position: u64, data: &[u8]) -> Result<Outcome<f64>> {
        if self.append {
            return Err(Error::runtime(format!(
                "dream.fs.Writer.writeAt: the writer over {} appends, so it cannot write at a position",
                display(&self.name)
            )));
        }
        let current = self.position.get();
        done!(self.with_file("writeAt", |file| {
            file.flush()?;
            write_all_at(file.get_ref(), position, data)?;
            file.get_mut().seek(SeekFrom::Start(current)).map(drop)
        })?);
        Ok(Outcome::Done(data.len() as f64))
    }

    fn seek(&self, position: u64) -> Result<Outcome<bool>> {
        done!(self.with_file("seek", |file| file.seek(SeekFrom::Start(position)).map(drop))?);
        self.position.set(position);
        Ok(Outcome::Done(true))
    }

    fn truncate(&self, length: u64) -> Result<Outcome<bool>> {
        let position = self.position.get().min(length);
        done!(self.with_file("truncate", |file| {
            file.flush()?;
            file.get_ref().set_len(length)?;
            file.get_mut().seek(SeekFrom::Start(position)).map(drop)
        })?);
        self.position.set(position);
        Ok(Outcome::Done(true))
    }

    fn sync(&self, data_only: bool) -> Result<Outcome<bool>> {
        let what = if data_only { "syncData" } else { "syncAll" };
        let outcome = self.with_file(what, |file| {
            file.flush()?;
            sync_file(file.get_ref(), data_only)
        })?;
        Ok(match outcome {
            Outcome::Done(()) => Outcome::Done(true),
            Outcome::Failed(failure) => Outcome::Failed(failure),
        })
    }

    /// Flushes and closes; an I/O failure remains observable on every close.
    fn close(&self) -> Outcome<bool> {
        if let Some(failure) = self.failure.borrow().as_ref() {
            return Outcome::Failed(failure.clone());
        }
        let Some(mut file) = self.file.0.borrow_mut().take() else {
            return Outcome::Done(true);
        };
        match file.flush() {
            Ok(()) => Outcome::Done(true),
            Err(error) => {
                let _ = file.into_parts();
                let failure = super::Failure::new("Writer.close", &self.name, &error);
                *self.failure.borrow_mut() = Some(failure.clone());
                Outcome::Failed(failure)
            }
        }
    }
}

pub(crate) fn describe_writer(d: &mut ExtensionDescriptor) {
    let mut writer = d.userdata::<Writer>(Writer::NAME);
    writer.tag(TagPolicy::Never).doc(
        "A buffered writer over one file. A write the OS refuses returns nil, the message and the kind; a closed writer raises.",
    );
    writer
        .method(
            "write",
            |w: &Writer, data: BytesView<'_>, offset: Option<Exact<i64>>, length: Option<Exact<i64>>| {
                let bytes = data_window("Writer.write", &data, offset, length)?;
                w.write(bytes)
            },
        )
        .signature("(self, data: buffer | string, offset: number?, length: number?): (number?, string?, dream_fs_ErrorKind?)")
        .doc("Writes data, or its slice from offset of length bytes, at the position and advances past it; returns the count.");
    writer
        .method(
            "writeAt",
            |w: &Writer,
             position: Exact<i64>,
             data: BytesView<'_>,
             offset: Option<Exact<i64>>,
             length: Option<Exact<i64>>| {
                let position = non_negative("Writer.writeAt", "position", position)?;
                let bytes = data_window("Writer.writeAt", &data, offset, length)?;
                w.write_at(position, bytes)
            },
        )
        .signature("(self, position: number, data: buffer | string, offset: number?, length: number?): (number?, string?, dream_fs_ErrorKind?)")
        .doc("Writes data at position without moving; an error on an append writer.");
    writer
        .method("seek", |w: &Writer, position: Exact<i64>| w.seek(non_negative("Writer.seek", "position", position)?))
        .signature("(self, position: number): (boolean?, string?, dream_fs_ErrorKind?)")
        .doc("Moves to position; past the end, the next write extends the file.");
    writer.method("tell", |w: &Writer| w.position.get() as f64).signature("(self): number");
    writer
        .method("flush", |w: &Writer| -> Result<Outcome<bool>> {
            Ok(match w.with_file("flush", Write::flush)? {
                Outcome::Done(()) => Outcome::Done(true),
                Outcome::Failed(failure) => Outcome::Failed(failure),
            })
        })
        .signature("(self): (boolean?, string?, dream_fs_ErrorKind?)");
    writer
        .method("syncData", |w: &Writer| w.sync(true))
        .signature("(self): (boolean?, string?, dream_fs_ErrorKind?)")
        .doc("Flushes buffered bytes and synchronizes file data and retrieval metadata; propagates failures.");
    writer
        .method("syncAll", |w: &Writer| w.sync(false))
        .signature("(self): (boolean?, string?, dream_fs_ErrorKind?)")
        .doc("Flushes buffered bytes and synchronizes file data and metadata; propagates failures.");
    writer
        .method("truncate", |w: &Writer, length: Exact<i64>| {
            w.truncate(non_negative("Writer.truncate", "length", length)?)
        })
        .signature("(self, length: number): (boolean?, string?, dream_fs_ErrorKind?)")
        .doc("Cuts or extends the file to length bytes; a position past it moves to it.");
    writer
        .method("close", Writer::close)
        .signature("(self): (boolean?, string?, dream_fs_ErrorKind?)")
        .doc("Flushes and closes; repeats any I/O failure on later closes.");
    writer.metamethod("__tostring", |w: &Writer| format!("dream.fs.Writer({})", display(&w.name)));
}

#[cfg(test)]
mod durability_tests {
    use super::*;

    struct ShortWriter {
        bytes: Vec<u8>,
        calls: usize,
        fail: bool,
    }
    impl Write for ShortWriter {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.calls == 1 {
                return Err(io::Error::from(io::ErrorKind::Interrupted));
            }
            if self.fail {
                return Ok(0);
            }
            let count = data.len().min(2);
            self.bytes.extend_from_slice(&data[..count]);
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn positional_read_handles_a_file_truncated_after_open() {
        let path = std::env::temp_dir().join(format!("l3i-positional-truncate-{}", std::process::id()));
        std::fs::write(&path, b"original").unwrap();
        let reader = open_regular(&path).unwrap();
        let mut buffer = [0_u8; 8];
        assert_eq!(read_file_at(&reader, 0, &mut buffer).unwrap(), 8);
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(0).unwrap();
        assert_eq!(read_file_at(&reader, 0, &mut buffer).unwrap(), 0);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn complete_writes_retry_interruptions_and_short_writes() {
        let mut writer = ShortWriter { bytes: Vec::new(), calls: 0, fail: false };
        write_complete(&mut writer, b"abcdef").unwrap();
        assert_eq!(writer.bytes, b"abcdef");
        assert_eq!(writer.calls, 4);
        writer.fail = true;
        assert_eq!(write_complete(&mut writer, b"x").unwrap_err().kind(), io::ErrorKind::WriteZero);
    }

    #[test]
    fn partial_write_poison_discards_buffer_and_rejects_further_mutation() {
        use crate::extension::{RuntimePlan, RuntimePolicy};
        use crate::fs::{FsExtension, READ_CAPABILITY, WRITE_CAPABILITY};
        let path = std::env::temp_dir().join(format!("l3i-partial-write-{}", std::process::id()));
        let policy = RuntimePolicy::new()
            .compat_global("@dream/fs", "fs")
            .capability(READ_CAPABILITY)
            .capability(WRITE_CAPABILITY);
        let plan = RuntimePlan::builder().policy(policy).extension(FsExtension).finalize().unwrap();
        let runtime = crate::Runtime::from_plan(&plan).unwrap();
        INJECT_PARTIAL_WRITE.with(|slot| slot.set(true));
        runtime
            .exec(&format!(
                "local w = assert(fs.openWrite({:?})) local ok, message = w:write('record') \
             assert(ok == nil and message:find('Writer.write')) \
             local later, reason = w:write('later') assert(later == nil and reason == message) \
             later, reason = w:truncate(0) assert(later == nil and reason == message) \
             later, reason = w:syncAll() assert(later == nil and reason == message) \
             local closed, closemsg = w:close() assert(closed == nil and closemsg == message) \
             closed, closemsg = w:close() assert(closed == nil and closemsg == message)",
                path.to_string_lossy()
            ))
            .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"r");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn injected_sync_failure_reaches_luau_and_close() {
        use crate::extension::{RuntimePlan, RuntimePolicy};
        use crate::fs::{FsExtension, READ_CAPABILITY, WRITE_CAPABILITY};
        let path = std::env::temp_dir().join(format!("l3i-sync-error-{}", std::process::id()));
        let policy = RuntimePolicy::new()
            .compat_global("@dream/fs", "fs")
            .capability(READ_CAPABILITY)
            .capability(WRITE_CAPABILITY);
        let plan = RuntimePlan::builder().policy(policy).extension(FsExtension).finalize().unwrap();
        let runtime = crate::Runtime::from_plan(&plan).unwrap();
        INJECT_SYNC_ERROR.with(|slot| slot.set(Some(5)));
        runtime
            .exec(&format!(
                "local w = assert(fs.openWrite({:?})) assert(w:write('record') == 6) \
             local ok, msg, kind = w:syncAll() \
             assert(ok == nil and kind ~= nil and msg:find('Writer.syncAll') and msg:find('os error 5')) \
             local later, latermsg = w:write('later') assert(later == nil and latermsg == msg) \
             later, latermsg = w:syncAll() assert(later == nil and latermsg == msg) \
             local closed, closemsg = w:close() assert(closed == nil and closemsg == msg) \
             closed, closemsg = w:close() assert(closed == nil and closemsg == msg)",
                path.to_string_lossy()
            ))
            .unwrap();
        std::fs::remove_file(path).unwrap();
    }
}
