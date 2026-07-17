// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::num::ParseIntError;

pub type Result<T> = std::result::Result<T, Error>;

/// Crate-wide error type.
///
/// `#[non_exhaustive]` because this enum is re-exported from a published
/// library: downstream `match`es must keep a wildcard arm so future
/// variants are not semver-breaking.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("System error: {0}")]
    Errno(#[from] rustix::io::Errno),

    #[error("Property not found: {0}")]
    NotFound(String),

    #[error("Encoding error: {0}")]
    Encoding(String),

    /// UTF-8 decode failure. A dedicated `#[from]` variant (rather than
    /// stringifying into [`Error::Encoding`]) so `source()` still reaches
    /// the original [`std::str::Utf8Error`].
    ///
    /// Deliberately NOT mirrored by a `String::from_utf8` variant:
    /// [`std::string::FromUtf8Error`] owns the failed byte buffer, and
    /// carrying wire *values* inside errors would violate the service's
    /// don't-log-values policy the moment someone logs `{e:?}`. Call sites
    /// convert with `.map_err(|e| Error::Utf8(e.utf8_error()))`, which
    /// keeps the diagnostic position info and drops the bytes.
    #[error("UTF-8 conversion error: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    #[error("Parse error: {0}")]
    Parse(String),

    /// Integer parse failure. `#[from]` for the same `source()`-chain
    /// reason as [`Error::Utf8`].
    #[error("Parse integer error: {0}")]
    ParseInt(#[from] ParseIntError),

    #[error("File validation error: {0}")]
    FileValidation(String),

    /// Never constructed by this crate; retained only because removing a
    /// public variant is semver-breaking.
    #[deprecated(
        note = "never produced by rsproperties; match it with a wildcard arm — \
                it will be removed in the next major release"
    )]
    #[error("Conversion error: {0}")]
    Conversion(String),

    /// Caller-supplied argument violated an API contract (over-long
    /// name/value, malformed input) — distinct from [`Error::FileValidation`],
    /// which reports corrupt on-disk state.
    #[error("Invalid argument: {0}")]
    InvalidArgument(String),

    /// A first-write-wins global (properties/socket directory) was already
    /// initialized — explicitly via `init()`/`try_init()` or implicitly by
    /// the first property access latching the default.
    #[error("Already initialized: {0}")]
    AlreadyInitialized(String),

    /// The property service accepted the connection but rejected the
    /// request at the protocol level — the socket itself is healthy, so
    /// this is deliberately not an [`Error::Io`].
    #[error("Property service rejected \"{name}\": error code {code:#x}")]
    ServiceError { name: String, code: i32 },

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    #[error("File size error: {0}")]
    FileSize(String),

    /// The fixed-size property area has no room for another allocation —
    /// an operational limit (bionic returns `false` here), distinct from
    /// the corrupt-file conditions reported as [`Error::FileValidation`] /
    /// [`Error::FileSize`].
    #[error("Property area full: {0}")]
    AreaFull(String),

    #[error("File ownership error: {0}")]
    FileOwnership(String),

    #[error("Lock error: {0}")]
    Lock(String),

    /// Cached global-initialization failure (see `try_system_properties`).
    /// Wraps the original in `Arc` because the `OnceLock` cache can only
    /// hand out references while callers need an owned value — the
    /// original variant stays reachable via `source()`/`Arc` and its own
    /// chain is preserved.
    ///
    /// Like `Context` below, the source appears in `Display` *and* via
    /// `#[source]` (duplicated text under chain-walking reporters) so that
    /// bare `{e}` log sites still show the root cause.
    #[error("SystemProperties initialization failed: {0}")]
    Init(#[source] std::sync::Arc<Error>),

    /// A wrapped error with caller-supplied context and the source error
    /// preserved for `Error::source()` chain traversal.
    ///
    /// Note: the source is deliberately included in `Display` *and*
    /// exposed via `#[source]`, deviating from the thiserror convention of
    /// one-or-the-other. This crate's log sites print bare `{e}`, so
    /// dropping the source from `Display` would silence root causes at
    /// every existing call site; the cost is a duplicated message when a
    /// reporter walks the chain (anyhow-style "caused by" output).
    #[error("{msg} (at {location}): {source}")]
    Context {
        msg: String,
        location: &'static std::panic::Location<'static>,
        #[source]
        source: Box<Error>,
    },
}

pub trait ContextWithLocation<T> {
    #[track_caller]
    fn context_with_location(self, msg: impl Into<String>) -> Result<T>;

    /// Lazy variant of [`Self::context_with_location`]: the message closure
    /// runs only on the error path. Use this in loops / hot paths where an
    /// eagerly-evaluated `format!` argument would allocate on every
    /// success.
    #[track_caller]
    fn with_context_location(self, f: impl FnOnce() -> String) -> Result<T>;
}

impl<T, E> ContextWithLocation<T> for std::result::Result<T, E>
where
    E: Into<Error>,
{
    #[track_caller]
    fn context_with_location(self, msg: impl Into<String>) -> Result<T> {
        let location = std::panic::Location::caller();
        self.map_err(|e| Error::Context {
            msg: msg.into(),
            location,
            source: Box::new(e.into()),
        })
    }

    #[track_caller]
    fn with_context_location(self, f: impl FnOnce() -> String) -> Result<T> {
        let location = std::panic::Location::caller();
        self.map_err(|e| Error::Context {
            msg: f(),
            location,
            source: Box::new(e.into()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_io_via_question_mark() {
        let err = try_open_file().unwrap_err();
        assert!(matches!(err, Error::Io(_)));
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn test_error_context_with_location() {
        let err: Error = std::fs::File::open("non-existent-file")
            .context_with_location("opening test file")
            .unwrap_err();
        assert!(matches!(err, Error::Context { .. }));
        let msg = format!("{err}");
        assert!(msg.contains("opening test file"));
        assert!(msg.contains("non-existent-file") || msg.contains("No such"));
        assert!(std::error::Error::source(&err).is_some());
    }

    fn try_open_file() -> Result<()> {
        std::fs::File::open("non-existent-file")?;
        Ok(())
    }
}
