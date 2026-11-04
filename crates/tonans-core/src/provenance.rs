//! The provenance manifest.
//!
//! A report is a claim. The manifest is what makes the claim checkable: the exact
//! bytes that went in, the exact version that processed them, the exact
//! assumptions, the warnings, and a digest of every artifact produced. A reader
//! who has the manifest and the source CSV can prove — not assume — that a given
//! report came from that data and no other.
//!
//! Nothing here is sent anywhere. The manifest is a file on the user's disk. There
//! is no identifier in it that could distinguish one installation from another: no
//! machine name, no user name, no install id, no path outside the workspace. See
//! `docs/PRIVACY.md`.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::canonical::{canonical_hash, Hash};
use crate::error::{Result, Warning};
use crate::limits::Limits;
use crate::run::RunResult;
use crate::schema::{Dataset, Project, Scenario};
use crate::sensitivity::{SensitivityAxis, SensitivityResult};
use crate::units::{unit_for_field, Unit};
use crate::version::{
    APP_VERSION, MODEL_EXCLUSIONS, MODEL_VERSION, PLANNING_DISCLAIMER, PRODUCT_NAME,
    SCHEMA_VERSION, SUPPORT_EMAIL, VENDOR_NAME,
};

/// What produced the artifacts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolInfo {
    /// The product.
    pub name: String,
    /// Who makes it.
    pub vendor: String,
    /// Where to report a defect.
    pub support: String,
    /// Application version.
    pub app_version: String,
    /// Energy-model version. Changes whenever a number could change.
    pub model_version: String,
    /// Project-schema version.
    pub schema_version: u32,
    /// Target the binary was built for. Not an identifier: it says `windows/x86_64`,
    /// not which machine.
    pub platform: String,
}

impl ToolInfo {
    /// The tool that is running now.
    #[must_use]
    pub fn current() -> Self {
        Self {
            name: PRODUCT_NAME.to_string(),
            vendor: VENDOR_NAME.to_string(),
            support: SUPPORT_EMAIL.to_string(),
            app_version: APP_VERSION.to_string(),
            model_version: MODEL_VERSION.to_string(),
            schema_version: SCHEMA_VERSION,
            platform: format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
        }
    }
}

/// The project the results came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectProvenance {
    /// Its name.
    pub name: String,
    /// Its identifier.
    pub id: String,
    /// Hash of the whole project file.
    pub project_hash: Hash,
    /// Hash of only what determines the numbers.
    pub model_input_hash: Hash,
    /// Site timezone.
    pub timezone: String,
    /// Site currency.
    pub currency: String,
    /// When the project was created.
    pub created_utc: DateTime<Utc>,
    /// When it was last written.
    pub modified_utc: DateTime<Utc>,
}

/// An imported file, and how it was read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputFile {
    /// The file's name as supplied.
    pub filename: String,
    /// SHA-256 of the exact bytes imported, before parsing.
    pub sha256: Hash,
    /// Size of those bytes.
    pub size_bytes: u64,
    /// Data rows read.
    pub row_count: usize,
    /// When it was imported.
    pub imported_utc: DateTime<Utc>,
    /// Interval duration.
    pub interval_minutes: u32,
    /// Intervals in the resulting data set.
    pub interval_count: usize,
    /// First interval.
    pub start_utc: DateTime<Utc>,
    /// End of the last interval.
    pub end_utc: DateTime<Utc>,
    /// Which source column was read as what, with the unit that was declared.
    pub column_mapping: BTreeMap<String, String>,
    /// Columns present in the file and deliberately not imported.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignored_columns: Vec<String>,
    /// What the importer was told to do about missing intervals.
    pub gap_policy: String,
    /// How many intervals were synthesized rather than measured.
    pub synthesized_interval_count: usize,
    /// Which ones, up to the recorded limit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub synthesized_interval_indices: Vec<usize>,
}

/// One scenario's exact assumptions and fingerprint.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScenarioProvenance {
    /// Its id.
    pub id: String,
    /// Its name.
    pub name: String,
    /// The dispatch rule it ran under.
    pub dispatch_policy: String,
    /// Hash of the full assumption set. Two scenarios with the same hash are the
    /// same scenario, whatever they are called.
    pub assumptions_hash: Hash,
    /// The exact assumptions used. The hash above makes comparison quick; this
    /// field makes the claim independently inspectable.
    pub assumptions: Scenario,
}

/// One server-computed sensitivity study included in the export.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SensitivityProvenance {
    /// Scenario varied by the study.
    pub scenario_id: String,
    /// Exact ordered axes and values that were recomputed for this export.
    pub axes: Vec<SensitivityAxis>,
    /// Digest of the complete result, including every cell and warning.
    pub result_hash: Hash,
    /// Study warnings, repeated here so the manifest is useful without opening
    /// the CSV or report.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Warning>,
}

/// A file the export produced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// The file's name inside the bundle.
    pub filename: String,
    /// SHA-256 of its bytes.
    pub sha256: Hash,
    /// Its size.
    pub size_bytes: u64,
    /// What it is.
    pub description: String,
}

/// The manifest.
// Not `Eq`: it carries `Limits`, which holds a float.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// What produced this.
    pub tool: ToolInfo,
    /// When. The only field that differs between two otherwise identical runs.
    pub generated_utc: DateTime<Utc>,
    /// The project.
    pub project: ProjectProvenance,
    /// What went in.
    pub inputs: Vec<InputFile>,
    /// The scenarios that were run.
    pub scenarios: Vec<ScenarioProvenance>,
    /// Sensitivity studies recomputed by Tonans for this export.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sensitivity_studies: Vec<SensitivityProvenance>,
    /// Hash of the results themselves. Rerun the same inputs and this must match.
    pub results_hash: Hash,
    /// What every unit suffix in the machine-readable exports means.
    pub units: BTreeMap<String, String>,
    /// The enforced envelope this run happened inside.
    pub limits: Limits,
    /// Every warning, most severe first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Warning>,
    /// Every file in the bundle, hashed. Empty until [`Provenance::with_artifacts`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
    /// The planning-only disclaimer, in the data rather than only in a template.
    pub disclaimer: String,
    /// What this model does not do.
    pub model_exclusions: Vec<String>,
}

impl Provenance {
    /// Build a manifest for a run.
    pub fn build(
        project: &Project,
        result: &RunResult,
        sensitivities: &[SensitivityResult],
    ) -> Result<Self> {
        let inputs = match &project.dataset {
            Some(dataset) => vec![input_file(project, dataset)?],
            None => Vec::new(),
        };

        let mut scenarios = Vec::with_capacity(result.scenarios.len());
        for scenario in &result.scenarios {
            scenarios.push(ScenarioProvenance {
                id: scenario.scenario_id.clone(),
                name: scenario.scenario_name.clone(),
                dispatch_policy: scenario.dispatch_policy.clone(),
                assumptions_hash: canonical_hash(&scenario.scenario)?,
                assumptions: scenario.scenario.clone(),
            });
        }

        let mut sensitivity_studies = Vec::with_capacity(sensitivities.len());
        for study in sensitivities {
            sensitivity_studies.push(SensitivityProvenance {
                scenario_id: study.scenario_id.clone(),
                axes: study.axes.clone(),
                result_hash: canonical_hash(study)?,
                warnings: study.warnings.clone(),
            });
        }

        Ok(Self {
            tool: ToolInfo::current(),
            generated_utc: result.generated_utc,
            project: ProjectProvenance {
                name: project.name.clone(),
                id: project.id.clone(),
                project_hash: result.project_hash.clone(),
                model_input_hash: result.model_input_hash.clone(),
                timezone: project.site.timezone.clone(),
                currency: project.site.currency.clone(),
                created_utc: project.created_utc,
                modified_utc: project.modified_utc,
            },
            inputs,
            scenarios,
            sensitivity_studies,
            results_hash: result.results_hash()?,
            units: unit_glossary(&project.site.currency),
            limits: result.limits,
            warnings: result.all_warnings(),
            artifacts: Vec::new(),
            disclaimer: PLANNING_DISCLAIMER.to_string(),
            model_exclusions: MODEL_EXCLUSIONS.iter().map(|s| (*s).to_string()).collect(),
        })
    }

    /// Attach the digests of the files the bundle contains.
    ///
    /// Called last, once every other artifact is on disk, because the manifest
    /// cannot contain its own hash. `SHA256SUMS.txt` closes that loop: it is
    /// written after the manifest and covers it too.
    #[must_use]
    pub fn with_artifacts(mut self, artifacts: Vec<Artifact>) -> Self {
        self.artifacts = artifacts;
        self
    }
}

fn input_file(project: &Project, dataset: &Dataset) -> Result<InputFile> {
    let grid = project.grid()?.ok_or_else(|| {
        crate::error::Error::validation("validation.no_dataset", "dataset", "no data set")
    })?;
    Ok(InputFile {
        filename: dataset.source.filename.clone(),
        sha256: dataset.source.sha256.clone(),
        size_bytes: dataset.source.size_bytes,
        row_count: dataset.source.row_count,
        imported_utc: dataset.source.imported_utc,
        interval_minutes: dataset.interval_minutes,
        interval_count: dataset.interval_count,
        start_utc: dataset.start_utc,
        end_utc: grid.end_utc(),
        column_mapping: dataset.source.column_mapping.clone(),
        ignored_columns: dataset.source.ignored_columns.clone(),
        gap_policy: dataset.gap_policy.describe().to_string(),
        synthesized_interval_count: dataset.synthesized_interval_count,
        synthesized_interval_indices: dataset.synthesized_interval_indices.clone(),
    })
}

/// What every unit suffix in the machine-readable exports means.
///
/// Built from the same table [`unit_for_field`] resolves against, so the glossary
/// cannot drift from the behaviour: a suffix that stopped resolving would produce
/// an empty glossary entry and fail the provenance test.
#[must_use]
pub fn unit_glossary(currency: &str) -> BTreeMap<String, String> {
    const SUFFIXES: &[&str] = &[
        "_kwh",
        "_kw",
        "_pct",
        "_usd",
        "_c",
        "_hours",
        "_years",
        "_minutes",
        "_cycles",
        "_bytes",
        "_count",
        "_multiplier",
        "_per_kwh_usd",
        "_per_kw_month_usd",
        "_per_kwh_throughput_usd",
    ];
    let mut glossary = BTreeMap::new();
    for suffix in SUFFIXES {
        // Prefix with a stem so the resolver sees a realistic field name.
        let resolved = unit_for_field(&format!("value{suffix}")).unwrap_or(Unit::Count);
        glossary.insert(
            (*suffix).to_string(),
            resolved.symbol_with_currency(currency),
        );
    }
    glossary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_glossary_resolves_every_suffix_the_exports_use() {
        let glossary = unit_glossary("USD");
        assert_eq!(glossary["_kwh"], "kWh");
        assert_eq!(glossary["_kw"], "kW");
        assert_eq!(glossary["_usd"], "USD");
        assert_eq!(glossary["_per_kwh_usd"], "USD/kWh");
        assert_eq!(glossary["_per_kw_month_usd"], "USD/kW-month");
        assert_eq!(glossary["_pct"], "%");
        assert_eq!(glossary["_c"], "°C");
        // No entry may be blank: a blank unit in the glossary is a quantity a
        // reader cannot interpret.
        for (suffix, symbol) in &glossary {
            if *suffix != "_count" {
                assert!(!symbol.is_empty(), "{suffix} has no unit symbol");
            }
        }
    }

    #[test]
    fn the_tool_stamp_names_no_machine_and_no_user() {
        let tool = ToolInfo::current();
        assert_eq!(tool.vendor, "Flintglade");
        assert_eq!(tool.support, "support@flintglade.com");
        assert!(!tool.app_version.is_empty());
        // Platform is a target triple fragment, never an identifier.
        assert!(tool.platform.contains('/'));
        assert!(!tool.platform.contains('\\'));
        assert!(!tool.platform.contains("Users"));
    }
}
