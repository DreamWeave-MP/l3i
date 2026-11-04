//! The Luau face of authenticated reads: `fs.authManifest`, `fs.openAuthenticated` and
//! `dream.fs.AuthenticatedReader`.

use std::borrow::Cow;
use std::cell::RefCell;
use std::fs::File;

use crate::bind::{Call, StackResults};
use crate::convert::{BufferView, BytesView, Exact, NewBuffer, new_buffer};
use crate::error::{Error, Result};
use crate::extension::{ExtensionDescriptor, TagPolicy};
use crate::options::Options;
use crate::outcome::{Failure, Outcome, done, kind_of};
use crate::stack::ValueView;
use crate::userdata::Userdata;

use super::{
    AuthError, AuthenticatedReader, Commitment, DEFAULT_CHUNK_SIZE, VerifiedManifest, build_manifest, valid_chunk_size,
};
use crate::fs::display;
use crate::fs::io::{non_negative, open_regular, window};

/// The Luau type of what an authenticated operation can fail with.
pub(crate) const AUTH_ERROR_KIND_TYPE: &str = "dream_fs_ErrorKind | \"integrityFailed\" | \"invalidManifest\"";

/// `error` as a script sees it. The bytes not matching what the caller trusts is its own kind,
/// never `other`, so a script can tell tampering from a disk that said no.
fn failure(what: &str, path: &[u8], error: &AuthError) -> Failure {
    let kind = match error {
        AuthError::Io(error) => kind_of(error),
        AuthError::ManifestMismatch
        | AuthError::LengthMismatch { .. }
        | AuthError::ShortChunk { .. }
        | AuthError::ChunkMismatch { .. } => "integrityFailed",
        AuthError::ManifestTooLong { .. } | AuthError::MalformedManifest(_) => "invalidManifest",
        AuthError::UnsupportedVersion(_) | AuthError::UnsupportedAlgorithm(_) => "unsupported",
        _ => "other",
    };
    Failure { message: Cow::Owned(format!("dream.fs.{what}: {}: {error}", display(path))), kind }
}

/// A refusal that is the script's mistake rather than the disk's answer.
fn mistake(what: &str, error: &AuthError) -> Error {
    Error::runtime(format!("dream.fs.{what}: {error}"))
}

/// `fs.authManifest(path, { chunkSize? })`: the manifest and the commitment to deliver separately.
pub(crate) fn auth_manifest(
    call: &Call<'_>,
    path: &[u8],
    options: Option<ValueView<'_>>,
) -> Result<Outcome<(NewBuffer, String)>> {
    const WHAT: &str = "authManifest";
    let chunk_size = match options.filter(|view| !view.is_nil()) {
        Some(options) => {
            Options::read(call, options, "dream.fs.authManifest", |o| o.optional::<Exact<i64>>("chunkSize"))?
        }
        None => None,
    };
    let chunk_size = chunk_size.map_or(i64::from(DEFAULT_CHUNK_SIZE), |size| size.0);
    // A bad size is the script's mistake whatever is at the path, so it is refused before the disk.
    let chunk_size = u32::try_from(chunk_size)
        .ok()
        .filter(|size| valid_chunk_size(u64::from(*size)))
        .ok_or_else(|| mistake(WHAT, &AuthError::InvalidChunkSize(chunk_size.unsigned_abs())))?;
    let host = crate::fs::host_path(WHAT, path)?;
    let file = done!(Outcome::of(open_regular(&host), WHAT, path));
    Ok(match build_manifest(&file, chunk_size) {
        Ok(published) => Outcome::Done((NewBuffer(published.bytes), published.commitment.to_string())),
        Err(error) => Outcome::Failed(failure(WHAT, path, &error)),
    })
}

/// `dream.fs.AuthenticatedReader`: a retained, unmapped handle and the manifest that checks it.
#[derive(Debug)]
pub(crate) struct AuthenticatedHandle {
    reader: RefCell<Option<AuthenticatedReader<File>>>,
    manifest: std::sync::Arc<VerifiedManifest>,
    name: Box<[u8]>,
}

// SAFETY: a file handle and plain data; dropping it closes the handle with no Lua API.
unsafe impl Userdata for AuthenticatedHandle {
    const NAME: &'static str = "dream.fs.AuthenticatedReader";
}

/// `fs.openAuthenticated(path, manifest, commitment)`. The manifest is authenticated before the
/// path is touched, and copied only once it has matched.
pub(crate) fn open_authenticated(
    path: &[u8],
    manifest: BytesView<'_>,
    commitment: &[u8],
) -> Result<Outcome<AuthenticatedHandle>> {
    const WHAT: &str = "openAuthenticated";
    let trusted = Commitment::from_hex(commitment).map_err(|error| mistake(WHAT, &error))?;
    // SAFETY: the bytes are hashed and copied with no call into Lua while the slice lives, through
    // this call's only view of them.
    let verified = VerifiedManifest::verify(unsafe { manifest.bytes_unchecked() }, &trusted);
    let verified = match verified {
        Ok(verified) => std::sync::Arc::new(verified),
        Err(error) => return Ok(Outcome::Failed(failure(WHAT, path, &error))),
    };
    let host = crate::fs::host_path(WHAT, path)?;
    let file = done!(Outcome::of(open_regular(&host), WHAT, path));
    match AuthenticatedReader::open(file, verified.clone()) {
        Ok(reader) => Ok(Outcome::Done(AuthenticatedHandle {
            reader: RefCell::new(Some(reader)),
            manifest: verified,
            name: path.into(),
        })),
        Err(error) => Ok(Outcome::Failed(failure(WHAT, path, &error))),
    }
}

impl AuthenticatedHandle {
    fn closed(&self, what: &str) -> Error {
        Error::runtime(format!(
            "dream.fs.AuthenticatedReader.{what}: the reader over {} is closed",
            display(&self.name)
        ))
    }
}

/// A range as the reader checks it: a mistake raises before any buffer exists for it.
fn checked_range(handle: &AuthenticatedHandle, what: &str, position: u64, length: u64) -> Result<()> {
    handle
        .manifest
        .check_range(position, length)
        .map_err(|error| mistake(&format!("AuthenticatedReader.{what}"), &error))
}

fn read_at(
    handle: &AuthenticatedHandle,
    call: &Call<'_>,
    position: Exact<i64>,
    length: Exact<i64>,
) -> Result<Outcome<StackResults>> {
    const WHAT: &str = "AuthenticatedReader.readAt";
    let position = non_negative(WHAT, "position", position)?;
    let length = non_negative(WHAT, "length", length)?;
    let mut guard = handle.reader.borrow_mut();
    let reader = guard.as_mut().ok_or_else(|| handle.closed("readAt"))?;
    checked_range(handle, "readAt", position, length)?;
    // In range, so at most the object's length, which fit a file on this machine.
    let length =
        usize::try_from(length).map_err(|_| Error::runtime(format!("dream.fs.{WHAT}: length {length} is too long")))?;
    let mut buffer = new_buffer(call, length)?;
    // SAFETY: the buffer was created by this call and no other view of it exists; the read never
    // calls into Lua. A failure's results go above the buffer, which the call drops.
    match reader.read_into(position, unsafe { buffer.bytes_mut_unchecked() }) {
        Ok(()) => Ok(Outcome::Done(StackResults)),
        Err(error) => Ok(Outcome::Failed(failure(WHAT, &handle.name, &error))),
    }
}

fn read_at_into(
    handle: &AuthenticatedHandle,
    mut target: BufferView<'_>,
    position: Exact<i64>,
    length: Exact<i64>,
    buffer_offset: Option<Exact<i64>>,
) -> Result<Outcome<f64>> {
    const WHAT: &str = "AuthenticatedReader.readAtInto";
    let (buffer_offset, count) = window(WHAT, "bufferOffset", "buffer", target.len(), buffer_offset, Some(length))?;
    let position = non_negative(WHAT, "position", position)?;
    let mut guard = handle.reader.borrow_mut();
    let reader = guard.as_mut().ok_or_else(|| handle.closed("readAtInto"))?;
    checked_range(handle, "readAtInto", position, count as u64)?;
    // SAFETY: the window is inside the buffer, this call holds the only view, and the read never
    // calls into Lua; on failure the window is zeroed before the script can see it.
    let window = unsafe { &mut target.bytes_mut_unchecked()[buffer_offset..buffer_offset + count] };
    match reader.read_into(position, window) {
        Ok(()) => Ok(Outcome::Done(count as f64)),
        Err(error) => Ok(Outcome::Failed(failure(WHAT, &handle.name, &error))),
    }
}

pub(crate) fn describe_authenticated_reader(d: &mut ExtensionDescriptor) {
    d.type_alias("dream_fs_AuthErrorKind", AUTH_ERROR_KIND_TYPE);
    let mut reader = d.userdata::<AuthenticatedHandle>(AuthenticatedHandle::NAME);
    reader.tag(TagPolicy::Never).doc(
        "Random access to an object through a manifest that matched a trusted commitment. Every byte returned has been verified; a read that cannot be verified returns nil, the message and the kind.",
    );
    reader
        .method("readAt", read_at)
        .signature("(self, position: number, length: number): (buffer?, string?, dream_fs_AuthErrorKind?)")
        .doc("Exactly length bytes from position as a new buffer. A range outside the object raises.");
    reader
        .method("readAtInto", read_at_into)
        .signature("(self, target: buffer, position: number, length: number, bufferOffset: number?): (number?, string?, dream_fs_AuthErrorKind?)")
        .doc("Copies exactly length verified bytes from position into target at bufferOffset; returns the count. On failure that window of target is zeroed.");
    reader
        .method("size", |h: &AuthenticatedHandle| h.manifest.object_length() as f64)
        .signature("(self): number")
        .doc("The committed length, not the file's current size.");
    reader
        .method("chunkSize", |h: &AuthenticatedHandle| f64::from(h.manifest.chunk_size()))
        .signature("(self): number");
    reader
        .method("commitment", |h: &AuthenticatedHandle| h.manifest.commitment().to_string())
        .signature("(self): string")
        .doc("The commitment this reader checks against, as hex.");
    reader
        .method("close", |h: &AuthenticatedHandle| {
            h.reader.borrow_mut().take();
        })
        .signature("(self)")
        .doc("Closes the handle; later reads raise. Closing twice does nothing.");
    reader.metamethod("__tostring", |h: &AuthenticatedHandle| {
        format!("dream.fs.AuthenticatedReader({})", display(&h.name))
    });
}
