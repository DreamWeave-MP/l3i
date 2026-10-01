//! Positional reads, `dream.fs.Reader` and `dream.fs.Writer`: the handles `fs.open` and
//! `fs.openWrite` return. A reader keeps a memory map of its file (or, when the file cannot be
//! mapped, the open handle for positional reads), so a parse that reads piece by piece copies
//! each piece once and allocates nothing per call.

use std::cell::{Cell, RefCell};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

use crate::bind::{Call, StackResults};
use crate::convert::{BufferView, BytesView, Exact, new_buffer};
use crate::error::{Error, Result};
use crate::extension::{ExtensionDescriptor, TagPolicy};
use crate::options::Options;
use crate::stack::{Scope, ValueView};
use crate::userdata::Userdata;

use super::{Outcome, display, done};

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

impl Backing {
    /// Maps the file at `path`, or opens it for positional reads when mapping fails.
    pub(crate) fn open(path: &Path) -> io::Result<Backing> {
        let file = File::open(path)?;
        let metadata = file.metadata()?;
        if metadata.is_dir() {
            // What reading it would fail with later, said now: EISDIR is 21 on Linux and macOS.
            #[cfg(unix)]
            return Err(io::Error::from_raw_os_error(21));
            #[cfg(not(unix))]
            return Err(io::Error::new(io::ErrorKind::IsADirectory, "Is a directory"));
        }
        let len = metadata.len();
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
    let backing = done!(Outcome::of(Backing::open(&host), "readAt", path));
    within_file("readAt", "offset", offset, backing.len())?;
    let mut buffer = new_buffer(call, clamp(offset, length, backing.len()))?;
    // SAFETY: the buffer was created by this call and no other view of it exists; the copy
    // never calls into Lua. A failure's results go above the buffer, which the call drops.
    let read = backing.read_at(offset, unsafe { buffer.bytes_mut_unchecked() });
    done!(Outcome::of(read, "readAt", path));
    Ok(Outcome::Done(StackResults))
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
}

impl OpenWrite {
    pub(crate) fn read(scope: &impl Scope, options: Option<ValueView<'_>>) -> Result<OpenWrite> {
        let mut open = OpenWrite { append: false, truncate: None, create: true };
        let Some(options) = options.filter(|view| !view.is_nil()) else {
            return Ok(open);
        };
        Options::read(scope, options, "dream.fs.openWrite", |o| {
            open.append = o.or("append", false)?;
            open.truncate = o.optional("truncate")?;
            open.create = o.or("create", true)?;
            Ok(open)
        })
    }

    fn open(self, path: &Path) -> io::Result<(File, u64)> {
        let truncate = self.truncate.unwrap_or(!self.append);
        let file = OpenOptions::new()
            .write(true)
            .create(self.create)
            .append(self.append)
            .truncate(truncate && !self.append)
            .open(path)?;
        let position = if self.append { file.metadata()?.len() } else { 0 };
        Ok((file, position))
    }
}

/// How `fs.writeFile` writes: `{ offset?, append?, create? }`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WriteFile {
    offset: Option<u64>,
    append: bool,
    create: bool,
}

impl WriteFile {
    pub(crate) fn read(scope: &impl Scope, options: Option<ValueView<'_>>) -> Result<WriteFile> {
        let mut write = WriteFile { offset: None, append: false, create: true };
        let Some(options) = options.filter(|view| !view.is_nil()) else {
            return Ok(write);
        };
        Options::read(scope, options, "dream.fs.writeFile", |o| {
            if let Some(offset) = o.optional::<Exact<i64>>("offset")? {
                write.offset = Some(non_negative("writeFile", "offset", offset)?);
            }
            write.append = o.or("append", false)?;
            write.create = o.or("create", true)?;
            Ok(())
        })?;
        if write.offset.is_some() && write.append {
            return Err(Error::runtime("dream.fs.writeFile: offset and append cannot be combined"));
        }
        Ok(write)
    }

    /// Writes `data` to `path` as the options say.
    pub(crate) fn write(self, path: &Path, data: &[u8]) -> io::Result<()> {
        let mut open = OpenOptions::new();
        open.write(true).create(self.create);
        if self.append {
            open.append(true);
        } else if self.offset.is_none() {
            open.truncate(true);
        }
        let mut file = open.open(path)?;
        match self.offset {
            Some(offset) => write_all_at(&file, offset, data),
            None => file.write_all(data),
        }
    }
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

/// `dream.fs.Writer`: a buffered writer over one file.
pub struct Writer {
    file: RefCell<Option<BufWriter<File>>>,
    position: Cell<u64>,
    append: bool,
    name: Box<[u8]>,
}

impl std::fmt::Debug for Writer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Writer")
            .field("name", &display(&self.name))
            .field("position", &self.position.get())
            .field("open", &self.file.borrow().is_some())
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
            file: RefCell::new(Some(BufWriter::new(file))),
            position: Cell::new(position),
            append: options.append,
            name: path.into(),
        }))
    }

    /// Runs `body` on the open file: a closed writer is the script's mistake and raises; the OS
    /// refusing is an outcome.
    fn with_file<R>(&self, what: &str, body: impl FnOnce(&mut BufWriter<File>) -> io::Result<R>) -> Result<Outcome<R>> {
        let mut file = self.file.borrow_mut();
        let Some(file) = file.as_mut() else {
            return Err(Error::runtime(format!(
                "dream.fs.Writer.{what}: the writer over {} is closed",
                display(&self.name)
            )));
        };
        Ok(Outcome::of(body(file), &format!("Writer.{what}"), &self.name))
    }

    fn write(&self, data: &[u8]) -> Result<Outcome<f64>> {
        done!(self.with_file("write", |file| file.write_all(data))?);
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

    /// Flushes and closes; closing a closed writer does nothing and answers true.
    fn close(&self) -> Outcome<bool> {
        let Some(mut file) = self.file.borrow_mut().take() else {
            return Outcome::Done(true);
        };
        Outcome::of(file.flush().map(|()| true), "Writer.close", &self.name)
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
        .method("truncate", |w: &Writer, length: Exact<i64>| {
            w.truncate(non_negative("Writer.truncate", "length", length)?)
        })
        .signature("(self, length: number): (boolean?, string?, dream_fs_ErrorKind?)")
        .doc("Cuts or extends the file to length bytes; a position past it moves to it.");
    writer
        .method("close", Writer::close)
        .signature("(self): (boolean?, string?, dream_fs_ErrorKind?)")
        .doc("Flushes and closes; closing again does nothing.");
    writer.metamethod("__tostring", |w: &Writer| format!("dream.fs.Writer({})", display(&w.name)));
}
