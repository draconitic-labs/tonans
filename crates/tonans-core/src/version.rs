//! Version identifiers stamped into every result, export, and provenance manifest.

/// Application version, taken from the crate manifest.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Version of the deterministic energy model.
///
/// This is deliberately independent of [`APP_VERSION`]. It must be incremented
/// whenever a change to the engine, dispatch policies, tariff accounting, or
/// economics can alter a numeric result for an unchanged input. UI-only or
/// documentation-only releases leave it untouched, which lets a reader of an old
/// report know whether today's build would still reproduce it.
pub const MODEL_VERSION: &str = "1.1.0";

/// Version of the on-disk project schema. See [`crate::schema`].
pub const SCHEMA_VERSION: u32 = 1;

/// Product name as it appears in the user interface and in every report.
pub const PRODUCT_NAME: &str = "Tonans";

/// Vendor identity. The support address is the only contact channel we publish.
pub const VENDOR_NAME: &str = "Flintglade";

/// Support contact printed in reports and in the application.
pub const SUPPORT_EMAIL: &str = "support@flintglade.com";

/// The disclaimer that must appear on every screen and in every exported artifact.
///
/// [`crate::report`] and [`crate::provenance`] both embed this verbatim, and the
/// report tests assert its presence, so it cannot be dropped by accident.
pub const PLANNING_DISCLAIMER: &str =
    "Planning estimate for human review. Tonans performs deterministic single-bus \
     energy accounting. It is not an AC power-flow study, a protection or interconnection \
     analysis, a safety certification, or a guarantee of savings. Results must be reviewed by \
     a qualified engineer before any procurement or design decision.";

/// Capabilities Tonans deliberately does not have. Rendered in the report so a
/// reader cannot mistake the scope of the analysis.
pub const MODEL_EXCLUSIONS: &[&str] = &[
    "AC power flow, voltage, reactive power, and short-circuit duty are not modelled.",
    "Protection coordination, conductor sizing, and interconnection compliance are not modelled.",
    "Real-time dispatch, device control, and SCADA integration are out of scope by design.",
    "Battery degradation is a transparent linear fade and throughput approximation, not an electrochemical lifetime model.",
    "Dispatch policies are fixed, named rules evaluated interval by interval. Tonans does not optimise and does not claim an optimal result.",
    "A single electrical bus is assumed. Losses inside the site distribution system are not modelled.",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_populated() {
        assert!(!APP_VERSION.is_empty());
        assert_eq!(MODEL_VERSION.split('.').count(), 3);
        assert_eq!(SCHEMA_VERSION, 1);
    }

    #[test]
    fn disclaimer_states_the_boundary() {
        // These phrases are load-bearing: the product boundary in PRODUCT-SPEC.md
        // is only honoured if the artifact itself carries it.
        assert!(PLANNING_DISCLAIMER.contains("Planning estimate"));
        assert!(PLANNING_DISCLAIMER.contains("not an AC power-flow study"));
        assert!(PLANNING_DISCLAIMER.contains("guarantee of savings"));
    }
}
