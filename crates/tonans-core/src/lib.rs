//! # Tonans core
//!
//! The deterministic model behind Tonans: the project schema, the strict interval
//! importer, the single-bus energy engine, tariff and economic accounting,
//! sensitivity studies, and the provenance-bearing exports.
//!
//! ## Two properties this crate guarantees
//!
//! **It is deterministic.** There is no clock and no randomness anywhere in it.
//! Wall-clock reads are banned workspace-wide by `clippy.toml`; every timestamp
//! and identifier is *injected* by the caller. Transcendental functions are
//! avoided in favour of iterated multiplication, because `powf` is a libm call and
//! libm is not required to be bit-identical across platforms. The consequence is
//! that the same project produces byte-identical results, on any machine, forever
//! — and [`run::RunResult::model_input_hash`] lets a reader prove it.
//!
//! **It refuses rather than repairs.** A duplicate timestamp, an ambiguous local
//! time, a `NaN`, a critical load above total load: each is an error naming the
//! row and column, never a value quietly substituted. The single exception is a
//! missing interval, and only when the user has *deliberately* configured a gap
//! policy — in which case the synthesized intervals are counted, their indices
//! recorded, and a critical warning rides on the data for the rest of its life.
//!
//! ## What it is not
//!
//! Not an AC power-flow solver. Not a protection or interconnection study. Not an
//! optimiser — the dispatch policies are named rules with no view of the future,
//! and Tonans never claims an optimum it did not compute. Not a battery lifetime
//! model — the fade is a transparent linear approximation and says so on every
//! artifact it touches. See [`version::MODEL_EXCLUSIONS`].
//!
//! ## The shape of a session
//!
//! ```
//! use std::collections::BTreeMap;
//! use tonans_core::csvimport::{ColumnMapping, ImportRequest, ImportUnit, MappedColumn};
//! use tonans_core::schema::{GapPolicy, Project, SeriesKind, Site};
//! use tonans_core::run;
//!
//! # fn main() -> tonans_core::Result<()> {
//! // The clock is the caller's. This crate never reads one.
//! let now = chrono::DateTime::from_timestamp(1_767_225_600, 0).expect("a valid instant");
//!
//! let site = Site { timezone: "America/New_York".into(), currency: "USD".into() };
//! let mut project = Project::new("demo", "Warehouse 3", site, now)?;
//!
//! // Every column is mapped explicitly, with its unit. Nothing is sniffed.
//! let mut series = BTreeMap::new();
//! series.insert(
//!     SeriesKind::Load,
//!     MappedColumn { header: "load_kw".into(), unit: ImportUnit::Kilowatt },
//! );
//! let request = ImportRequest {
//!     filename: "meter.csv".into(),
//!     timezone: "America/New_York".into(),
//!     interval_minutes: 60,
//!     gap_policy: GapPolicy::Reject,
//!     mapping: ColumnMapping {
//!         timestamp: "timestamp".into(),
//!         series,
//!         demand_period: None,
//!         ignore_columns: Vec::new(),
//!     },
//! };
//!
//! let csv = b"timestamp,load_kw\n2024-01-01T00:00:00-05:00,40\n2024-01-01T01:00:00-05:00,55\n";
//! project.dataset = Some(tonans_core::csvimport::import(csv, &request, now)?.dataset);
//!
//! let result = run::run_project(&project, now)?;
//! let baseline = result.baseline().expect("a new project always has a baseline");
//! assert_eq!(baseline.energy.grid_import_kwh, 95.0); // 40 kWh + 55 kWh, no battery
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]
#![warn(clippy::doc_markdown)]

pub mod canonical;
pub mod csvimport;
pub mod economics;
pub mod engine;
pub mod error;
pub mod export;
pub mod limits;
pub mod paths;
pub mod profile;
pub mod provenance;
pub mod report;
pub mod run;
pub mod schema;
pub mod sensitivity;
pub mod store;
pub mod svg;
pub mod timegrid;
pub mod units;
pub mod version;

pub use error::{Error, Result, Severity, Warning};
pub use version::{APP_VERSION, MODEL_VERSION, PLANNING_DISCLAIMER, SCHEMA_VERSION};
