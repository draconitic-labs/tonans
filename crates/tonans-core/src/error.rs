//! The error taxonomy.
//!
//! Every rejection carries a stable machine code alongside its human message.
//! The codes are part of the API contract: the UI maps them to remediation hints
//! and the import-abuse tests assert on them, so a refactor that silently
//! reclassifies a rejection fails the suite.

use std::fmt;

/// The result type used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// A rejected input, a violated limit, or a failed I/O operation.
///
/// Tonans never repairs malformed data, so most variants here are terminal: they
/// describe precisely what was wrong and where, and the caller is expected to
/// show that to a human rather than guess at a fix.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A CSV row, header, or cell was malformed.
    #[error("{message}")]
    Import {
        /// Stable machine code, e.g. `import.duplicate_timestamp`.
        code: &'static str,
        /// 1-based line number in the source file, header included, when known.
        line: Option<u64>,
        /// Source column name, when the failure is attributable to one.
        column: Option<String>,
        /// Human-readable explanation, including the offending value.
        message: String,
    },

    /// An enforced size, count, or duration limit was exceeded.
    #[error("{message}")]
    LimitExceeded {
        /// Stable machine code, e.g. `limit.csv_rows`.
        code: &'static str,
        /// The bound that was breached.
        limit: u64,
        /// What was actually supplied.
        actual: u64,
        /// Human-readable explanation.
        message: String,
    },

    /// A project, scenario, or assumption was internally inconsistent.
    #[error("{message}")]
    Validation {
        /// Stable machine code, e.g. `validation.reserve_below_floor`.
        code: &'static str,
        /// Dotted path to the offending field, e.g. `scenario.battery.reserve_soc_pct`.
        field: String,
        /// Human-readable explanation.
        message: String,
    },

    /// The project on disk is not the one this request was made against.
    #[error("{message}")]
    Conflict {
        /// Stable machine code, e.g. `conflict.project_changed`.
        code: &'static str,
        /// Human-readable explanation.
        message: String,
    },

    /// A name or path was unsafe, reserved, or outside the workspace.
    #[error("{message}")]
    UnsafePath {
        /// Stable machine code, e.g. `path.traversal`.
        code: &'static str,
        /// Human-readable explanation. Never echoes the raw rejected bytes.
        message: String,
    },

    /// A project file could not be understood.
    #[error("{message}")]
    Schema {
        /// Stable machine code, e.g. `schema.version_unsupported`.
        code: &'static str,
        /// Human-readable explanation.
        message: String,
    },

    /// The engine produced or was about to produce a non-finite quantity.
    ///
    /// This is a defect, not a user error. Validation is supposed to make it
    /// unreachable; the check exists so that if it ever is reached, the result is
    /// a loud failure rather than a `null` in an exported report.
    #[error("{message}")]
    NonFinite {
        /// Where the value appeared.
        field: String,
        /// Human-readable explanation.
        message: String,
    },

    /// Filesystem failure.
    #[error("{context}: {source}")]
    Io {
        /// What we were trying to do.
        context: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },

    /// Serialization or deserialization failure that is not a schema violation.
    #[error("{context}: {source}")]
    Json {
        /// What we were trying to do.
        context: String,
        /// The underlying error.
        #[source]
        source: serde_json::Error,
    },
}

impl Error {
    /// The stable machine code for this error.
    #[must_use]
    pub fn code(&self) -> &str {
        match self {
            Error::Import { code, .. }
            | Error::LimitExceeded { code, .. }
            | Error::Validation { code, .. }
            | Error::Conflict { code, .. }
            | Error::UnsafePath { code, .. }
            | Error::Schema { code, .. } => code,
            Error::NonFinite { .. } => "internal.non_finite",
            Error::Io { .. } => "io.failed",
            Error::Json { .. } => "json.failed",
        }
    }

    /// The HTTP status this error should map to.
    ///
    /// Lives here rather than in the server so that the classification is
    /// testable without standing up a router, and so that a new variant cannot be
    /// added without deciding how a client sees it.
    #[must_use]
    pub fn http_status(&self) -> u16 {
        match self {
            Error::Import { .. } | Error::Validation { .. } | Error::Schema { .. } => 422,
            Error::LimitExceeded { .. } => 413,
            Error::Conflict { .. } => 409,
            Error::UnsafePath { .. } => 400,
            Error::NonFinite { .. } | Error::Io { .. } | Error::Json { .. } => 500,
        }
    }

    /// Build an import error attributed to a source line and column.
    pub fn import(
        code: &'static str,
        line: Option<u64>,
        column: Option<&str>,
        message: impl Into<String>,
    ) -> Self {
        Error::Import {
            code,
            line,
            column: column.map(str::to_owned),
            message: message.into(),
        }
    }

    /// Build a limit error.
    pub fn limit(code: &'static str, limit: u64, actual: u64, message: impl Into<String>) -> Self {
        Error::LimitExceeded {
            code,
            limit,
            actual,
            message: message.into(),
        }
    }

    /// Build a validation error attributed to a schema field.
    pub fn validation(
        code: &'static str,
        field: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Error::Validation {
            code,
            field: field.into(),
            message: message.into(),
        }
    }

    /// Build a non-finite defect error.
    pub fn non_finite(field: impl Into<String>, value: f64) -> Self {
        let field = field.into();
        Error::NonFinite {
            message: format!(
                "internal error: {field} evaluated to {value}, which is not a finite number. \
                 This is a defect in Tonans. Report it to {support} with the project file.",
                support = crate::version::SUPPORT_EMAIL
            ),
            field,
        }
    }

    /// Wrap an I/O failure with the operation that produced it.
    pub fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Error::Io {
            context: context.into(),
            source,
        }
    }

    /// Wrap a JSON failure with the operation that produced it.
    pub fn json(context: impl Into<String>, source: serde_json::Error) -> Self {
        Error::Json {
            context: context.into(),
            source,
        }
    }
}

/// Guard a computed quantity against `NaN` and infinity.
///
/// Used at the few points where a division could in principle go wrong even
/// after validation. Cheap enough to leave in release builds, and it converts a
/// silent `null` in an exported report into a loud, attributable failure.
pub fn finite(field: &str, value: f64) -> Result<f64> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(Error::non_finite(field, value))
    }
}

/// A non-fatal observation about an input, a model limit, or a result that a
/// reader must see before trusting the number next to it.
///
/// Warnings are first-class output: they are surfaced in the UI ledger, printed
/// in the report, and recorded in the provenance manifest.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Warning {
    /// Stable machine code, e.g. `data.synthesized_intervals`.
    pub code: String,
    /// How much it matters.
    pub severity: Severity,
    /// Human-readable explanation.
    pub message: String,
}

impl Warning {
    /// Build a warning.
    pub fn new(code: impl Into<String>, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            severity,
            message: message.into(),
        }
    }

    /// An observation that changes nothing but is worth stating.
    pub fn info(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, Severity::Info, message)
    }

    /// A result the reader should treat with care.
    pub fn caution(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, Severity::Caution, message)
    }

    /// A result that is probably not usable as it stands.
    pub fn critical(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(code, Severity::Critical, message)
    }
}

/// How much a [`Warning`] matters.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Worth stating; does not undermine the result.
    Info,
    /// The result is usable but a stated assumption is doing real work.
    Caution,
    /// The result is unreliable as it stands.
    Critical,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Severity::Info => "info",
            Severity::Caution => "caution",
            Severity::Critical => "critical",
        })
    }
}

/// Collect warnings, deduplicating by code and honouring [`crate::limits::MAX_WARNINGS`].
///
/// Deduplication matters: a per-interval condition would otherwise emit 35,040
/// identical lines and bury everything else in the ledger. The first occurrence
/// wins, so the message can carry the first offending interval, and callers that
/// want a count fold it into that message themselves.
#[derive(Debug, Clone, Default)]
pub struct WarningLedger {
    warnings: Vec<Warning>,
}

impl WarningLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a warning unless its code is already present or the ledger is full.
    pub fn push(&mut self, warning: Warning) {
        if self.warnings.len() >= crate::limits::MAX_WARNINGS {
            return;
        }
        if self.warnings.iter().any(|w| w.code == warning.code) {
            return;
        }
        self.warnings.push(warning);
    }

    /// Record every warning in `warnings`, subject to the same rules as [`Self::push`].
    pub fn extend(&mut self, warnings: impl IntoIterator<Item = Warning>) {
        for warning in warnings {
            self.push(warning);
        }
    }

    /// Whether any warning of `severity` or worse was recorded.
    #[must_use]
    pub fn has_at_least(&self, severity: Severity) -> bool {
        self.warnings.iter().any(|w| w.severity >= severity)
    }

    /// The warnings, ordered most severe first and then by code.
    ///
    /// The ordering is total and content-derived, so two runs of the same input
    /// produce byte-identical ledgers.
    #[must_use]
    pub fn into_sorted(self) -> Vec<Warning> {
        let mut warnings = self.warnings;
        warnings.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then_with(|| a.code.cmp(&b.code))
                .then_with(|| a.message.cmp(&b.message))
        });
        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_has_a_code_and_a_status() {
        let cases = [
            Error::import("import.bad", Some(3), Some("load_kw"), "bad"),
            Error::limit("limit.rows", 10, 11, "too many"),
            Error::validation("validation.bad", "a.b", "bad"),
            Error::UnsafePath {
                code: "path.traversal",
                message: "no".into(),
            },
            Error::Schema {
                code: "schema.version_unsupported",
                message: "no".into(),
            },
            Error::non_finite("x", f64::NAN),
        ];
        for error in cases {
            assert!(!error.code().is_empty(), "{error:?}");
            assert!((400..=500).contains(&error.http_status()), "{error:?}");
            assert!(!error.to_string().is_empty(), "{error:?}");
        }
    }

    #[test]
    fn non_finite_never_reports_a_user_error() {
        let error = Error::non_finite("soc_pct", f64::INFINITY);
        assert_eq!(error.http_status(), 500);
        assert!(error.to_string().contains("defect in Tonans"));
        assert!(error.to_string().contains("support@draconitic.com"));
    }

    #[test]
    fn finite_guard_passes_real_numbers_and_rejects_the_rest() {
        assert!(finite("x", 1.5).is_ok());
        assert!(finite("x", 0.0).is_ok());
        assert!(finite("x", f64::NAN).is_err());
        assert!(finite("x", f64::INFINITY).is_err());
        assert!(finite("x", f64::NEG_INFINITY).is_err());
    }

    #[test]
    fn ledger_deduplicates_by_code_and_sorts_by_severity() {
        let mut ledger = WarningLedger::new();
        ledger.push(Warning::info("b.info", "first"));
        ledger.push(Warning::critical("a.critical", "second"));
        ledger.push(Warning::info("b.info", "duplicate, dropped"));
        ledger.push(Warning::caution("c.caution", "third"));

        assert!(ledger.has_at_least(Severity::Critical));
        let sorted = ledger.into_sorted();
        assert_eq!(sorted.len(), 3);
        assert_eq!(sorted[0].code, "a.critical");
        assert_eq!(sorted[1].code, "c.caution");
        assert_eq!(sorted[2].code, "b.info");
        assert_eq!(sorted[2].message, "first");
    }

    #[test]
    fn ledger_is_bounded() {
        let mut ledger = WarningLedger::new();
        for i in 0..(crate::limits::MAX_WARNINGS * 2) {
            ledger.push(Warning::info(format!("code.{i}"), "x"));
        }
        assert_eq!(ledger.into_sorted().len(), crate::limits::MAX_WARNINGS);
    }
}
