//! The control API.
//!
//! Every route here takes a project *name*, never a path. The browser cannot hand us
//! a filesystem location and we would not accept one: names are validated by
//! `tonans_core::paths` and joined beneath the workspace root, so traversal is not
//! something to be filtered out — it is something that cannot be expressed.
//!
//! There is no route that reads an arbitrary file, no route that runs a program, and
//! no route that talks to a device. There never will be.

use std::collections::BTreeMap;
use std::sync::atomic::Ordering;

use serde::{Deserialize, Serialize};

use tonans_core::canonical::Hash;
use tonans_core::csvimport::{self, CsvPreview, ImportRequest};
use tonans_core::engine::IntervalRow;
use tonans_core::error::{Error, Result};
use tonans_core::export::{self, ExportBundle};
use tonans_core::limits::{self, Limits};
use tonans_core::paths::source_file_name;
use tonans_core::run::{self, RunResult};
use tonans_core::schema::{Project, Site};
use tonans_core::sensitivity::{self, SensitivityRequest, SensitivityResult};
use tonans_core::store::ProjectEntry;
use tonans_core::svg;
use tonans_core::version::{
    APP_VERSION, MODEL_EXCLUSIONS, MODEL_VERSION, PLANNING_DISCLAIMER, SCHEMA_VERSION,
};

use crate::clock;
use crate::http::{Request, Response};
use crate::server::problem;
use crate::state::{AppState, DEMO_PROJECT_NAME};
use crate::token;

/// Turn a `Result` into a response, preserving the error's own classification.
///
/// An API consumer sees exactly the rejection the model made — never a flattened
/// "400 Bad Request" that hides which row of which column was wrong.
fn respond<T: Serialize>(result: Result<T>) -> Response {
    match result {
        Ok(value) => match serde_json::to_vec(&value) {
            Ok(body) => Response::json(200, body),
            Err(error) => problem(
                "500".parse().unwrap_or(500),
                "json.failed",
                &error.to_string(),
            ),
        },
        Err(error) => problem(error.http_status(), error.code(), &error.to_string()),
    }
}

fn parse_body<T: for<'de> Deserialize<'de>>(request: &Request) -> Result<T> {
    serde_json::from_slice(&request.body).map_err(|e| Error::Schema {
        code: "schema.bad_request_body",
        message: format!("the request body is not valid for this route: {e}"),
    })
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// What the UI needs to render itself honestly.
#[derive(Debug, Serialize)]
pub struct Session {
    /// Application version.
    pub app_version: String,
    /// Energy-model version.
    pub model_version: String,
    /// Project-schema version.
    pub schema_version: u32,
    /// The enforced envelope.
    pub limits: Limits,
    /// Where projects are stored. Shown so the user knows where their data is.
    pub workspace: String,
    /// The reserved name of the bundled demonstration project.
    pub demo_project: String,
    /// The planning-only disclaimer, so the UI cannot render without it.
    pub disclaimer: String,
    /// What the model does not do.
    pub model_exclusions: Vec<String>,
}

/// `GET /api/session`
pub fn session(state: &AppState) -> Response {
    respond(Ok(Session {
        app_version: APP_VERSION.to_string(),
        model_version: MODEL_VERSION.to_string(),
        schema_version: SCHEMA_VERSION,
        limits: Limits::current(),
        workspace: state.workspace.root().display().to_string(),
        demo_project: DEMO_PROJECT_NAME.to_string(),
        disclaimer: PLANNING_DISCLAIMER.to_string(),
        model_exclusions: MODEL_EXCLUSIONS.iter().map(|s| (*s).to_string()).collect(),
    }))
}

// ---------------------------------------------------------------------------
// Projects
// ---------------------------------------------------------------------------

/// `GET /api/projects`
pub fn list_projects(state: &AppState) -> Response {
    #[derive(Serialize)]
    struct Listing {
        projects: Vec<ProjectEntry>,
        user_project_count: usize,
    }
    respond(state.workspace.list_projects().map(|projects| {
        let user_project_count = projects
            .iter()
            .filter(|p| p.name != DEMO_PROJECT_NAME)
            .count();
        Listing {
            projects,
            user_project_count,
        }
    }))
}

/// Body of `POST /api/projects`.
#[derive(Debug, Deserialize)]
pub struct CreateProject {
    /// The project's name. Becomes its file name; validated, never a path.
    pub name: String,
    /// IANA timezone the site is in.
    pub timezone: String,
}

/// `POST /api/projects`
pub fn create_project(state: &AppState, request: &Request) -> Response {
    respond((|| -> Result<Project> {
        let body: CreateProject = parse_body(request)?;
        if body.name == DEMO_PROJECT_NAME {
            return Err(Error::validation(
                "validation.reserved_project_name",
                "project.name",
                format!("'{DEMO_PROJECT_NAME}' is the bundled demonstration project. Choose another name."),
            ));
        }
        if state.workspace.project_exists(&body.name)? {
            return Err(Error::validation(
                "validation.project_exists",
                "project.name",
                format!(
                    "a project named '{}' already exists in this workspace",
                    body.name
                ),
            ));
        }
        let now = clock::now_utc();
        let project = Project::new(
            token::opaque_id(),
            &body.name,
            Site {
                timezone: body.timezone,
                currency: "USD".to_string(),
            },
            now,
        )?;
        state.workspace.save_project(&project)?;
        Ok(project)
    })())
}

/// `GET /api/projects/{name}`
pub fn open_project(state: &AppState, name: &str) -> Response {
    respond(state.workspace.load_project(name))
}

/// `PUT /api/projects/{name}`
pub fn save_project(state: &AppState, name: &str, request: &Request) -> Response {
    respond((|| -> Result<Project> {
        state.workspace.load_project(name)?;
        let mut project: Project = parse_body(request)?;
        if project.name != name {
            return Err(Error::validation(
                "validation.project_name_mismatch",
                "project.name",
                "the project in the body does not have the name in the URL. Tonans will not rename a \
                 project as a side effect of saving it.",
            ));
        }
        project.modified_utc = clock::now_utc();
        project.app_version = APP_VERSION.to_string();
        project.model_version = MODEL_VERSION.to_string();
        project.validate()?;
        state.workspace.save_project(&project)?;
        Ok(project)
    })())
}

// ---------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------

/// What an upload yields: the file's real headers, so the user maps them by hand.
#[derive(Debug, Serialize)]
pub struct UploadReceipt {
    /// Handle the import step refers back to.
    pub upload_id: String,
    /// The file's sanitized name.
    pub filename: String,
    /// The header row and a few sample rows, uninterpreted.
    #[serde(flatten)]
    pub preview: CsvPreview,
}

/// `POST /api/upload` — raw CSV bytes, filename in a header.
///
/// The bytes are staged in memory and never written to disk. The user then maps the
/// columns, and only then does anything get interpreted.
pub fn upload(state: &AppState, request: &Request) -> Response {
    respond((|| -> Result<UploadReceipt> {
        let supplied = request.header("x-tonans-filename").unwrap_or("upload.csv");
        // The header is attacker-reachable, so the name is validated here, before the
        // body is looked at. The UI sends the File API's `file.name`, which is a bare
        // name; a name carrying a path is refused rather than trimmed down to
        // something we invented. See `tonans_core::paths::source_file_name`.
        let filename = source_file_name(supplied)?;

        let preview = csvimport::preview(&request.body)?;
        let upload_id = state.stage_upload(filename.clone(), request.body.clone())?;

        Ok(UploadReceipt {
            upload_id,
            filename,
            preview,
        })
    })())
}

/// Body of `POST /api/projects/{name}/import`.
#[derive(Debug, Deserialize)]
pub struct ImportBody {
    /// Which staged upload to read.
    pub upload_id: String,
    /// How to read it. Every column mapped or explicitly ignored; nothing sniffed.
    pub request: ImportRequest,
}

/// `POST /api/projects/{name}/import`
pub fn import(state: &AppState, name: &str, request: &Request) -> Response {
    respond((|| -> Result<serde_json::Value> {
        let body: ImportBody = parse_body(request)?;
        let mut project = state.workspace.load_project(name)?;
        let staged = state.take_upload(&body.upload_id)?;

        // The name the file arrived under wins over anything the body claims, so a
        // client cannot launder a hostile name through the import request.
        let mut import_request = body.request;
        import_request.filename = staged.filename;

        let now = clock::now_utc();
        let report = csvimport::import(&staged.bytes, &import_request, now)?;

        project.dataset = Some(report.dataset.clone());
        project.site.timezone = import_request.timezone.clone();
        project.modified_utc = now;
        project.validate()?;
        state.workspace.save_project(&project)?;

        Ok(serde_json::json!({
            "project": project,
            "import": {
                "row_count": report.row_count,
                "interval_count": report.interval_count,
                "synthesized_interval_count": report.synthesized_interval_count,
                "warnings": report.warnings,
            }
        }))
    })())
}

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

/// One series, decimated for a chart.
#[derive(Debug, Serialize)]
pub struct UiSeries {
    /// Lowest value in each bucket.
    pub min: Vec<f64>,
    /// Highest value in each bucket.
    pub max: Vec<f64>,
    /// Mean of each bucket.
    pub mean: Vec<f64>,
}

/// A scenario's timeline, decimated for the browser.
///
/// The browser gets buckets, not 35,040 points per series. Each bucket keeps its own
/// minimum and maximum, so a peak is never smoothed away — and the UI says, on the
/// chart, that it is looking at buckets and where the full data is.
#[derive(Debug, Serialize)]
pub struct UiTimeline {
    /// Which scenario.
    pub scenario_id: String,
    /// How many intervals it was built from.
    pub source_interval_count: usize,
    /// Whether any decimation happened at all.
    pub decimated: bool,
    /// Start of each bucket.
    pub bucket_start_utc: Vec<String>,
    /// The series, keyed by their unit-bearing name.
    pub series: BTreeMap<String, UiSeries>,
    /// Whether each bucket contains any outage interval.
    pub outage: Vec<bool>,
}

/// The body of `POST /api/projects/{name}/run`.
#[derive(Debug, Serialize)]
pub struct RunResponse {
    /// The full result.
    pub run: RunResult,
    /// One decimated timeline per scenario.
    pub timelines: Vec<UiTimeline>,
    /// The key an export bundle for this run would be filed under.
    pub bundle_key: String,
    /// A digest of the results, so two runs can be compared for identity.
    pub results_hash: Hash,
}

/// `POST /api/projects/{name}/run`
pub fn run(state: &AppState, name: &str) -> Response {
    respond((|| -> Result<RunResponse> {
        let project = state.workspace.load_project(name)?;
        let result = run::run_project(&project, clock::now_utc())?;

        let hours = project
            .dataset
            .as_ref()
            .map_or(1.0, |d| f64::from(d.interval_minutes) / 60.0);
        let timelines = result
            .scenarios
            .iter()
            .map(|scenario| timeline_for(&scenario.scenario_id, &scenario.timeline, hours))
            .collect();

        Ok(RunResponse {
            bundle_key: export::bundle_key(&project, &result, &[])?,
            results_hash: result.results_hash()?,
            timelines,
            run: result,
        })
    })())
}

fn timeline_for(scenario_id: &str, rows: &[IntervalRow], hours: f64) -> UiTimeline {
    let buckets = limits::CHART_MAX_POINTS;
    let mut series = BTreeMap::new();

    // The engine works in energy per interval; a chart is read in power. Convert once.
    let mut add = |name: &str, values: Vec<f64>| {
        let decimated = svg::decimate(&values, buckets);
        series.insert(
            name.to_string(),
            UiSeries {
                min: decimated.iter().map(|b| b.min).collect(),
                max: decimated.iter().map(|b| b.max).collect(),
                mean: decimated.iter().map(|b| b.mean).collect(),
            },
        );
    };
    add("load_kw", rows.iter().map(|r| r.load_kwh / hours).collect());
    add(
        "critical_load_kw",
        rows.iter().map(|r| r.critical_load_kwh / hours).collect(),
    );
    add(
        "generation_kw",
        rows.iter()
            .map(|r| r.generation_available_kwh / hours)
            .collect(),
    );
    add(
        "grid_import_kw",
        rows.iter().map(|r| r.grid_import_kwh / hours).collect(),
    );
    add(
        "grid_export_kw",
        rows.iter().map(|r| r.grid_export_kwh / hours).collect(),
    );
    add(
        "battery_charge_kw",
        rows.iter().map(|r| r.battery_charge_kwh / hours).collect(),
    );
    add(
        "battery_discharge_kw",
        rows.iter()
            .map(|r| r.battery_discharge_kwh / hours)
            .collect(),
    );
    add(
        "unmet_critical_kw",
        rows.iter().map(|r| r.unmet_critical_kwh / hours).collect(),
    );
    add("soc_pct", rows.iter().map(|r| r.soc_pct).collect());

    // Bucket boundaries, computed exactly as `decimate` computes them, so the x-axis
    // lines up with the values rather than merely looking as if it does.
    let count = series.values().next().map_or(0, |s| s.mean.len());
    let bucket_start_utc = (0..count)
        .map(|index| {
            let start = index * rows.len() / count.max(1);
            rows.get(start)
                .map(|r| r.start_utc.to_rfc3339())
                .unwrap_or_default()
        })
        .collect();
    let outage = (0..count)
        .map(|index| {
            let start = index * rows.len() / count.max(1);
            let end = ((index + 1) * rows.len() / count.max(1))
                .max(start + 1)
                .min(rows.len());
            rows[start..end].iter().any(|r| r.outage)
        })
        .collect();

    UiTimeline {
        scenario_id: scenario_id.to_string(),
        source_interval_count: rows.len(),
        decimated: svg::is_decimated(rows.len()),
        bucket_start_utc,
        series,
        outage,
    }
}

// ---------------------------------------------------------------------------
// Sensitivity
// ---------------------------------------------------------------------------

/// `POST /api/projects/{name}/sensitivity`
pub fn sensitivity(state: &AppState, name: &str, request: &Request) -> Response {
    respond((|| -> Result<SensitivityResult> {
        let body: SensitivityRequest = parse_body(request)?;
        let project = state.workspace.load_project(name)?;
        sensitivity::run(&project, &body)
    })())
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

/// Body of `POST /api/projects/{name}/export`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportBody {
    /// At most one sensitivity request to recompute and include. The browser may
    /// describe a study; it may never submit finished evidence for Tonans to trust.
    #[serde(default)]
    pub sensitivity: Option<SensitivityRequest>,
}

/// `POST /api/projects/{name}/export`
pub fn export(state: &AppState, name: &str, request: &Request) -> Response {
    respond((|| -> Result<ExportBundle> {
        let body: ExportBody = if request.body.is_empty() {
            ExportBody::default()
        } else {
            parse_body(request)?
        };

        let project = state.workspace.load_project(name)?;
        let result = run::run_project(&project, clock::now_utc())?;
        let sensitivities = match body.sensitivity {
            Some(request) => vec![sensitivity::run(&project, &request)?],
            None => Vec::new(),
        };

        let key = export::bundle_key(&project, &result, &sensitivities)?;
        let directory = state.workspace.export_dir(&project.name, &key)?;
        export::write_bundle(&directory, &project, &result, &sensitivities)
    })())
}

/// `GET /api/projects/{name}/exports/{key}/{artifact}`
///
/// Every path component is validated. This cannot be walked out of the workspace,
/// even if all three are attacker-controlled.
pub fn download(state: &AppState, name: &str, key: &str, artifact: &str) -> Response {
    match state.workspace.read_export(name, key, artifact) {
        Ok(bytes) => {
            let content_type = match artifact.rsplit('.').next() {
                Some("html") => "text/html; charset=utf-8",
                Some("json") => "application/json",
                Some("csv") => "text/csv; charset=utf-8",
                _ => "text/plain; charset=utf-8",
            };
            // Never rendered inline: an exported report is a file to save, and
            // `attachment` keeps the browser from executing anything in it against this
            // origin.
            Response::new(200, content_type, bytes).with_header("content-disposition", "attachment")
        }
        Err(error) => problem(error.http_status(), error.code(), &error.to_string()),
    }
}

/// `POST /api/shutdown`
pub fn shutdown(state: &AppState) -> Response {
    state.stop.store(true, Ordering::SeqCst);
    Response::json(200, r#"{"stopping":true}"#)
}
