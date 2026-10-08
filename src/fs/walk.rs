//! `fs.walk`: everything under a directory, depth first, as parallel arrays, so a walk of a
//! hundred thousand files makes a handful of tables instead of a hundred thousand.

use std::path::Path;

use crate::bind::{Call, StackResults};
use crate::convert::Exact;
use crate::error::{Error, Result};
use crate::options::Options;
use crate::stack::{Frame, Scope, TableView, ValueView};

use super::{Failure, Outcome, done, host_path, kind_name, path_bytes, since_epoch};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Include {
    All,
    Files,
    Dirs,
}

struct WalkOptions {
    follow_links: bool,
    include: Include,
    metadata: bool,
    sort: bool,
    max_depth: Option<usize>,
    skip_errors: bool,
}

impl WalkOptions {
    fn read(scope: &impl Scope, options: Option<ValueView<'_>>) -> Result<WalkOptions> {
        let mut walk = WalkOptions {
            follow_links: false,
            include: Include::All,
            metadata: false,
            sort: false,
            max_depth: None,
            skip_errors: false,
        };
        let Some(options) = options.filter(|view| !view.is_nil()) else {
            return Ok(walk);
        };
        Options::read(scope, options, "dream.fs.walk", |o| {
            walk.follow_links = o.or("followLinks", false)?;
            walk.metadata = o.or("metadata", false)?;
            walk.sort = o.or("sort", false)?;
            walk.skip_errors = o.or("skipErrors", false)?;
            if let Some(include) = o.optional_str("include", |text| Ok(text.to_owned()))? {
                walk.include = match include.as_str() {
                    "all" => Include::All,
                    "files" => Include::Files,
                    "dirs" => Include::Dirs,
                    other => {
                        return Err(Error::runtime(format!(
                            "dream.fs.walk: include must be 'all', 'files' or 'dirs', got '{other}'"
                        )));
                    }
                };
            }
            if let Some(depth) = o.optional::<Exact<i64>>("maxDepth")? {
                walk.max_depth = Some(usize::try_from(depth.0).ok().filter(|depth| *depth >= 1).ok_or_else(|| {
                    Error::runtime(format!("dream.fs.walk: maxDepth must be at least 1, got {}", depth.0))
                })?);
            }
            Ok(())
        })?;
        Ok(walk)
    }
}

/// What the walk found, column by column.
#[derive(Default)]
struct Columns {
    paths: Vec<Vec<u8>>,
    kinds: Vec<&'static str>,
    sizes: Vec<f64>,
    seconds: Vec<Option<f64>>,
    nanos: Vec<Option<f64>>,
    errors: Vec<(Vec<u8>, String)>,
}

/// A walk error as the failure a script gets: the OS error's kind when there is one.
fn failure(path: &[u8], error: &walkdir::Error) -> Failure {
    Failure {
        message: format!("dream.fs.walk: {}: {error}", super::display(path)).into(),
        kind: error.io_error().map_or("other", crate::outcome::kind_of),
    }
}

/// Everything under `root`, or the first error when `skipErrors` is off.
fn collect(root: &Path, root_bytes: &[u8], options: &WalkOptions) -> Outcome<Columns> {
    let mut walker = walkdir::WalkDir::new(root).min_depth(1).follow_links(options.follow_links);
    if let Some(depth) = options.max_depth {
        walker = walker.max_depth(depth);
    }
    if options.sort {
        walker = walker.sort_by_file_name();
    }
    let mut columns = Columns::default();
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                let path = error.path().map_or_else(|| root_bytes.to_vec(), |path| path_bytes(path).to_vec());
                if !options.skip_errors {
                    return Outcome::Failed(failure(&path, &error));
                }
                columns.errors.push((path, error.to_string()));
                continue;
            }
        };
        let file_type = entry.file_type();
        let wanted = match options.include {
            Include::All => true,
            Include::Files => file_type.is_file(),
            Include::Dirs => file_type.is_dir(),
        };
        if !wanted {
            continue;
        }
        if options.metadata {
            match entry.metadata() {
                Ok(metadata) => {
                    columns.sizes.push(if metadata.is_dir() { 0.0 } else { metadata.len() as f64 });
                    let exact = metadata.modified().ok().and_then(|time| since_epoch(time).1);
                    columns.seconds.push(exact.map(|(seconds, _)| seconds as f64));
                    columns.nanos.push(exact.map(|(_, nanos)| f64::from(nanos)));
                }
                Err(error) => {
                    let path = path_bytes(entry.path()).to_vec();
                    if !options.skip_errors {
                        return Outcome::Failed(failure(&path, &error));
                    }
                    columns.errors.push((path, error.to_string()));
                    continue;
                }
            }
        }
        let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
        columns.paths.push(relative_bytes(relative));
        columns.kinds.push(kind_name(file_type));
    }
    Outcome::Done(columns)
}

/// Pushes `values` as an array into `table[key]`.
fn column<T>(
    frame: &Frame<'_>,
    table: &TableView<'_>,
    key: &str,
    values: &[T],
    push: impl Fn(&Frame<'_>, &T) -> Result<()>,
) -> Result<()> {
    let array = frame.push_table(values.len(), 0)?;
    for (index, value) in values.iter().enumerate() {
        push(frame, value)?;
        array.raw_set_index(frame, index as i64 + 1)?;
    }
    table.raw_set(frame, key)
}

/// A path under the walk's root as a script sees it: components joined with `/` on every
/// platform, so `root .. '/' .. path` names the entry on Windows too.
#[cfg(windows)]
fn relative_bytes(relative: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    for (index, component) in relative.components().enumerate() {
        if index > 0 {
            bytes.push(b'/');
        }
        bytes.extend_from_slice(component.as_os_str().as_encoded_bytes());
    }
    bytes
}

/// A path under the walk's root as a script sees it; `/` is already the separator here.
#[cfg(not(windows))]
fn relative_bytes(relative: &Path) -> Vec<u8> {
    path_bytes(relative).to_vec()
}

/// `fs.walk(root, options?)`.
pub(crate) fn walk(call: &Call<'_>, root: &[u8], options: Option<ValueView<'_>>) -> Result<Outcome<StackResults>> {
    let options = WalkOptions::read(call, options)?;
    let host = host_path("walk", root)?;
    let columns = done!(collect(&host, root, &options));
    let mut frame = call.frame();
    let table = frame.push_table(0, 6)?;
    column(&frame, &table, "paths", &columns.paths, |frame, path| frame.push(path.as_slice()).map(drop))?;
    column(&frame, &table, "kinds", &columns.kinds, |frame, kind| frame.push(*kind).map(drop))?;
    if options.metadata {
        column(&frame, &table, "sizes", &columns.sizes, |frame, size| frame.push(size).map(drop))?;
        // A time before 1970, or one the platform cannot give, is 0 in both columns.
        column(&frame, &table, "modifiedSeconds", &columns.seconds, |frame, seconds| {
            frame.push(&seconds.unwrap_or(0.0)).map(drop)
        })?;
        column(&frame, &table, "modifiedNanoseconds", &columns.nanos, |frame, nanos| {
            frame.push(&nanos.unwrap_or(0.0)).map(drop)
        })?;
    }
    column(&frame, &table, "errors", &columns.errors, |frame, (path, message)| {
        let row = frame.push_table(0, 2)?;
        row.raw_set_value(frame, "path", path.as_slice())?;
        row.raw_set_value(frame, "message", message.as_str())
    })?;
    frame.release();
    Ok(Outcome::Done(StackResults))
}
