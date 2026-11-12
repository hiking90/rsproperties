// Copyright 2024 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The crate's error type.

/// `Result` with this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// Crate-wide error type.
///
/// `#[non_exhaustive]` because this enum is re-exported from a published
/// library: downstream `match`es must keep a wildcard arm so future
/// variants are not semver-breaking.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// An OS error. Every failed syscall is reported through this variant,
    /// so one `match` arm sees every errno; read it with
    /// `std::io::Error::raw_os_error` or `kind`.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// No property with this name (the payload) exists.
    #[error("Property not found: {0}")]
    NotFound(String),

    /// UTF-8 decode failure. A dedicated `#[from]` variant (rather than a
    /// message string) so `source()` still reaches the original
    /// [`std::str::Utf8Error`].
    ///
    /// Deliberately NOT mirrored by a `String::from_utf8` variant:
    /// [`std::string::FromUtf8Error`] owns the failed byte buffer, and
    /// carrying wire *values* inside errors would violate the service's
    /// don't-log-values policy the moment someone logs `{e:?}`. Call sites
    /// convert with `.map_err(|e| Error::Utf8(e.utf8_error()))`, which
    /// keeps the diagnostic position info and drops the bytes.
    #[error("UTF-8 conversion error: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    /// Text that could not be parsed: malformed property names, bad
    /// property-info lines, and — via `get<T>` — a value that is not a
    /// `T` (`FromProperty::from_property` returned `None`). This is the
    /// **single** parse-failure variant of the public API. For `get<T>`
    /// the message names the property, the value's length and `T`, never
    /// the value, and there is no `source()`.
    #[error("Parse error: {0}")]
    Parse(String),

    /// A file of the property store (an area file, `property_info`) or a
    /// prop file has content, a type or a mode this crate refuses to use.
    #[error("File validation error: {0}")]
    FileValidation(String),

    /// Caller-supplied argument violated an API contract (over-long
    /// name/value, malformed input) — distinct from [`Error::FileValidation`],
    /// which reports corrupt on-disk state.
    #[error("Invalid argument: {0}")]
    InvalidArgument(String),

    /// A first-write-wins global (properties/socket directory) already
    /// holds a different directory — set by an earlier `init()`, or
    /// implicitly by the first property access latching the default. Also
    /// used for
    /// initialization-order conflicts of the same shape, e.g. a property
    /// area already mapped read-only when a writable mapping is requested.
    #[error("Already initialized: {0}")]
    AlreadyInitialized(String),

    /// A global resource budget was exhausted (e.g. the build.prop import
    /// loader's total-file-loads budget). Deliberately NOT used for the
    /// per-import recursion-depth cap, which is logged and skipped rather
    /// than aborting. A dedicated variant — not [`Error::Parse`] — so
    /// callers can distinguish "abort the whole load" limits from ordinary
    /// per-item failures with `matches!` instead of re-inspecting counters
    /// or message text.
    #[error("Limit exceeded: {0}")]
    LimitExceeded(String),

    /// The property service accepted the connection but rejected the
    /// request at the protocol level — the socket itself is healthy, so
    /// this is deliberately not an [`Error::Io`].
    #[error("Property service rejected \"{name}\": error code {code:#x}")]
    ServiceError {
        /// The property name of the rejected request.
        name: String,
        /// The service's reply code: one of `wire::PROP_ERROR_*`.
        code: i32,
    },

    /// The property service refused the request for this sender.
    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    /// A file of the property store is smaller or larger than its format
    /// allows.
    #[error("File size error: {0}")]
    FileSize(String),

    /// The fixed-size property area has no room for another allocation —
    /// an operational limit (bionic returns `false` here), distinct from
    /// the corrupt-file conditions reported as [`Error::FileValidation`] /
    /// [`Error::FileSize`].
    #[error("Property area full: {0}")]
    AreaFull(String),

    /// A file of the property store is not owned by root
    /// (`strict-file-validation` feature).
    #[error("File ownership error: {0}")]
    FileOwnership(String),

    /// Another instance holds the writer lock of the properties directory
    /// (`AreaWriterLock::acquire`).
    #[error("Lock error: {0}")]
    Lock(String),

    /// The global reader could not be created (see `system_properties`).
    /// Not cached: the next access tries again. The original error is the
    /// `source()`.
    ///
    /// Like `Context` below, the source appears in `Display` *and* via
    /// `#[source]` (duplicated text under chain-walking reporters) so that
    /// bare `{e}` log sites still show the root cause.
    #[error("SystemProperties initialization failed: {0}")]
    Init(#[source] Box<Error>),

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
        /// What the failed step was doing.
        msg: String,
        /// The source location that added the context.
        location: &'static std::panic::Location<'static>,
        /// The error the step failed with.
        #[source]
        source: Box<Error>,
    },
}

impl Error {
    /// The error under every [`Error::Context`] and [`Error::Init`] layer;
    /// `self` when it is neither.
    ///
    /// Which calls add a layer is not part of the API contract, so match
    /// on this instead of on the returned value when the cause decides
    /// what to do:
    ///
    /// ```
    /// use rsproperties::Error;
    ///
    /// fn store_is_missing(err: &Error) -> bool {
    ///     matches!(err.root_cause(),
    ///         Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound)
    /// }
    /// ```
    pub fn root_cause(&self) -> &Error {
        let mut err = self;
        while let Error::Context { source, .. } | Error::Init(source) = err {
            err = source;
        }
        err
    }
}

/// What [`ContextWithLocation`] accepts as the wrapped error. `rustix`
/// calls report `Errno`; it becomes [`Error::Io`] here instead of through a
/// public `From` impl, which would put a `rustix` type (and its major
/// version) into this crate's public API.
pub(crate) trait IntoError {
    fn into_error(self) -> Error;
}

impl IntoError for Error {
    fn into_error(self) -> Error {
        self
    }
}

impl IntoError for std::io::Error {
    fn into_error(self) -> Error {
        Error::Io(self)
    }
}

impl IntoError for rustix::io::Errno {
    fn into_error(self) -> Error {
        Error::Io(self.into())
    }
}

/// Wraps the error of a `Result` in [`Error::Context`].
pub(crate) trait ContextWithLocation<T> {
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
    E: IntoError,
{
    #[track_caller]
    fn context_with_location(self, msg: impl Into<String>) -> Result<T> {
        let location = std::panic::Location::caller();
        self.map_err(|e| Error::Context {
            msg: msg.into(),
            location,
            source: Box::new(e.into_error()),
        })
    }

    #[track_caller]
    fn with_context_location(self, f: impl FnOnce() -> String) -> Result<T> {
        let location = std::panic::Location::caller();
        self.map_err(|e| Error::Context {
            msg: f(),
            location,
            source: Box::new(e.into_error()),
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

    #[test]
    fn errno_converts_into_io() {
        let err = rustix::io::Errno::ACCESS.into_error();
        match err {
            Error::Io(io) => assert_eq!(
                rustix::io::Errno::from_io_error(&io),
                Some(rustix::io::Errno::ACCESS)
            ),
            other => panic!("expected Error::Io, got {other:?}"),
        }
    }

    #[test]
    fn root_cause_strips_context_and_init() {
        let plain = Error::NotFound("a.b".into());
        assert!(matches!(plain.root_cause(), Error::NotFound(_)));

        let wrapped: Error = Err::<(), _>(rustix::io::Errno::NOENT)
            .context_with_location("inner")
            .context_with_location("outer")
            .unwrap_err();
        let wrapped = Error::Init(Box::new(wrapped));
        assert!(matches!(
            wrapped.root_cause(),
            Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound
        ));
    }

    fn try_open_file() -> Result<()> {
        std::fs::File::open("non-existent-file")?;
        Ok(())
    }
}
