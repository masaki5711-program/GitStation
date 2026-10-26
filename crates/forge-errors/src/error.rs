//! `ForgeError`: a registered code + safe context + optional source.

use std::error::Error as StdError;
use std::fmt;

use crate::{ErrorCode, Lang};

type Source = Box<dyn StdError + Send + Sync + 'static>;

/// The one error type that crosses crate boundaries in forge.
///
/// - `code` decides the HTTP status, severity and user message.
/// - `context` holds safe key/value fields for the log event (ids, repo name,
///   operation) — never secrets.
/// - `source` keeps the underlying cause chain for the log only.
///
/// `Display` is the **internal** representation (code + context + source) for
/// logs; users get [`ForgeError::user_message`].
#[derive(Debug)]
pub struct ForgeError {
    code: ErrorCode,
    context: Vec<(&'static str, String)>,
    source: Option<Source>,
}

impl ForgeError {
    pub fn new(code: ErrorCode) -> Self {
        Self {
            code,
            context: Vec::new(),
            source: None,
        }
    }

    /// Add a safe context field (appears in logs, never in user output).
    #[must_use]
    pub fn with_context(mut self, key: &'static str, value: impl fmt::Display) -> Self {
        self.context.push((key, value.to_string()));
        self
    }

    /// Attach the underlying cause.
    #[must_use]
    pub fn with_source(mut self, source: impl StdError + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    pub fn code(&self) -> ErrorCode {
        self.code
    }

    pub fn context(&self) -> &[(&'static str, String)] {
        &self.context
    }

    /// The localized, internals-free message shown to users.
    pub fn user_message(&self, lang: Lang) -> &'static str {
        self.code.user_message(lang)
    }
}

impl fmt::Display for ForgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.code.id())?;
        for (k, v) in &self.context {
            write!(f, " {k}={v}")?;
        }
        if let Some(src) = &self.source {
            write!(f, ": {src}")?;
        }
        Ok(())
    }
}

impl StdError for ForgeError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source
            .as_deref()
            .map(|s| s as &(dyn StdError + 'static))
    }
}
