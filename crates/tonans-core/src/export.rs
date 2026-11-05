//! The export bundle.
//!
//! One directory holding everything a reader needs and nothing they have to take
//! on trust:
//!
//! | File | What it is |
//! |---|---|
//! | `report.html` | The self-contained report. No script, no network. |
//! | `summary.json` | Every result, canonically serialized. |
//! | `scenarios.csv` | One row per scenario, for a spreadsheet. |
//! | `timeseries-<id>.csv` | Every interval, at full resolution. |
//! | `sensitivity-<id>.csv` | Every cell of a sensitivity study. |
//! | `provenance.json` | Input hashes, versions, assumptions, warnings, limits. |
//! | `SHA256SUMS.txt` | A digest of every file above. |
//!
//! `SHA256SUMS.txt` is written last and covers `provenance.json`, which closes the
//! loop: the manifest cannot contain its own hash, but the checksum file can. It is
//! in the format `sha256sum -c` and PowerShell's `Get-FileHash` both understand, so
//! a recipient can verify the bundle with tools they already have.
//!
//! Every CSV header carries its unit. A column called `grid_import` would be a
//! defect; it is `grid_import_kwh`.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static STAGING_SEQUENCE: AtomicU64 = AtomicU64::new(0);

use crate::canonical::{canonical_hash, canonical_json, hash_bytes};
use crate::error::{Error, Result};
use crate::provenance::{Artifact, Provenance};
use crate::report::{self, ReportInput};
use crate::run::RunResult;
use crate::schema::Project;
use crate::sensitivity::SensitivityResult;
use crate::store::write_atomically;
use crate::units::{round_money, round_physical};

/// A written bundle.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExportBundle {
    /// The directory it was written to.
    pub directory: String,
    /// Full SHA-256 key of every artifact-determining input to this export.
    pub key: String,
    /// Every file written, with its digest.
    pub artifacts: Vec<Artifact>,
}

/// Write the full bundle.
pub fn write_bundle(
    directory: &Path,
    project: &Project,
    run: &RunResult,
    sensitivities: &[SensitivityResult],
) -> Result<ExportBundle> {
    let parent = directory.parent().ok_or_else(|| {
        Error::validation(
            "validation.export_no_parent",
            "export.directory",
            "the export directory has no parent",
        )
    })?;
    fs::create_dir_all(parent).map_err(|e| Error::io("creating the export parent", e))?;

    // Exclusively create our own sibling directory. Never remove a pre-existing
    // staging path: it may belong to another export or contain unrelated data.
    let staging = loop {
        let candidate = parent.join(format!(
            ".tonans-export-{}-{}",
            std::process::id(),
            STAGING_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::create_dir(&candidate) {
            Ok(()) => break candidate,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(Error::io("creating the export staging directory", error)),
        }
    };

    let outcome = write_bundle_into(&staging, project, run, sensitivities);
    let mut bundle = match outcome {
        Ok(bundle) => bundle,
        Err(error) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(error);
        }
    };
    if directory.exists() {
        let existing = (|| {
            if !directory.is_dir()
                || fs::symlink_metadata(directory)
                    .map_err(|e| Error::io("inspecting the export destination", e))?
                    .file_type()
                    .is_symlink()
            {
                return Err(Error::validation(
                    "validation.export_destination_exists",
                    "export.directory",
                    "the export destination is not an ordinary directory",
                ));
            }
            let entries = fs::read_dir(directory)
                .map_err(|e| Error::io("reading the export destination", e))?
                .collect::<std::io::Result<Vec<_>>>()
                .map_err(|e| Error::io("reading the export entries", e))?;
            if entries.is_empty() {
                fs::remove_dir(directory)
                    .map_err(|e| Error::io("removing the empty export destination", e))?;
                return Ok(false);
            }
            // Reuse only the complete, byte-identical bundle. Extra files,
            // symlinks and modified artifacts are preserved and refused.
            let identical = entries.len() == bundle.artifacts.len()
                && bundle.artifacts.iter().all(|artifact| {
                    let path = directory.join(&artifact.filename);
                    fs::symlink_metadata(&path)
                        .is_ok_and(|m| m.file_type().is_file() && m.len() == artifact.size_bytes)
                        && fs::read(&path).is_ok_and(|bytes| hash_bytes(&bytes) == artifact.sha256)
                });
            if identical {
                return Ok(true);
            }
            Err(Error::validation(
                "validation.export_destination_not_empty",
                "export.directory",
                "the export destination contains different files; choose a new directory",
            ))
        })();
        match existing {
            Ok(true) => {
                let _ = fs::remove_dir_all(&staging);
                bundle.directory = directory.display().to_string();
                return Ok(bundle);
            }
            Ok(false) => {}
            Err(error) => {
                let _ = fs::remove_dir_all(&staging);
                return Err(error);
            }
        }
    }
    if let Err(error) = fs::rename(&staging, directory) {
        let _ = fs::remove_dir_all(&staging);
        return Err(Error::io("promoting the complete export bundle", error));
    }
    bundle.directory = directory.display().to_string();
    Ok(bundle)
}

fn write_bundle_into(
    directory: &Path,
    project: &Project,
    run: &RunResult,
    sensitivities: &[SensitivityResult],
) -> Result<ExportBundle> {
    let provenance = Provenance::build(project, run, sensitivities)?;
    let mut artifacts: Vec<Artifact> = Vec::new();

    let report_html = report::render(&ReportInput {
        project,
        run,
        sensitivities,
        provenance: &provenance,
    })?;
    write_artifact(
        directory,
        &mut artifacts,
        "report.html",
        report_html.as_bytes(),
        "Self-contained HTML report.",
    )?;

    let summary = canonical_json(run)?;
    write_artifact(
        directory,
        &mut artifacts,
        "summary.json",
        summary.as_bytes(),
        "Every result, canonically serialized with sorted keys.",
    )?;

    write_artifact(
        directory,
        &mut artifacts,
        "scenarios.csv",
        scenarios_csv(run).as_bytes(),
        "One row per scenario. Every column names its unit.",
    )?;

    for scenario in &run.scenarios {
        if scenario.timeline.is_empty() {
            continue;
        }
        let name = format!("timeseries-{}.csv", scenario.scenario_id);
        write_artifact(
            directory,
            &mut artifacts,
            &name,
            timeseries_csv(scenario).as_bytes(),
            "Every interval at full resolution.",
        )?;
    }

    for (index, study) in sensitivities.iter().enumerate() {
        let study_hash = canonical_hash(study)?;
        let name = format!(
            "sensitivity-{:02}-{}.csv",
            index + 1,
            &study_hash.bare_hex()[..16]
        );
        write_artifact(
            directory,
            &mut artifacts,
            &name,
            sensitivity_csv(study).as_bytes(),
            &format!(
                "Every cell of sensitivity study {} for scenario '{}'.",
                index + 1,
                study.scenario_id
            ),
        )?;
    }

    // The manifest goes second to last: it hashes everything above it.
    let provenance = provenance.with_artifacts(artifacts.clone());
    let provenance_json = serde_json::to_string_pretty(&provenance)
        .map_err(|e| Error::json("writing the provenance manifest", e))?;
    write_artifact(
        directory,
        &mut artifacts,
        "provenance.json",
        provenance_json.as_bytes(),
        "Input hashes, versions, assumptions, warnings, and the enforced limits.",
    )?;

    // ...and the checksums go last, so they cover the manifest too.
    let checksums = checksums_txt(&artifacts);
    write_atomically(&directory.join("SHA256SUMS.txt"), checksums.as_bytes())?;
    artifacts.push(Artifact {
        filename: "SHA256SUMS.txt".to_string(),
        sha256: hash_bytes(checksums.as_bytes()),
        size_bytes: checksums.len() as u64,
        description:
            "SHA-256 of every file in this bundle. Verify with `sha256sum -c` or `Get-FileHash`."
                .to_string(),
    });

    Ok(ExportBundle {
        directory: directory.display().to_string(),
        key: bundle_key(project, run, sensitivities)?,
        artifacts,
    })
}

/// The directory name a run's bundle is filed under.
pub fn bundle_key(
    project: &Project,
    run: &RunResult,
    sensitivities: &[SensitivityResult],
) -> Result<String> {
    #[derive(serde::Serialize)]
    struct ExportIdentity<'a> {
        project: &'a Project,
        run: &'a RunResult,
        numeric_results_hash: crate::canonical::Hash,
        sensitivities: &'a [SensitivityResult],
    }

    Ok(canonical_hash(&ExportIdentity {
        project,
        run,
        numeric_results_hash: run.results_hash()?,
        sensitivities,
    })?
    .bare_hex()
    .to_string())
}

fn write_artifact(
    directory: &Path,
    artifacts: &mut Vec<Artifact>,
    filename: &str,
    bytes: &[u8],
    description: &str,
) -> Result<PathBuf> {
    // The filename is ours, never the user's, but it is still validated: a scenario
    // id reaches this through `timeseries-{id}.csv`, and ids are attacker-adjacent.
    let name = crate::paths::safe_name("artifact name", filename)?;
    let path = directory.join(&name);
    write_atomically(&path, bytes)?;
    artifacts.push(Artifact {
        filename: name,
        sha256: hash_bytes(bytes),
        size_bytes: bytes.len() as u64,
        description: description.to_string(),
    });
    Ok(path)
}

fn checksums_txt(artifacts: &[Artifact]) -> String {
    let mut out = String::new();
    // Sorted by name so the file is byte-identical for identical content, whatever
    // order the artifacts happened to be written in.
    let mut sorted: Vec<&Artifact> = artifacts.iter().collect();
    sorted.sort_by(|a, b| a.filename.cmp(&b.filename));
    for artifact in sorted {
        // Two spaces then the name: the format `sha256sum -c` expects.
        let _ = writeln!(out, "{}  {}", artifact.sha256.bare_hex(), artifact.filename);
    }
    out
}

// ---------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------

/// Quote a CSV field.
///
/// A scenario name can contain a comma, a quote, or — if someone is trying it on —
/// a leading `=`. The first two are handled by RFC 4180 quoting. The third is the
/// spreadsheet formula-injection problem: a cell beginning with one of
/// [`FORMULA_LEADS`] is *executed* by Excel and by Calc when the file is opened,
/// and `=cmd|'/c calc'!A1` is a DDE payload, not a label. Prefixing a single quote
/// neutralises it without changing what a human reads.
///
/// A defused field is then always quoted, even when RFC 4180 would not require it.
/// That is OWASP's remediation for this exact threat, and the reason is that a
/// *bare* field whose first byte is `'` or a tab is at the mercy of the importer:
/// one that trims leading whitespace, or that strips a text-prefix marker, undoes
/// the neutralisation. A quoted field has one reading, in every parser.
fn field(value: &str) -> String {
    let defused = value.starts_with(FORMULA_LEADS);
    let body = if defused {
        format!("'{value}")
    } else {
        value.to_string()
    };
    if defused || body.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", body.replace('"', "\"\""))
    } else {
        body
    }
}

/// The characters that make a spreadsheet treat a cell as a formula.
///
/// `-` is on the list, and belongs there. The negative *number* -1.5 never reaches
/// [`field`] — numbers are rendered by [`number`] and [`money`], which do not quote
/// and are never user-controlled — so the only thing that can arrive here starting
/// with a minus is a *name*, and Excel evaluates `-1+1+cmd|'/c calc'!A0` just as
/// eagerly as it evaluates the `=` form.
const FORMULA_LEADS: [char; 6] = ['=', '+', '-', '@', '\t', '\r'];

fn number(value: f64) -> String {
    format!("{}", round_physical(value))
}

fn money(value: f64) -> String {
    format!("{:.2}", round_money(value))
}

/// Preserve interval-level currency closely enough that the maximum supported
/// time grid still reconciles to its aggregate cost. Customer-facing totals use
/// [`money`]; this formatter is only for machine-readable timeline rows.
fn interval_money(value: f64) -> String {
    let rendered = format!("{value:.12}");
    let trimmed = rendered.trim_end_matches('0').trim_end_matches('.');
    if trimmed == "-0" {
        "0".to_string()
    } else {
        trimmed.to_string()
    }
}

fn scenarios_csv(run: &RunResult) -> String {
    let mut out = String::with_capacity(4_096);
    out.push_str(concat!(
        "scenario_id,scenario_name,dispatch_policy,is_baseline,",
        "load_kwh,served_load_kwh,critical_load_kwh,unmet_critical_kwh,shed_noncritical_kwh,",
        "generation_available_kwh,generation_used_kwh,generation_to_load_kwh,generation_to_battery_kwh,generation_to_grid_kwh,curtailed_kwh,",
        "battery_charge_kwh,battery_discharge_kwh,battery_to_load_kwh,battery_to_grid_kwh,conversion_loss_kwh,self_discharge_loss_kwh,",
        "grid_import_kwh,grid_to_load_kwh,grid_to_battery_kwh,grid_export_kwh,peak_import_kw,peak_export_kw,",
        "generation_origin_battery_to_load_kwh,grid_origin_battery_to_load_kwh,initial_origin_battery_to_load_kwh,",
        "generation_origin_battery_to_grid_kwh,grid_origin_battery_to_grid_kwh,initial_origin_battery_to_grid_kwh,",
        "min_soc_pct,max_soc_pct,self_consumption_pct,self_sufficiency_pct,equivalent_full_cycles,",
        "energy_balance_residual_kwh,",
        "import_energy_cost_usd,export_credit_usd,demand_charge_usd,fixed_charge_usd,total_grid_cost_usd,",
        "annualization_multiplier,baseline_annual_grid_cost_usd,scenario_annual_grid_cost_usd,",
        "year_one_gross_savings_usd,year_one_energy_savings_usd,year_one_non_energy_savings_usd,annual_throughput_kwh,equivalent_full_cycles_per_year,",
        "annual_fade_pct_per_year,capex_usd,npv_usd,simple_payback_years,discounted_payback_years,",
        "grid_cost_delta_usd,peak_import_delta_kw,unmet_critical_delta_kwh\n"
    ));

    for s in &run.scenarios {
        let (e, c, k, b) = (&s.energy, &s.cost, &s.economics, &s.comparison);
        let _ = writeln!(
            out,
            "{id},{name},{policy},{baseline},\
             {load},{served},{critical},{unmet},{shed},\
             {gen_avail},{gen_used},{gen_load},{gen_battery},{gen_grid},{curtailed},\
             {charge},{discharge},{battery_load},{battery_grid},{conv},{standing},\
             {import},{grid_load},{grid_battery},{export},{peak_in},{peak_out},\
             {origin_gen_load},{origin_grid_load},{origin_initial_load},\
             {origin_gen_grid},{origin_grid_grid},{origin_initial_grid},\
             {min_soc},{max_soc},{self_cons},{self_suff},{cycles},\
             {residual},\
             {import_cost},{export_credit},{demand},{fixed},{total},\
             {annualization},{baseline_cost},{scenario_cost},\
             {savings},{energy_savings},{non_energy_savings},{throughput},{cycles_year},\
             {fade},{capex},{npv},{payback},{disc_payback},\
             {cost_delta},{peak_delta},{unmet_delta}",
            id = field(&s.scenario_id),
            name = field(&s.scenario_name),
            policy = field(&s.dispatch_policy),
            baseline = b.is_baseline,
            load = number(e.load_kwh),
            served = number(e.served_load_kwh),
            critical = number(e.critical_load_kwh),
            unmet = number(e.unmet_critical_kwh),
            shed = number(e.shed_noncritical_kwh),
            gen_avail = number(e.generation_available_kwh),
            gen_used = number(e.generation_used_kwh),
            gen_load = number(e.generation_to_load_kwh),
            gen_battery = number(e.generation_to_battery_kwh),
            gen_grid = number(e.generation_to_grid_kwh),
            curtailed = number(e.curtailed_kwh),
            charge = number(e.battery_charge_kwh),
            discharge = number(e.battery_discharge_kwh),
            battery_load = number(e.battery_to_load_kwh),
            battery_grid = number(e.battery_to_grid_kwh),
            conv = number(e.conversion_loss_kwh),
            standing = number(e.self_discharge_loss_kwh),
            import = number(e.grid_import_kwh),
            grid_load = number(e.grid_to_load_kwh),
            grid_battery = number(e.grid_to_battery_kwh),
            export = number(e.grid_export_kwh),
            peak_in = number(e.peak_import_kw),
            peak_out = number(e.peak_export_kw),
            origin_gen_load = number(e.generation_origin_battery_to_load_kwh),
            origin_grid_load = number(e.grid_origin_battery_to_load_kwh),
            origin_initial_load = number(e.initial_origin_battery_to_load_kwh),
            origin_gen_grid = number(e.generation_origin_battery_to_grid_kwh),
            origin_grid_grid = number(e.grid_origin_battery_to_grid_kwh),
            origin_initial_grid = number(e.initial_origin_battery_to_grid_kwh),
            min_soc = number(e.min_soc_pct),
            max_soc = number(e.max_soc_pct),
            self_cons = number(e.self_consumption_pct),
            self_suff = number(e.self_sufficiency_pct),
            cycles = number(e.equivalent_full_cycles),
            residual = number(e.energy_balance_residual_kwh),
            import_cost = money(c.import_energy_cost_usd),
            export_credit = money(c.export_credit_usd),
            demand = money(c.demand_charge_usd),
            fixed = money(c.fixed_charge_usd),
            total = money(c.total_grid_cost_usd),
            annualization = number(k.annualization_multiplier),
            baseline_cost = money(k.baseline_annual_grid_cost_usd),
            scenario_cost = money(k.scenario_annual_grid_cost_usd),
            savings = money(k.year_one_gross_savings_usd),
            energy_savings = money(k.year_one_energy_savings_usd),
            non_energy_savings = money(k.year_one_non_energy_savings_usd),
            throughput = number(k.annual_throughput_kwh),
            cycles_year = number(k.equivalent_full_cycles_per_year),
            fade = number(k.annual_fade_pct_per_year),
            capex = money(k.capex_usd),
            npv = money(k.npv_usd),
            payback = k
                .simple_payback_years
                .map_or_else(String::new, |y| format!("{y:.4}")),
            disc_payback = k
                .discounted_payback_years
                .map_or_else(String::new, |y| format!("{y:.4}")),
            cost_delta = money(b.grid_cost_delta_usd),
            peak_delta = number(b.peak_import_delta_kw),
            unmet_delta = number(b.unmet_critical_delta_kwh),
        );
    }
    out
}

fn timeseries_csv(scenario: &crate::run::ScenarioResult) -> String {
    let mut out = String::with_capacity(scenario.timeline.len() * 180 + 512);
    out.push_str(concat!(
        "start_utc,outage,",
        "load_kwh,critical_load_kwh,served_load_kwh,shed_noncritical_kwh,unmet_critical_kwh,",
        "generation_available_kwh,generation_used_kwh,generation_to_load_kwh,generation_to_battery_kwh,generation_to_grid_kwh,curtailed_kwh,",
        "battery_charge_kwh,battery_discharge_kwh,battery_to_load_kwh,battery_to_grid_kwh,stored_energy_kwh,soc_pct,",
        "grid_import_kwh,grid_to_load_kwh,grid_to_battery_kwh,grid_export_kwh,",
        "generation_origin_battery_to_load_kwh,grid_origin_battery_to_load_kwh,initial_origin_battery_to_load_kwh,",
        "generation_origin_battery_to_grid_kwh,grid_origin_battery_to_grid_kwh,initial_origin_battery_to_grid_kwh,",
        "conversion_loss_kwh,self_discharge_loss_kwh,",
        "import_cost_usd,export_credit_usd\n"
    ));
    for row in &scenario.timeline {
        let _ = writeln!(
            out,
            "{start},{outage},{load},{critical},{served},{shed},{unmet},\
             {gen_avail},{gen_used},{gen_load},{gen_battery},{gen_grid},{curtailed},\
             {charge},{discharge},{battery_load},{battery_grid},{stored},{soc},\
             {import},{grid_load},{grid_battery},{export},\
             {origin_gen_load},{origin_grid_load},{origin_initial_load},\
             {origin_gen_grid},{origin_grid_grid},{origin_initial_grid},\
             {conv},{standing},\
             {import_cost},{export_credit}",
            start = row.start_utc.to_rfc3339(),
            outage = row.outage,
            load = number(row.load_kwh),
            critical = number(row.critical_load_kwh),
            served = number(row.served_load_kwh),
            shed = number(row.shed_noncritical_kwh),
            unmet = number(row.unmet_critical_kwh),
            gen_avail = number(row.generation_available_kwh),
            gen_used = number(row.generation_used_kwh),
            gen_load = number(row.generation_to_load_kwh),
            gen_battery = number(row.generation_to_battery_kwh),
            gen_grid = number(row.generation_to_grid_kwh),
            curtailed = number(row.curtailed_kwh),
            charge = number(row.battery_charge_kwh),
            discharge = number(row.battery_discharge_kwh),
            battery_load = number(row.battery_to_load_kwh),
            battery_grid = number(row.battery_to_grid_kwh),
            stored = number(row.stored_energy_kwh),
            soc = number(row.soc_pct),
            import = number(row.grid_import_kwh),
            grid_load = number(row.grid_to_load_kwh),
            grid_battery = number(row.grid_to_battery_kwh),
            export = number(row.grid_export_kwh),
            origin_gen_load = number(row.generation_origin_battery_to_load_kwh),
            origin_grid_load = number(row.grid_origin_battery_to_load_kwh),
            origin_initial_load = number(row.initial_origin_battery_to_load_kwh),
            origin_gen_grid = number(row.generation_origin_battery_to_grid_kwh),
            origin_grid_grid = number(row.grid_origin_battery_to_grid_kwh),
            origin_initial_grid = number(row.initial_origin_battery_to_grid_kwh),
            conv = number(row.conversion_loss_kwh),
            standing = number(row.self_discharge_loss_kwh),
            // Per-interval money keeps twelve decimal places so the maximum
            // supported time grid reconciles to the scenario total. Cents are a
            // presentation boundary for totals, not a license to erase small
            // interval charges.
            import_cost = interval_money(row.import_cost_usd),
            export_credit = interval_money(row.export_credit_usd),
        );
    }
    out
}

fn sensitivity_csv(study: &SensitivityResult) -> String {
    let mut out = String::with_capacity(2_048);
    out.push_str("scenario_id");
    for (index, axis) in study.axes.iter().enumerate() {
        let _ = write!(out, ",axis{}_param,axis{}_value", index + 1, index + 1);
        let _ = axis;
    }
    out.push_str(concat!(
        ",installed_cost_usd,capex_per_kwh_usd,",
        "annual_grid_cost_usd,annual_savings_usd,npv_usd,simple_payback_years,",
        "peak_import_kw,grid_import_kwh,unmet_critical_kwh,self_sufficiency_pct,",
        "min_soc_pct,equivalent_full_cycles\n"
    ));

    for cell in &study.cells {
        out.push_str(&field(&study.scenario_id));
        for (axis, value) in study.axes.iter().zip(&cell.axis_values) {
            let _ = write!(out, ",{},{}", field(axis.param.label()), number(*value));
        }
        let m = &cell.metrics;
        let _ = writeln!(
            out,
            ",{installed},{rate},{cost},{saving},{npv},{payback},{peak},{import},{unmet},{sufficiency},{soc},{cycles}",
            installed = money(m.installed_cost_usd),
            rate = study.capex_per_kwh_usd.map_or_else(String::new, number),
            cost = money(m.annual_grid_cost_usd),
            saving = money(m.annual_savings_usd),
            npv = money(m.npv_usd),
            payback = m
                .simple_payback_years
                .map_or_else(String::new, |y| format!("{y:.4}")),
            peak = number(m.peak_import_kw),
            import = number(m.grid_import_kwh),
            unmet = number(m.unmet_critical_kwh),
            sufficiency = number(m.self_sufficiency_pct),
            soc = number(m.min_soc_pct),
            cycles = number(m.equivalent_full_cycles),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::csvimport::{self, ColumnMapping, ImportRequest, ImportUnit, MappedColumn};
    use crate::run;
    use crate::schema::{GapPolicy, Project, RateSource, SeriesKind, Site};
    use crate::sensitivity::{self, SensitivityAxis, SensitivityParam, SensitivityRequest};

    static TEST_DIRECTORY_ID: AtomicU64 = AtomicU64::new(0);

    fn test_directory(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "tonans-export-{label}-{}-{}",
            std::process::id(),
            TEST_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn runnable_project() -> (Project, RunResult) {
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let mut series = BTreeMap::new();
        series.insert(
            SeriesKind::Load,
            MappedColumn {
                header: "load_kw".to_string(),
                unit: ImportUnit::Kilowatt,
            },
        );
        let dataset = csvimport::import(
            b"timestamp,load_kw\n2026-01-01T00:00:00Z,10\n",
            &ImportRequest {
                filename: "meter.csv".to_string(),
                timezone: "UTC".to_string(),
                interval_minutes: 60,
                gap_policy: GapPolicy::Reject,
                mapping: ColumnMapping {
                    timestamp: "timestamp".to_string(),
                    series,
                    demand_period: None,
                    ignore_columns: Vec::new(),
                },
            },
            now,
        )
        .unwrap()
        .dataset;
        let mut project = Project::new(
            "export-test",
            "Export test",
            Site {
                timezone: "UTC".to_string(),
                currency: "USD".to_string(),
            },
            now,
        )
        .unwrap();
        project.dataset = Some(dataset);
        let run = run::run_project(&project, now).unwrap();
        (project, run)
    }

    fn sensitivity(project: &Project, capex: f64) -> SensitivityResult {
        sensitivity::run(
            project,
            &SensitivityRequest {
                scenario_id: project.baseline_scenario_id.clone(),
                axes: vec![SensitivityAxis {
                    param: SensitivityParam::CapexUsd,
                    values: vec![capex],
                }],
                capex_per_kwh_usd: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn csv_fields_are_quoted_and_formula_injection_is_neutralised() {
        assert_eq!(field("plain"), "plain");
        assert_eq!(field("has,comma"), "\"has,comma\"");
        assert_eq!(field("has\"quote"), "\"has\"\"quote\"");
        assert_eq!(field("line\nbreak"), "\"line\nbreak\"");

        // Excel executes a cell that begins with =, +, -, @, tab, or CR. A scenario
        // name is user-controlled, so it reaches a spreadsheet only after being
        // defused — prefixed with a quote *and* wrapped, so no importer can undo it.
        assert_eq!(field("=1+1"), "\"'=1+1\"");
        assert_eq!(field("=cmd|'/c calc'!A1"), "\"'=cmd|'/c calc'!A1\"");
        assert_eq!(field("@SUM(A1)"), "\"'@SUM(A1)\"");
        assert_eq!(field("+1"), "\"'+1\"");
        assert_eq!(field("\tlead"), "\"'\tlead\"");

        // The regression: a leading `-` was treated as "just a number" and passed
        // through. It is not a number here — `field` only ever renders *names*, and
        // Excel evaluates this one.
        assert_eq!(field("-1+1+cmd|'/c calc'!A0"), "\"'-1+1+cmd|'/c calc'!A0\"");
        assert_eq!(field("-1"), "\"'-1\"");

        // An embedded quote in a defused field is still doubled, so the field never
        // breaks out of its own quoting.
        assert_eq!(field("=\"x\""), "\"'=\"\"x\"\"\"");
    }

    #[test]
    fn numbers_are_never_defused_because_they_never_pass_through_field() {
        // The reason `-` can safely be on the dangerous list: the CSV writers send
        // numbers through `number`/`money`, never `field`. If that ever changed, a
        // negative number would start arriving as `'-1.5` and this test would be the
        // thing that noticed.
        assert_eq!(number(-1.5), "-1.5");
        assert_eq!(money(-1.5), "-1.50");
        assert!(!number(-1.5).starts_with('\''));
        assert!(!money(-1.5).starts_with('\''));
    }

    #[test]
    fn checksums_are_sorted_and_in_the_standard_format() {
        let artifacts = vec![
            Artifact {
                filename: "zebra.json".into(),
                sha256: hash_bytes(b"z"),
                size_bytes: 1,
                description: String::new(),
            },
            Artifact {
                filename: "alpha.html".into(),
                sha256: hash_bytes(b"a"),
                size_bytes: 1,
                description: String::new(),
            },
        ];
        let text = checksums_txt(&artifacts);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("  alpha.html"), "{}", lines[0]);
        assert!(lines[1].ends_with("  zebra.json"), "{}", lines[1]);
        // 64 hex characters, two spaces, the name: what sha256sum -c reads.
        let digest = lines[0].split("  ").next().unwrap();
        assert_eq!(digest.len(), 64);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!digest.starts_with("sha256:"));
    }

    #[test]
    fn numbers_never_serialize_in_scientific_notation() {
        // A CSV consumer that reads "1e-7" as text and not a number is a support
        // ticket. Rounding to six decimals keeps every value in plain decimal.
        assert_eq!(number(0.0000001), "0");
        assert_eq!(number(1234.5), "1234.5");
        assert_eq!(money(1234.567), "1234.57");
        assert!(!number(1e-9).contains('e'));
    }

    #[test]
    fn a_nonempty_destination_is_refused_without_touching_its_contents() {
        let (project, run) = runnable_project();
        let directory = test_directory("nonempty");
        fs::create_dir_all(&directory).unwrap();
        let marker = directory.join("keep.txt");
        fs::write(&marker, b"user data").unwrap();

        let error = write_bundle(&directory, &project, &run, &[]).unwrap_err();
        assert_eq!(error.code(), "validation.export_destination_not_empty");
        assert_eq!(fs::read(&marker).unwrap(), b"user data");

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn export_preserves_unrelated_staging_and_reuses_only_identical_bundles() {
        let (project, run) = runnable_project();
        let directory = test_directory("reuse");
        let unrelated = directory.with_extension("tonans-staging");
        fs::create_dir_all(&unrelated).unwrap();
        fs::write(unrelated.join("keep.txt"), b"unrelated data").unwrap();
        let first = write_bundle(&directory, &project, &run, &[]).unwrap();
        let second = write_bundle(&directory, &project, &run, &[]).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            fs::read(unrelated.join("keep.txt")).unwrap(),
            b"unrelated data"
        );
        fs::write(directory.join("summary.json"), b"user modification").unwrap();
        assert_eq!(
            write_bundle(&directory, &project, &run, &[])
                .unwrap_err()
                .code(),
            "validation.export_destination_not_empty"
        );
        assert_eq!(
            fs::read(directory.join("summary.json")).unwrap(),
            b"user modification"
        );
        fs::remove_dir_all(directory).unwrap();
        fs::remove_dir_all(unrelated).unwrap();
    }

    #[test]
    fn bundle_identity_and_artifacts_cover_server_computed_sensitivities() {
        let (project, run) = runnable_project();
        let plain_directory = test_directory("plain");
        let study_a = sensitivity(&project, 100.0);
        let study_b = sensitivity(&project, 200.0);
        let plain = write_bundle(&plain_directory, &project, &run, &[]).unwrap();

        let study_directory = test_directory("study");
        let with_studies = write_bundle(
            &study_directory,
            &project,
            &run,
            &[study_a.clone(), study_b.clone()],
        )
        .unwrap();
        assert_ne!(plain.key, with_studies.key);
        assert_eq!(
            with_studies.key,
            bundle_key(&project, &run, &[study_a, study_b]).unwrap()
        );

        let sensitivity_files: Vec<_> = with_studies
            .artifacts
            .iter()
            .filter(|artifact| artifact.filename.starts_with("sensitivity-"))
            .map(|artifact| artifact.filename.as_str())
            .collect();
        assert_eq!(sensitivity_files.len(), 2);
        assert_ne!(sensitivity_files[0], sensitivity_files[1]);

        for artifact in &with_studies.artifacts {
            let bytes = fs::read(study_directory.join(&artifact.filename)).unwrap();
            assert_eq!(hash_bytes(&bytes), artifact.sha256, "{}", artifact.filename);
            assert_eq!(bytes.len() as u64, artifact.size_bytes);
        }
        let provenance: Provenance =
            serde_json::from_slice(&fs::read(study_directory.join("provenance.json")).unwrap())
                .unwrap();
        assert_eq!(provenance.sensitivity_studies.len(), 2);
        // A study's warnings change how its numbers read, so the report carries them.
        let report = fs::read_to_string(study_directory.join("report.html")).unwrap();
        assert!(report.contains("sensitivity.per_cell_simulation"));
        let checksum_lines = fs::read_to_string(study_directory.join("SHA256SUMS.txt"))
            .unwrap()
            .lines()
            .count();
        assert_eq!(checksum_lines + 1, with_studies.artifacts.len());

        fs::remove_dir_all(plain_directory).unwrap();
        fs::remove_dir_all(study_directory).unwrap();
    }

    #[test]
    fn full_resolution_money_rows_preserve_sub_cent_charges() {
        let (mut project, _) = runnable_project();
        let dataset = project.dataset.as_mut().unwrap();
        dataset.interval_minutes = 1;
        dataset.series.get_mut(&SeriesKind::Load).unwrap().values[0] = 2.94;
        project.scenarios[0].tariff.import = RateSource::Flat {
            price_per_kwh_usd: 0.10,
        };
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let run = run::run_project(&project, now).unwrap();
        let scenario = &run.scenarios[0];
        let csv = timeseries_csv(scenario);
        let headers: Vec<_> = csv.lines().next().unwrap().split(',').collect();
        let values: Vec<_> = csv.lines().nth(1).unwrap().split(',').collect();
        let import_cost_index = headers
            .iter()
            .position(|header| *header == "import_cost_usd")
            .unwrap();
        let row_cost: f64 = values[import_cost_index].parse().unwrap();

        assert!((row_cost - 0.0049).abs() <= 1e-9, "{csv}");
        assert!((row_cost - scenario.cost.import_energy_cost_usd).abs() <= 1e-6);

        let tiny_row_cost = 0.000_000_49;
        let rendered_sum: f64 = (0..105_120)
            .map(|_| interval_money(tiny_row_cost).parse::<f64>().unwrap())
            .sum();
        let expected_sum = tiny_row_cost * 105_120.0;
        assert_eq!(interval_money(tiny_row_cost), "0.00000049");
        assert!((rendered_sum - expected_sum).abs() <= 1e-12);
    }
}
