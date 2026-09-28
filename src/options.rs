//! Strict option tables (`L3I_EXTENSION_RUNTIME_ARCHITECTURE.md` §37): camelCase keys, every
//! key known, required keys reported precisely, field-path diagnostics, no permissive default.
//!
//! ```ignore
//! let plan = Options::read(call, view, "archive:extract", |o| {
//!     Ok(Extract { path: o.required("path")?, force: o.optional("force")?.unwrap_or(false) })
//! })?;
//! ```
//! `read` opens a frame, checks that the value is a table with string keys, hands the reader to
//! `body`, and then fails if any key was never asked for. A misspelt option is an error, not a
//! silently ignored default.

use std::collections::BTreeSet;

use crate::convert::FromView;
use crate::error::{Error, Result};
use crate::stack::{Frame, Scope, TableView, Type, ValueView};

/// A type built from an option table.
pub trait FromOptions: Sized {
    /// Reads the fields; `Options::finish` runs afterwards, so unknown keys still fail.
    fn from_options(options: &mut Options<'_, '_>) -> Result<Self>;

    /// Reads `view` as a `Self` under the diagnostic `context` (the API name).
    fn read(scope: &impl Scope, view: ValueView<'_>, context: &str) -> Result<Self> {
        Options::read(scope, view, context, Self::from_options)
    }
}

/// One option table being read.
pub struct Options<'a, 'f> {
    frame: &'a Frame<'f>,
    table: TableView<'a>,
    context: String,
    keys: BTreeSet<String>,
    seen: BTreeSet<String>,
}

impl<'f> Options<'_, 'f> {
    /// Reads the table at `view` with `body`, then rejects keys `body` never consumed.
    pub fn read<R>(
        scope: &impl Scope,
        view: ValueView<'_>,
        context: &str,
        body: impl FnOnce(&mut Options<'_, '_>) -> Result<R>,
    ) -> Result<R> {
        if matches!(view.type_of(), Type::None | Type::Nil) {
            return Err(Error::runtime(format!("{context}: missing options table")));
        }
        if !view.is_table() {
            return Err(Error::runtime(format!("{context}: options must be a table, got {}", view.type_of().name())));
        }
        let index = view.index();
        scope.with_frame(|frame| {
            let table = frame.at(index).as_table()?;
            let mut keys = BTreeSet::new();
            table.for_each(frame, |_, key, _| match key.read::<&str>() {
                Ok(text) => {
                    keys.insert(text.to_owned());
                    Ok(())
                }
                Err(_) => {
                    Err(Error::runtime(format!("{context}: option keys must be strings, got {}", key.type_of().name())))
                }
            })?;
            let mut options = Options { frame, table, context: context.to_owned(), keys, seen: BTreeSet::new() };
            let result = body(&mut options)?;
            options.finish()?;
            Ok(result)
        })
    }

    /// The diagnostic path of this table.
    pub fn context(&self) -> &str {
        &self.context
    }

    /// The frame the reader works on: open nested frames from here, never from the scope
    /// `read` was given (that scope already has this frame open).
    pub fn frame(&self) -> &Frame<'f> {
        self.frame
    }

    /// Whether the table has `key` (consumes nothing).
    pub fn has(&self, key: &str) -> bool {
        self.keys.contains(key)
    }

    fn take(&mut self, key: &str) {
        self.seen.insert(key.to_owned());
    }

    /// A required field, converted with `T`'s `FromView`.
    pub fn required<T: for<'v> FromView<'v>>(&mut self, key: &str) -> Result<T> {
        self.take(key);
        if !self.keys.contains(key) {
            return Err(Error::runtime(format!("{}: missing required option '{key}'", self.context)));
        }
        self.convert::<T>(key)
    }

    /// An optional field: absent or nil reads as `None`.
    pub fn optional<T: for<'v> FromView<'v>>(&mut self, key: &str) -> Result<Option<T>> {
        self.take(key);
        if !self.keys.contains(key) {
            return Ok(None);
        }
        let step = self.frame.frame();
        let value = self.table.raw_get(&step, key)?;
        if value.type_of() == Type::Nil {
            return Ok(None);
        }
        T::from_view(value).map(Some).map_err(|cause| self.field_error(key, &cause))
    }

    /// An optional field with a default.
    pub fn or<T: for<'v> FromView<'v>>(&mut self, key: &str, default: T) -> Result<T> {
        Ok(self.optional(key)?.unwrap_or(default))
    }

    /// A nested option table under `key`, read with `body`; absent reads as `None`.
    pub fn nested<R>(&mut self, key: &str, body: impl FnOnce(&mut Options<'_, '_>) -> Result<R>) -> Result<Option<R>> {
        self.take(key);
        if !self.keys.contains(key) {
            return Ok(None);
        }
        let context = format!("{}.{key}", self.context);
        let step = self.frame.frame();
        let value = self.table.raw_get(&step, key)?;
        if value.type_of() == Type::Nil {
            return Ok(None);
        }
        Options::read(&step, value, &context, body).map(Some)
    }

    fn convert<T: for<'v> FromView<'v>>(&self, key: &str) -> Result<T> {
        let step = self.frame.frame();
        let value = self.table.raw_get(&step, key)?;
        T::from_view(value).map_err(|cause| self.field_error(key, &cause))
    }

    fn field_error(&self, key: &str, cause: &Error) -> Error {
        Error::runtime(format!("{}.{key}: {cause}", self.context))
    }

    /// Fails when the table holds keys nothing asked for, naming them and the known ones.
    fn finish(self) -> Result<()> {
        let unknown: Vec<&str> = self.keys.difference(&self.seen).map(String::as_str).collect();
        if unknown.is_empty() {
            return Ok(());
        }
        let known: Vec<&str> = self.seen.iter().map(String::as_str).collect();
        Err(Error::runtime(format!(
            "{}: unknown option{} {}; known options are {}",
            self.context,
            if unknown.len() == 1 { "" } else { "s" },
            unknown.iter().map(|k| format!("'{k}'")).collect::<Vec<_>>().join(", "),
            if known.is_empty() { "none".to_owned() } else { known.join(", ") }
        )))
    }
}
