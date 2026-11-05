//! The strict interval-CSV importer.
//!
//! Two rules govern everything here.
//!
//! **Nothing is guessed.** The user states the timezone, the interval, and the
//! unit of every column. The importer does not sniff a delimiter, infer an
//! interval from the first two rows, assume kW because the header says "load", or
//! decide what an empty cell meant. Where a file is ambiguous, the import fails
//! and says exactly which row and column made it ambiguous.
//!
//! **Nothing is repaired.** A duplicate timestamp, a backwards timestamp, a NaN,
//! a negative load, a critical load above total load — each is a hard error.
//! Missing intervals are the single exception, and only because meter exports
//! genuinely do drop them: the user may *deliberately* configure a
//! [`GapPolicy`], the synthesized intervals are counted and their indices
//! recorded, a `critical` warning rides on the data set for the rest of its life,
//! and the whole thing is capped at [`limits::MAX_SYNTHESIZED_FRACTION`] of the
//! file. A filled interval is never silent.
//!
//! The column *mapping* is explicit for the same reason. Every source header must
//! be either mapped to a role or named in `ignore_columns`. A stray column is an
//! error, not a shrug — because the one time it matters, that column was the real
//! load and the one you mapped was a forecast.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, NaiveDateTime, TimeDelta, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use crate::canonical::hash_bytes;
use crate::error::{Error, Result, Warning};
use crate::limits;
use crate::paths::source_file_name;
use crate::schema::{Dataset, DemandPeriods, GapPolicy, Series, SeriesKind, SourceFile};
use crate::timegrid::{parse_timezone, resolve_local};

/// The unit a source column is stated to be in.
///
/// Conversion to the model's canonical unit happens once, at import, and is
/// recorded in the provenance manifest. The engine only ever sees kW, USD/kWh,
/// and °C.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportUnit {
    /// Mean power over the interval, kilowatts.
    Kilowatt,
    /// Mean power over the interval, watts.
    Watt,
    /// Mean power over the interval, megawatts.
    Megawatt,
    /// Energy *in* the interval, kilowatt-hours. Divided by the interval duration.
    KilowattHourPerInterval,
    /// Energy *in* the interval, watt-hours.
    WattHourPerInterval,
    /// Price, currency per kilowatt-hour.
    UsdPerKilowattHour,
    /// Price, cents per kilowatt-hour.
    CentsPerKilowattHour,
    /// Price, currency per megawatt-hour.
    UsdPerMegawattHour,
    /// Temperature, degrees Celsius.
    Celsius,
    /// Temperature, degrees Fahrenheit.
    Fahrenheit,
}

impl ImportUnit {
    /// Convert one reading to the canonical unit for its role.
    ///
    /// `interval_hours` is needed only by the energy-per-interval units, and that
    /// is exactly the conversion an importer most often gets wrong: 15-minute kWh
    /// data read as kW overstates every power figure by a factor of four.
    #[must_use]
    pub fn to_canonical(self, value: f64, interval_hours: f64) -> f64 {
        match self {
            ImportUnit::Kilowatt | ImportUnit::UsdPerKilowattHour | ImportUnit::Celsius => value,
            ImportUnit::Watt => value / 1_000.0,
            ImportUnit::Megawatt => value * 1_000.0,
            ImportUnit::KilowattHourPerInterval => value / interval_hours,
            ImportUnit::WattHourPerInterval => value / 1_000.0 / interval_hours,
            ImportUnit::CentsPerKilowattHour => value / 100.0,
            ImportUnit::UsdPerMegawattHour => value / 1_000.0,
            ImportUnit::Fahrenheit => (value - 32.0) * 5.0 / 9.0,
        }
    }

    /// Whether this unit can express the given role at all.
    #[must_use]
    pub const fn fits(self, kind: SeriesKind) -> bool {
        match kind {
            SeriesKind::Load | SeriesKind::CriticalLoad | SeriesKind::Generation => matches!(
                self,
                ImportUnit::Kilowatt
                    | ImportUnit::Watt
                    | ImportUnit::Megawatt
                    | ImportUnit::KilowattHourPerInterval
                    | ImportUnit::WattHourPerInterval
            ),
            SeriesKind::ImportPrice | SeriesKind::ExportPrice => matches!(
                self,
                ImportUnit::UsdPerKilowattHour
                    | ImportUnit::CentsPerKilowattHour
                    | ImportUnit::UsdPerMegawattHour
            ),
            SeriesKind::Temperature => matches!(self, ImportUnit::Celsius | ImportUnit::Fahrenheit),
        }
    }

    /// The unit's label, for the provenance manifest and the report.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            ImportUnit::Kilowatt => "kW",
            ImportUnit::Watt => "W",
            ImportUnit::Megawatt => "MW",
            ImportUnit::KilowattHourPerInterval => "kWh per interval",
            ImportUnit::WattHourPerInterval => "Wh per interval",
            ImportUnit::UsdPerKilowattHour => "USD/kWh",
            ImportUnit::CentsPerKilowattHour => "cents/kWh",
            ImportUnit::UsdPerMegawattHour => "USD/MWh",
            ImportUnit::Celsius => "°C",
            ImportUnit::Fahrenheit => "°F",
        }
    }
}

/// A source column bound to a model role, with its stated unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappedColumn {
    /// The source header, exactly as it appears in the file.
    pub header: String,
    /// The unit the user states this column is in.
    pub unit: ImportUnit,
}

/// Which source column means what.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnMapping {
    /// The source header holding timestamps.
    pub timestamp: String,
    /// Role bindings. `load` is required; everything else is optional.
    pub series: BTreeMap<SeriesKind, MappedColumn>,
    /// The source header holding demand-charge period labels, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub demand_period: Option<String>,
    /// Source headers the user has explicitly decided to ignore.
    ///
    /// A header that is neither mapped nor listed here fails the import. Silence
    /// is not consent.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore_columns: Vec<String>,
}

/// Everything the importer needs, stated by the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImportRequest {
    /// The file's name, for the audit trail. Sanitized; never used as a path.
    pub filename: String,
    /// IANA timezone the site is in, and that naive timestamps are read in.
    pub timezone: String,
    /// The declared interval duration. Every consecutive timestamp must be
    /// exactly this far apart.
    pub interval_minutes: u32,
    /// What to do about a missing interval. Defaults to rejecting the file.
    #[serde(default)]
    pub gap_policy: GapPolicy,
    /// Which column means what.
    pub mapping: ColumnMapping,
}

/// What the importer did, beyond the data itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportReport {
    /// The imported data.
    pub dataset: Dataset,
    /// Data rows read from the file.
    pub row_count: usize,
    /// Intervals in the resulting data set, synthesized ones included.
    pub interval_count: usize,
    /// Intervals the gap policy synthesized.
    pub synthesized_interval_count: usize,
    /// Everything the reader needs to know about this data.
    pub warnings: Vec<Warning>,
}

/// The header row and a few sample rows, for the mapping screen.
///
/// Produced before any interpretation, so the user maps columns against what the
/// file actually contains rather than against what Tonans guessed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CsvPreview {
    /// SHA-256 of the exact uploaded bytes.
    pub sha256: crate::canonical::Hash,
    /// Size of those bytes.
    pub size_bytes: u64,
    /// The header row.
    pub headers: Vec<String>,
    /// The first few data rows, verbatim.
    pub sample_rows: Vec<Vec<String>>,
    /// Data rows in the file.
    pub row_count: usize,
}

/// The name the blank interval-data template is offered under.
pub const TEMPLATE_FILE_NAME: &str = "tonans-interval-template.csv";

/// A blank interval-data template: the columns Tonans can read, and three rows showing
/// the format of each. The rows are examples of the format, not data about any site.
pub const TEMPLATE_CSV: &str = "\
timestamp,load_kw,critical_load_kw,pv_kw,import_price_per_kwh,export_price_per_kwh,demand_period,ambient_c
2025-01-01T00:00:00-05:00,50.0,15.0,0.0,0.10,0.04,off-peak,2.0
2025-01-01T01:00:00-05:00,48.5,15.0,0.0,0.10,0.04,off-peak,1.5
2025-01-01T02:00:00-05:00,47.0,15.0,0.0,0.10,0.04,off-peak,1.0
";

/// Longest accepted demand-period label.
const MAX_PERIOD_LABEL_CHARS: usize = 32;

/// Sample rows returned by [`preview`].
const PREVIEW_ROWS: usize = 5;

/// Read the header and a sample of rows, without interpreting anything.
pub fn preview(bytes: &[u8]) -> Result<CsvPreview> {
    let text = decode(bytes)?;
    let mut reader = reader_for(&text);

    let headers = read_headers(&mut reader)?;
    let mut sample_rows = Vec::new();
    let mut row_count = 0usize;

    for record in reader.records() {
        let record = record.map_err(|e| csv_error(&e))?;
        row_count += 1;
        if row_count > limits::MAX_CSV_ROWS {
            return Err(Error::limit(
                "limit.csv_rows",
                limits::MAX_CSV_ROWS as u64,
                row_count as u64,
                format!("the file has more than {} data rows", limits::MAX_CSV_ROWS),
            ));
        }
        validate_record_fields(&record, &headers, row_count as u64 + 1)?;
        if sample_rows.len() < PREVIEW_ROWS {
            sample_rows.push(record.iter().map(str::to_owned).collect());
        }
    }

    Ok(CsvPreview {
        sha256: hash_bytes(bytes),
        size_bytes: bytes.len() as u64,
        headers,
        sample_rows,
        row_count,
    })
}

/// Import interval data.
///
/// `now_utc` is injected rather than read: this crate has no clock. It is
/// recorded as the import time in the provenance trail.
pub fn import(
    bytes: &[u8],
    request: &ImportRequest,
    now_utc: DateTime<Utc>,
) -> Result<ImportReport> {
    let filename = source_file_name(&request.filename)?;
    let timezone = parse_timezone(&request.timezone)?;
    let interval_minutes = request.interval_minutes;
    if !(limits::MIN_INTERVAL_MINUTES..=limits::MAX_INTERVAL_MINUTES).contains(&interval_minutes)
        || 60 % interval_minutes != 0
    {
        return Err(Error::validation(
            "validation.interval_minutes",
            "import.interval_minutes",
            format!(
                "interval must divide an hour and lie between {min} and {max} minutes, got {interval_minutes}",
                min = limits::MIN_INTERVAL_MINUTES,
                max = limits::MAX_INTERVAL_MINUTES
            ),
        ));
    }
    // A gap policy that may fill more intervals than a data set can even hold is not
    // a gap policy. `max_consecutive_intervals` arrives as a plain number in a JSON
    // body, and the synthesized-fraction ceiling below cannot help: it only runs once
    // the whole file is in memory, which is exactly the memory being asked for. The
    // running check in `Accumulator::synthesize` is what actually bounds the
    // allocation; this one is here so the user is told which knob was wrong.
    let allowed_gap = request.gap_policy.max_consecutive();
    if allowed_gap > limits::MAX_INTERVALS {
        return Err(Error::limit(
            "limit.gap_policy",
            limits::MAX_INTERVALS as u64,
            allowed_gap as u64,
            format!(
                "the gap policy would fill up to {allowed_gap} consecutive intervals, but a data \
                 set holds at most {}.",
                limits::MAX_INTERVALS
            ),
        ));
    }

    let interval_hours = f64::from(interval_minutes) / 60.0;
    let step = TimeDelta::minutes(i64::from(interval_minutes));

    let text = decode(bytes)?;
    let mut reader = reader_for(&text);
    let headers = read_headers(&mut reader)?;

    let plan = ColumnPlan::resolve(&headers, &request.mapping)?;
    let mut accumulator = Accumulator::new(&plan, interval_hours, request.gap_policy);
    let mut timestamps = TimestampTracker::new(timezone, step, request.gap_policy);

    let mut row_count = 0usize;
    for record in reader.records() {
        let record = record.map_err(|e| csv_error(&e))?;
        row_count += 1;
        // The header is line 1; the first data row is line 2.
        let line = row_count as u64 + 1;

        if row_count > limits::MAX_CSV_ROWS {
            return Err(Error::limit(
                "limit.csv_rows",
                limits::MAX_CSV_ROWS as u64,
                row_count as u64,
                format!("the file has more than {} data rows", limits::MAX_CSV_ROWS),
            ));
        }
        if record.len() != headers.len() {
            return Err(Error::import(
                "import.row_length",
                Some(line),
                None,
                format!(
                    "this row has {} fields but the header declares {}",
                    record.len(),
                    headers.len()
                ),
            ));
        }
        validate_record_fields(&record, &headers, line)?;

        let raw_timestamp = field(&record, plan.timestamp, line, &plan.timestamp_header)?;
        let instant = timestamps.parse(raw_timestamp, line)?;
        let missing = timestamps.advance(instant, line)?;
        accumulator.synthesize(missing)?;
        accumulator.push_row(&record, line, &plan)?;
    }

    if row_count == 0 {
        return Err(Error::import(
            "import.no_data_rows",
            None,
            None,
            "the file has a header but no data rows",
        ));
    }

    let interval_count = accumulator.interval_count();
    let synthesized = accumulator.synthesized.len();
    let synthesized_fraction = synthesized as f64 / interval_count as f64;
    if synthesized_fraction > limits::MAX_SYNTHESIZED_FRACTION {
        return Err(Error::limit(
            "limit.synthesized_fraction",
            (limits::MAX_SYNTHESIZED_FRACTION * 100.0) as u64,
            (synthesized_fraction * 100.0).ceil() as u64,
            format!(
                "the gap policy would synthesize {synthesized} of {interval_count} intervals \
                 ({pct:.1}%), above the {limit:.0}% ceiling. Fix the export at its source.",
                pct = synthesized_fraction * 100.0,
                limit = limits::MAX_SYNTHESIZED_FRACTION * 100.0
            ),
        ));
    }

    let start_utc = timestamps
        .first
        .expect("row_count > 0 implies a first timestamp");
    let mut warnings = accumulator.warnings.clone();

    if synthesized > 0 {
        warnings.push(Warning::critical(
            "data.synthesized_intervals",
            format!(
                "{synthesized} of {interval_count} intervals were not present in the source file and \
                 were synthesized by the '{policy}' gap policy you selected. Those intervals are not \
                 measurements. Every result below depends on them.",
                policy = request.gap_policy.describe()
            ),
        ));
    }
    if !plan.ignored.is_empty() {
        warnings.push(Warning::info(
            "data.ignored_columns",
            format!(
                "these columns were present in the file and were not imported, at your request: {}",
                plan.ignored.join(", ")
            ),
        ));
    }
    if accumulator.saw_negative_import_price {
        warnings.push(Warning::caution(
            "data.negative_import_price",
            "the import price is negative in at least one interval, so importing energy can earn \
             money and a dispatch policy that avoids import is not necessarily the cheaper one.",
        ));
    }

    let mut series = BTreeMap::new();
    for (kind, values) in accumulator.series {
        series.insert(
            kind,
            Series {
                unit: kind.unit(),
                values,
            },
        );
    }

    let demand_periods = accumulator.periods.map(|periods| DemandPeriods {
        labels: periods.labels,
        index: periods.index,
    });

    let mut column_mapping = BTreeMap::new();
    column_mapping.insert("timestamp".to_string(), plan.timestamp_header.clone());
    for (kind, column) in &request.mapping.series {
        column_mapping.insert(
            kind.as_str().to_string(),
            format!("{} [{}]", column.header, column.unit.label()),
        );
    }
    if let Some(header) = &request.mapping.demand_period {
        column_mapping.insert("demand_period".to_string(), header.clone());
    }

    let dataset = Dataset {
        source: SourceFile {
            filename,
            sha256: hash_bytes(bytes),
            size_bytes: bytes.len() as u64,
            row_count,
            imported_utc: now_utc,
            column_mapping,
            ignored_columns: plan.ignored.clone(),
        },
        interval_minutes,
        start_utc,
        interval_count,
        series,
        demand_periods,
        gap_policy: request.gap_policy,
        synthesized_interval_count: synthesized,
        synthesized_interval_indices: accumulator
            .synthesized
            .iter()
            .copied()
            .take(limits::MAX_RECORDED_SYNTHESIZED_INDICES)
            .collect(),
        import_warnings: warnings.clone(),
    };
    dataset.validate()?;

    Ok(ImportReport {
        dataset,
        row_count,
        interval_count,
        synthesized_interval_count: synthesized,
        warnings,
    })
}

// ---------------------------------------------------------------------------
// Decoding and framing
// ---------------------------------------------------------------------------

fn decode(bytes: &[u8]) -> Result<String> {
    if bytes.len() > limits::MAX_CSV_BYTES {
        return Err(Error::limit(
            "limit.csv_bytes",
            limits::MAX_CSV_BYTES as u64,
            bytes.len() as u64,
            format!(
                "the file is {} bytes; the limit is {} bytes",
                bytes.len(),
                limits::MAX_CSV_BYTES
            ),
        ));
    }
    if bytes.is_empty() {
        return Err(Error::import(
            "import.empty_file",
            None,
            None,
            "the file is empty",
        ));
    }
    // Excel writes a UTF-8 BOM. It is not part of the first header.
    let body = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    std::str::from_utf8(body).map(str::to_owned).map_err(|e| {
        Error::import(
            "import.not_utf8",
            None,
            None,
            format!(
                "the file is not valid UTF-8 (byte {}). Re-export it as UTF-8.",
                e.valid_up_to()
            ),
        )
    })
}

fn reader_for(text: &str) -> csv::Reader<&[u8]> {
    csv::ReaderBuilder::new()
        .has_headers(true)
        // A row whose field count differs from the header's is malformed, and we
        // want the error rather than a silently padded record.
        .flexible(false)
        .trim(csv::Trim::None)
        .from_reader(text.as_bytes())
}

fn read_headers(reader: &mut csv::Reader<&[u8]>) -> Result<Vec<String>> {
    let headers = reader.headers().map_err(|e| csv_error(&e))?;
    if headers.is_empty() {
        return Err(Error::import(
            "import.empty_file",
            None,
            None,
            "the file has no header row",
        ));
    }
    if headers.len() > limits::MAX_CSV_COLUMNS {
        return Err(Error::limit(
            "limit.csv_columns",
            limits::MAX_CSV_COLUMNS as u64,
            headers.len() as u64,
            format!(
                "the header has {} columns; the limit is {}",
                headers.len(),
                limits::MAX_CSV_COLUMNS
            ),
        ));
    }

    let mut out: Vec<String> = Vec::with_capacity(headers.len());
    for (index, header) in headers.iter().enumerate() {
        if header.is_empty() {
            return Err(Error::import(
                "import.empty_header",
                Some(1),
                None,
                format!("column {} has an empty header", index + 1),
            ));
        }
        if header.len() > limits::MAX_CSV_FIELD_BYTES {
            return Err(Error::limit(
                "limit.csv_field",
                limits::MAX_CSV_FIELD_BYTES as u64,
                header.len() as u64,
                format!("the header of column {} is too long", index + 1),
            ));
        }
        if header.chars().any(char::is_control) {
            return Err(Error::import(
                "import.header_control_character",
                Some(1),
                None,
                format!(
                    "the header of column {} contains a control character",
                    index + 1
                ),
            ));
        }
        if out.iter().any(|existing| existing == header) {
            return Err(Error::import(
                "import.duplicate_header",
                Some(1),
                Some(header),
                format!("the header '{header}' appears more than once."),
            ));
        }
        out.push(header.to_string());
    }
    Ok(out)
}

fn validate_record_fields(record: &csv::StringRecord, headers: &[String], line: u64) -> Result<()> {
    for (index, value) in record.iter().enumerate() {
        if value.len() > limits::MAX_CSV_FIELD_BYTES {
            let header = headers.get(index).map_or("unknown column", String::as_str);
            return Err(Error::limit(
                "limit.csv_field",
                limits::MAX_CSV_FIELD_BYTES as u64,
                value.len() as u64,
                format!("the value in column '{header}' on line {line} is too long"),
            ));
        }
    }
    Ok(())
}

fn csv_error(error: &csv::Error) -> Error {
    let line = error.position().map(csv::Position::line);
    Error::import(
        "import.malformed_csv",
        line,
        None,
        format!("the file is not well-formed CSV: {error}"),
    )
}

fn field<'a>(
    record: &'a csv::StringRecord,
    index: usize,
    line: u64,
    header: &str,
) -> Result<&'a str> {
    record.get(index).ok_or_else(|| {
        Error::import(
            "import.missing_field",
            Some(line),
            Some(header),
            format!("this row has no value for column '{header}'"),
        )
    })
}

// ---------------------------------------------------------------------------
// Column plan
// ---------------------------------------------------------------------------

struct ColumnPlan {
    timestamp: usize,
    timestamp_header: String,
    series: Vec<(SeriesKind, usize, MappedColumn)>,
    demand_period: Option<(usize, String)>,
    ignored: Vec<String>,
}

impl ColumnPlan {
    fn resolve(headers: &[String], mapping: &ColumnMapping) -> Result<Self> {
        let find = |wanted: &str| -> Result<usize> {
            headers.iter().position(|h| h == wanted).ok_or_else(|| {
                Error::import(
                    "import.missing_column",
                    Some(1),
                    Some(wanted),
                    format!(
                        "the file has no column named '{wanted}'. Its columns are: {}.",
                        headers.join(", ")
                    ),
                )
            })
        };

        let timestamp = find(&mapping.timestamp)?;

        let mut series = Vec::new();
        for (kind, column) in &mapping.series {
            if !column.unit.fits(*kind) {
                return Err(Error::import(
                    "import.unit_mismatch",
                    Some(1),
                    Some(&column.header),
                    format!(
                        "column '{header}' is mapped to {role}, but you declared its unit as {unit}, \
                         which cannot express {role}. A {role} column must be in {expected}.",
                        header = column.header,
                        role = kind.as_str(),
                        unit = column.unit.label(),
                        expected = expected_units_for(*kind),
                    ),
                ));
            }
            series.push((*kind, find(&column.header)?, column.clone()));
        }
        if !mapping.series.contains_key(&SeriesKind::Load) {
            return Err(Error::import(
                "import.missing_load_mapping",
                Some(1),
                None,
                "no column is mapped to load. A load series is required.",
            ));
        }
        series.sort_by_key(|(kind, _, _)| *kind);

        let demand_period = match &mapping.demand_period {
            Some(header) => Some((find(header)?, header.clone())),
            None => None,
        };

        let mut owners = BTreeMap::<String, String>::new();
        let mut claim = |header: &str, role: &str| -> Result<()> {
            if let Some(previous) = owners.insert(header.to_string(), role.to_string()) {
                return Err(Error::import(
                    "import.column_mapped_twice",
                    Some(1),
                    Some(header),
                    format!(
                        "column '{header}' is mapped to both {previous} and {role}. Each source \
                         column may have exactly one meaning."
                    ),
                ));
            }
            Ok(())
        };
        claim(&mapping.timestamp, "timestamp")?;
        for (kind, column) in &mapping.series {
            claim(&column.header, kind.as_str())?;
        }
        if let Some(header) = &mapping.demand_period {
            claim(header, "demand_period")?;
        }

        let ignored: BTreeSet<&str> = mapping.ignore_columns.iter().map(String::as_str).collect();
        for claimed in owners.keys() {
            if ignored.contains(claimed.as_str()) {
                return Err(Error::import(
                    "import.mapped_column_ignored",
                    Some(1),
                    Some(claimed),
                    format!("column '{claimed}' is both mapped and ignored"),
                ));
            }
        }

        // Every header must be accounted for: mapped, or explicitly ignored.
        let mut claimed: Vec<&str> = vec![mapping.timestamp.as_str()];
        claimed.extend(mapping.series.values().map(|c| c.header.as_str()));
        if let Some(header) = &mapping.demand_period {
            claimed.push(header.as_str());
        }

        let mut ignored = Vec::new();
        let mut unclaimed = Vec::new();
        for header in headers {
            if claimed.contains(&header.as_str()) {
                continue;
            }
            if mapping.ignore_columns.iter().any(|i| i == header) {
                ignored.push(header.clone());
            } else {
                unclaimed.push(header.clone());
            }
        }
        if !unclaimed.is_empty() {
            return Err(Error::import(
                "import.unmapped_column",
                Some(1),
                Some(&unclaimed[0]),
                format!(
                    "these columns are neither mapped to a role nor listed as ignored: {}. Map each \
                     one, or list it under ignore_columns.",
                    unclaimed.join(", ")
                ),
            ));
        }

        Ok(Self {
            timestamp,
            timestamp_header: mapping.timestamp.clone(),
            series,
            demand_period,
            ignored,
        })
    }
}

fn expected_units_for(kind: SeriesKind) -> &'static str {
    match kind {
        SeriesKind::Load | SeriesKind::CriticalLoad | SeriesKind::Generation => {
            "kW, W, MW, kWh per interval, or Wh per interval"
        }
        SeriesKind::ImportPrice | SeriesKind::ExportPrice => "USD/kWh, cents/kWh, or USD/MWh",
        SeriesKind::Temperature => "°C or °F",
    }
}

// ---------------------------------------------------------------------------
// Timestamps
// ---------------------------------------------------------------------------

/// A file must commit to one timestamp style. A file that mixes offset-bearing
/// and naive timestamps has been through two systems with different ideas about
/// time, and at least one of them was wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimestampStyle {
    WithOffset,
    NaiveLocal,
}

struct TimestampTracker {
    timezone: Tz,
    step: TimeDelta,
    gap_policy: GapPolicy,
    style: Option<TimestampStyle>,
    first: Option<DateTime<Utc>>,
    previous: Option<DateTime<Utc>>,
    expected: Option<DateTime<Utc>>,
}

impl TimestampTracker {
    fn new(timezone: Tz, step: TimeDelta, gap_policy: GapPolicy) -> Self {
        Self {
            timezone,
            step,
            gap_policy,
            style: None,
            first: None,
            previous: None,
            expected: None,
        }
    }

    fn parse(&mut self, raw: &str, line: u64) -> Result<DateTime<Utc>> {
        if raw.is_empty() {
            return Err(Error::import(
                "import.empty_timestamp",
                Some(line),
                Some("timestamp"),
                "the timestamp is empty",
            ));
        }
        if raw.len() > limits::MAX_CSV_FIELD_BYTES {
            return Err(Error::limit(
                "limit.csv_field",
                limits::MAX_CSV_FIELD_BYTES as u64,
                raw.len() as u64,
                format!("the timestamp on line {line} is too long"),
            ));
        }

        // An RFC 3339 timestamp carries its own offset and is never ambiguous.
        let (instant, style) = if let Ok(fixed) = DateTime::parse_from_rfc3339(raw) {
            (fixed.with_timezone(&Utc), TimestampStyle::WithOffset)
        } else {
            let naive = parse_naive(raw, line)?;
            (
                resolve_local(self.timezone, naive, Some(line))?,
                TimestampStyle::NaiveLocal,
            )
        };

        match self.style {
            None => self.style = Some(style),
            Some(established) if established != style => {
                return Err(Error::import(
                    "import.mixed_timestamp_format",
                    Some(line),
                    Some("timestamp"),
                    format!(
                        "this file mixes timestamp formats: earlier rows are {}, this one is {}. \
                         Re-export with one consistent format.",
                        match established {
                            TimestampStyle::WithOffset =>
                                "offset-bearing (e.g. 2024-01-01T00:00:00-05:00)",
                            TimestampStyle::NaiveLocal => "naive local (e.g. 2024-01-01T00:00:00)",
                        },
                        match style {
                            TimestampStyle::WithOffset => "offset-bearing",
                            TimestampStyle::NaiveLocal => "naive local",
                        }
                    ),
                ));
            }
            Some(_) => {}
        }
        Ok(instant)
    }

    /// Register `instant` and return how many intervals were missing before it.
    fn advance(&mut self, instant: DateTime<Utc>, line: u64) -> Result<usize> {
        let Some(expected) = self.expected else {
            self.first = Some(instant);
            self.previous = Some(instant);
            self.expected = Some(instant + self.step);
            return Ok(0);
        };
        let previous = self.previous.expect("expected implies previous");

        if instant == previous {
            return Err(Error::import(
                "import.duplicate_timestamp",
                Some(line),
                Some("timestamp"),
                format!("timestamp {} appears twice in a row.", instant.to_rfc3339()),
            ));
        }
        if instant < previous {
            return Err(Error::import(
                "import.non_monotonic_timestamp",
                Some(line),
                Some("timestamp"),
                format!(
                    "timestamp {} is earlier than the previous row's {}. Intervals must ascend.",
                    instant.to_rfc3339(),
                    previous.to_rfc3339()
                ),
            ));
        }

        if instant == expected {
            self.previous = Some(instant);
            self.expected = Some(instant + self.step);
            return Ok(0);
        }

        // Ahead of, behind, or off the grid.
        let delta = instant - previous;
        let step_seconds = self.step.num_seconds();
        let whole_steps = delta.num_seconds() / step_seconds;
        let aligned = i32::try_from(whole_steps)
            .ok()
            .is_some_and(|steps| previous + self.step * steps == instant);
        if !aligned {
            return Err(Error::import(
                "import.timestamp_not_aligned",
                Some(line),
                Some("timestamp"),
                format!(
                    "timestamp {} does not fall on a whole {}-minute interval after the previous \
                     row at {}. Either the declared interval is wrong, or the file is.",
                    instant.to_rfc3339(),
                    self.step.num_minutes(),
                    previous.to_rfc3339(),
                ),
            ));
        }

        let missing = usize::try_from(whole_steps - 1).unwrap_or(0);
        let allowed = self.gap_policy.max_consecutive();
        if matches!(self.gap_policy, GapPolicy::Reject) {
            return Err(Error::import(
                "import.gap",
                Some(line),
                Some("timestamp"),
                format!(
                    "{missing} interval(s) are missing between {} and {}. Fix the export, or \
                     choose a gap policy.",
                    previous.to_rfc3339(),
                    instant.to_rfc3339()
                ),
            ));
        }
        if missing > allowed {
            return Err(Error::import(
                "import.gap_too_long",
                Some(line),
                Some("timestamp"),
                format!(
                    "{missing} consecutive intervals are missing between {} and {}, above the \
                     {allowed} you allowed the gap policy to fill.",
                    previous.to_rfc3339(),
                    instant.to_rfc3339()
                ),
            ));
        }

        self.previous = Some(instant);
        self.expected = Some(instant + self.step);
        Ok(missing)
    }
}

/// The naive timestamp formats the importer accepts.
const NAIVE_FORMATS: &[&str] = &[
    "%Y-%m-%dT%H:%M:%S",
    "%Y-%m-%d %H:%M:%S",
    "%Y-%m-%dT%H:%M",
    "%Y-%m-%d %H:%M",
];

fn parse_naive(raw: &str, line: u64) -> Result<NaiveDateTime> {
    for format in NAIVE_FORMATS {
        if let Ok(naive) = NaiveDateTime::parse_from_str(raw, format) {
            return Ok(naive);
        }
    }
    Err(Error::import(
        "import.bad_timestamp",
        Some(line),
        Some("timestamp"),
        format!(
            "'{raw}' is not a readable timestamp. Use RFC 3339 with an offset \
             (2024-01-01T00:00:00-05:00), or one of: {}.",
            NAIVE_FORMATS.join(", ")
        ),
    ))
}

// ---------------------------------------------------------------------------
// Mapping suggestions
// ---------------------------------------------------------------------------

/// One column's suggested meaning.
///
/// A suggestion is something the user asked for and then reviews. The importer never
/// sees one: it still reads only the mapping the user confirms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ColumnSuggestion {
    /// The column, verbatim.
    pub header: String,
    /// `timestamp`, `demand_period`, or a series name such as `critical_load`.
    pub role: String,
    /// The unit, only when something in the file states it.
    pub unit: Option<ImportUnit>,
    /// What in the file led to this suggestion.
    pub reason: String,
    /// What the file could not settle, for the user to check.
    pub check: Option<String>,
}

/// What [`suggest_mapping`] found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MappingSuggestion {
    /// Columns with a suggested meaning, in file order.
    pub columns: Vec<ColumnSuggestion>,
    /// The spacing of the first two timestamps, if it is a legal interval.
    pub interval_minutes: Option<u32>,
    /// Columns left for the user, and why.
    pub notes: Vec<String>,
}

/// Suggest a mapping from a file's headers and first rows.
///
/// Called only when the user asks, and every suggestion carries its reason, because
/// the rule that matters is that Tonans never guesses *silently*.
///
/// **A column gets a role only when every word of its header is recognized, and they state both
/// what it means and its unit.** A word Tonans does not know means it cannot tell what the column
/// measures, so nothing is suggested and a note says why. That is what keeps `Non_Critical_Load`,
/// `Load_Setpoint_kW`, and `Solar_Irradiance_W/m2` from reading as what they partly spell, without
/// a list of every bad name, which could never be finished. A suggestion is still only a
/// suggestion: it reads a header, not the data, and always needs review.
///
/// One flagged exception on units: a price per kWh with no currency is read by size as dollars or
/// cents, and says that it was.
#[must_use]
pub fn suggest_mapping(preview: &CsvPreview) -> MappingSuggestion {
    let mut candidates: Vec<ColumnSuggestion> = Vec::new();
    let mut notes = Vec::new();

    for (index, header) in preview.headers.iter().enumerate() {
        let samples: Vec<&str> = preview
            .sample_rows
            .iter()
            .filter_map(|row| row.get(index))
            .map(|cell| cell.trim())
            .collect();
        match classify_column(header, &samples) {
            Ok(candidate) => candidates.push(candidate),
            Err(note) => notes.push(note),
        }
    }

    // Two columns claiming one role is a question only the user can answer.
    let mut columns = Vec::with_capacity(candidates.len());
    for candidate in &candidates {
        let rivals: Vec<&str> = candidates
            .iter()
            .filter(|other| other.role == candidate.role)
            .map(|other| other.header.trim())
            .collect();
        if rivals.len() == 1 {
            columns.push(candidate.clone());
        } else if rivals[0] == candidate.header.trim() {
            notes.push(format!(
                "{} could each be {}.",
                rivals
                    .iter()
                    .map(|header| format!("'{header}'"))
                    .collect::<Vec<_>>()
                    .join(" and "),
                role_phrase(&candidate.role)
            ));
        }
    }

    let interval_minutes = columns
        .iter()
        .find(|column| column.role == "timestamp")
        .and_then(|column| preview.headers.iter().position(|h| *h == column.header))
        .and_then(|index| first_interval_minutes(&preview.sample_rows, index));

    MappingSuggestion {
        columns,
        interval_minutes,
        notes,
    }
}

fn role_phrase(role: &str) -> &'static str {
    match role {
        "timestamp" => "the timestamp",
        "demand_period" => "the demand-charge period",
        "load" => "the load",
        "critical_load" => "the critical load",
        "generation" => "the generation",
        "import_price" => "the import price",
        "export_price" => "the export credit",
        _ => "the temperature",
    }
}

/// Unit spellings whose capitals set them apart inside a longer word, so that `kWhLoad`
/// is `kWh` and `Load` rather than `k`, `Wh` and `Load`, which would be 1000 times too
/// small. Longest first at any one position.
const CASED_UNITS: [&str; 18] = [
    "kVArh", "kVAh", "kVAr", "kVA", "KWH", "KWh", "kWh", "mWh", "MWH", "MWh", "GWH", "GWh", "KW",
    "kW", "mW", "MW", "GW", "Wh",
];

/// Whole words that are units however they are capitalized.
const UNIT_WORDS: [&str; 14] = [
    "kw", "kwh", "kwhr", "mw", "mwh", "mwhr", "gw", "gwh", "wh", "whr", "kva", "kvar", "kvah",
    "kvarh",
];

/// The first cased unit in `word`. What follows it must not be a lowercase letter, so
/// that `peakWindow` does not hold a `kW`.
fn next_cased_unit(word: &str) -> Option<(usize, &'static str)> {
    word.char_indices().find_map(|(start, _)| {
        CASED_UNITS
            .iter()
            .find(|unit| {
                word[start..]
                    .strip_prefix(**unit)
                    .is_some_and(|after| !after.chars().next().is_some_and(char::is_lowercase))
            })
            .map(|unit| (start, *unit))
    })
}

fn split_camel_case(raw: &str, tokens: &mut Vec<String>) {
    if raw.is_empty() {
        return;
    }
    // `FiT` is one word, the short form of a feed-in tariff.
    if UNIT_WORDS.contains(&raw.to_lowercase().as_str()) || raw.eq_ignore_ascii_case("fit") {
        tokens.push(raw.to_string());
        return;
    }
    let mut current = String::new();
    let mut previous_lower = false;
    for c in raw.chars() {
        if c.is_uppercase() && previous_lower {
            tokens.push(std::mem::take(&mut current));
        }
        previous_lower = c.is_lowercase();
        current.push(c);
    }
    tokens.push(current);
}

/// The header's words with their capitals, which is the only place `mW` and `MW` differ.
/// Currency symbols become words so that `$/kWh` reads as a currency.
fn header_tokens(header: &str) -> Vec<String> {
    let spaced = header
        .replace('$', " usd ")
        .replace('¢', " cents ")
        .replace('€', " eur ")
        .replace('£', " gbp ")
        .replace('¥', " jpy ")
        .replace('₹', " inr ")
        .replace('º', " ")
        .replace('℃', " C ")
        .replace('℉', " F ")
        .replace('㎾', " kW ");
    let mut tokens = Vec::new();
    for raw in spaced.split(|c: char| !c.is_alphanumeric()) {
        // A whole word that is a unit stays whole: `kWH` is not `k` and `WH`.
        if UNIT_WORDS.contains(&raw.to_lowercase().as_str()) {
            tokens.push(raw.to_string());
            continue;
        }
        let mut rest = raw;
        while let Some((start, unit)) = next_cased_unit(rest) {
            split_camel_case(&rest[..start], &mut tokens);
            tokens.push(unit.to_string());
            rest = &rest[start + unit.len()..];
        }
        split_camel_case(rest, &mut tokens);
    }
    tokens
}

/// The header's words, lowercased. `Load (kW)`, `load_kW`, `loadKw`, `kWLoad` and `LoadkW`
/// all give `load` and `kw`. Spelled-out energy units become one word: `kilowatt hours`,
/// `kW-hr`, `kW-h`, and `kWhr` are all `kwh`, while `kW/h` is not an amount of energy. So does a
/// feed-in: `Feed-in`, `Feed In`, and `FeedIn` are all `feedin`, and `in` means nothing else here.
fn header_words(header: &str, tokens: &[String]) -> Vec<String> {
    let per_hour = header.to_lowercase().contains("/h");
    let mut words: Vec<String> = Vec::with_capacity(tokens.len());
    for token in tokens {
        let lower = token.to_lowercase();
        if lower == "in" && words.last().is_some_and(|word| word == "feed") {
            words.pop();
            words.push("feedin".to_string());
            continue;
        }
        if lower == "feedintariff" {
            words.push("feedin".to_string());
            words.push("tariff".to_string());
            continue;
        }
        if matches!(lower.as_str(), "hr" | "hrs" | "hour" | "hours") || (lower == "h" && !per_hour) {
            let energy = match words.last().map(String::as_str) {
                Some("kw" | "kilowatt" | "kilowatts") => Some("kwh"),
                Some("w" | "watt" | "watts") => Some("wh"),
                Some("mw" | "megawatt" | "megawatts") => Some("mwh"),
                _ => None,
            };
            if let Some(energy) = energy {
                words.pop();
                words.push(energy.to_string());
                continue;
            }
        }
        words.push(
            match lower.as_str() {
                "kwhr" | "kwhrs" | "kwhs" | "kilowatthour" | "kilowatthours" => "kwh",
                "whr" | "whrs" | "watthour" | "watthours" => "wh",
                "mwhr" | "mwhrs" | "mwhs" | "megawatthour" | "megawatthours" => "mwh",
                other => other,
            }
            .to_string(),
        );
    }
    words
}

fn parses_as_timestamp(raw: &str) -> bool {
    DateTime::parse_from_rfc3339(raw).is_ok()
        || NAIVE_FORMATS
            .iter()
            .any(|format| NaiveDateTime::parse_from_str(raw, format).is_ok())
}

/// The minutes between the first two values of a timestamp column, if legal.
fn first_interval_minutes(rows: &[Vec<String>], index: usize) -> Option<u32> {
    let instant = |row: &Vec<String>| -> Option<NaiveDateTime> {
        let raw = row.get(index)?.trim();
        if let Ok(fixed) = DateTime::parse_from_rfc3339(raw) {
            return Some(fixed.naive_utc());
        }
        NAIVE_FORMATS
            .iter()
            .find_map(|format| NaiveDateTime::parse_from_str(raw, format).ok())
    };
    let first = instant(rows.first()?)?;
    let second = instant(rows.get(1)?)?;
    let minutes = u32::try_from((second - first).num_minutes()).ok()?;
    (limits::MIN_INTERVAL_MINUTES..=limits::MAX_INTERVAL_MINUTES)
        .contains(&minutes)
        .then_some(minutes)
}

/// Words that qualify a power or energy column without changing what it measures.
const COLUMN_QUALIFIERS: [&str; 15] = [
    "site",
    "building",
    "facility",
    "whole",
    "avg",
    "average",
    "mean",
    "power",
    "energy",
    "electric",
    "electrical",
    "electricity",
    "active",
    "metered",
    "measured",
];

/// Words that say a column is the site's load.
const LOAD_WORDS: [&str; 5] = ["load", "loads", "consumption", "usage", "demand"];

/// Words that name where a whole load is measured, and leave it the load. `HVAC` and `Lighting`
/// are not here: each is part of a load, and mapping one as the load would drop the rest.
const LOAD_QUALIFIERS: [&str; 9] = [
    "profile",
    "house",
    "home",
    "household",
    "bldg",
    "campus",
    "feeder",
    "bus",
    "transformer",
];

/// Words that narrow a load to the part that must stay on in an outage.
const CRITICAL_WORDS: [&str; 2] = ["critical", "essential"];

/// Words that say a column is on-site generation. `generator` is not one: it may be a machine
/// that runs only when called on, which is not output that simply arrives.
const GENERATION_WORDS: [&str; 9] = [
    "gen",
    "pv",
    "solar",
    "photovoltaic",
    "generation",
    "generated",
    "wind",
    "renewable",
    "renewables",
];

/// Words that qualify generation without changing what it measures.
const GENERATION_QUALIFIERS: [&str; 8] = [
    "output",
    "yield",
    "production",
    "produced",
    "array",
    "onsite",
    "on",
    "ac",
];

/// Units Tonans reads for a power or energy column.
const POWER_UNIT_WORDS: [&str; 11] = [
    "kw",
    "kilowatt",
    "kilowatts",
    "w",
    "watt",
    "watts",
    "mw",
    "megawatt",
    "megawatts",
    "kwh",
    "wh",
];

/// Units that are recognized, and that Tonans does not read for such a column.
const UNREAD_POWER_UNITS: [(&str, &str); 7] = [
    ("mwh", "MWh"),
    ("gw", "GW"),
    ("gwh", "GWh"),
    ("kva", "kVA"),
    ("kvah", "kVAh"),
    ("kvar", "kvar"),
    ("kvarh", "kvarh"),
];

const UNREAD_POWER_UNIT_WORDS: [&str; 7] = ["mwh", "gw", "gwh", "kva", "kvah", "kvar", "kvarh"];

/// Words that say a column is the temperature of the air at the site, and nothing else.
const TEMPERATURE_WORDS: [&str; 5] = ["temperature", "temp", "ambient", "oat", "drybulb"];

/// Everything else a temperature header may say, including its unit.
const TEMPERATURE_QUALIFIERS: [&str; 21] = [
    "outdoor",
    "outside",
    "oa",
    "amb",
    "air",
    "dry",
    "bulb",
    "site",
    "avg",
    "average",
    "mean",
    "deg",
    "degree",
    "degrees",
    "c",
    "celsius",
    "centigrade",
    "degc",
    "f",
    "fahrenheit",
    "degf",
];

/// The temperature words that state a unit.
const TEMPERATURE_UNIT_WORDS: [&str; 10] = [
    "c",
    "celsius",
    "centigrade",
    "degc",
    "f",
    "fahrenheit",
    "degf",
    "deg",
    "degree",
    "degrees",
];

/// Words that say a column labels the demand-charge period.
const PERIOD_WORDS: [&str; 3] = ["period", "tou", "season"];

/// Everything else a period header may say.
const PERIOD_QUALIFIERS: [&str; 8] = [
    "demand", "charge", "time", "of", "use", "rate", "label", "name",
];

/// Words that name a price. `cost` and `credit` also name amounts of money, so those two need an
/// explicit `per` or `/` as well.
const PRICE_WORDS: [&str; 8] = [
    "price", "prices", "rate", "tariff", "cost", "credit", "lmp", "lbmp",
];

/// Words that say a price is for energy bought.
const IMPORT_WORDS: [&str; 6] = [
    "import",
    "imported",
    "buy",
    "purchase",
    "purchased",
    "retail",
];

/// Words that say a price is for energy sold.
const EXPORT_WORDS: [&str; 8] = [
    "export", "exported", "sell", "sold", "feed", "feedin", "fit", "credit",
];

/// Everything else a price header may say, including the money and the energy.
const PRICE_QUALIFIERS: [&str; 20] = [
    "spot",
    "wholesale",
    "market",
    "tou",
    "period",
    "usage",
    "metering",
    "us",
    "energy",
    "electricity",
    "electric",
    "per",
    "usd",
    "dollar",
    "dollars",
    "cents",
    "kwh",
    "mwh",
    "rt",
    "da",
];

/// Spellings that mean a column is money even with no price word beside them.
const MONEY_MARKS: [&str; 3] = ["usd", "cents", "dollars"];

/// Currencies other than the US dollar that Tonans recognizes, so that it can name them. A
/// spelling that is not here is still refused, as a word it does not recognize. The two-letter
/// and everyday words count only in a header that is already about money.
const OTHER_CURRENCIES: [(&str, &str); 55] = [
    ("eur", "EUR"),
    ("euro", "EUR"),
    ("euros", "EUR"),
    ("eurocent", "euro cents"),
    ("eurocents", "euro cents"),
    ("gbp", "GBP"),
    ("pound", "pounds"),
    ("pounds", "pounds"),
    ("sterling", "pounds sterling"),
    ("pence", "pence"),
    ("p", "pence ('p')"),
    ("cad", "CAD"),
    ("aud", "AUD"),
    ("nzd", "NZD"),
    ("jpy", "JPY"),
    ("yen", "JPY"),
    ("chf", "CHF"),
    ("sfr", "CHF"),
    ("fr", "francs"),
    ("franc", "francs"),
    ("francs", "francs"),
    ("cny", "CNY"),
    ("rmb", "CNY"),
    ("yuan", "CNY"),
    ("inr", "INR"),
    ("rs", "rupees"),
    ("rupee", "rupees"),
    ("rupees", "rupees"),
    ("sek", "SEK"),
    ("nok", "NOK"),
    ("dkk", "DKK"),
    ("kr", "kronor or kroner"),
    ("krw", "KRW"),
    ("won", "KRW"),
    ("mxn", "MXN"),
    ("brl", "BRL"),
    ("zar", "ZAR"),
    ("rand", "ZAR"),
    ("pln", "PLN"),
    ("zl", "PLN"),
    ("zloty", "PLN"),
    ("hkd", "HKD"),
    ("sgd", "SGD"),
    ("twd", "TWD"),
    ("nis", "ILS"),
    ("ils", "ILS"),
    ("aed", "AED"),
    ("sar", "SAR"),
    ("thb", "THB"),
    ("myr", "MYR"),
    ("idr", "IDR"),
    ("czk", "CZK"),
    ("huf", "HUF"),
    ("ron", "RON"),
    ("uah", "UAH"),
];

/// Words that mark a currency all by themselves, whatever else the header says.
const CURRENCY_MARKS: [&str; 34] = [
    "eur",
    "euro",
    "euros",
    "eurocent",
    "eurocents",
    "gbp",
    "sterling",
    "pence",
    "cad",
    "aud",
    "nzd",
    "jpy",
    "chf",
    "sfr",
    "cny",
    "rmb",
    "inr",
    "sek",
    "nok",
    "dkk",
    "krw",
    "mxn",
    "brl",
    "zar",
    "pln",
    "hkd",
    "sgd",
    "twd",
    "nis",
    "ils",
    "aed",
    "thb",
    "czk",
    "huf",
];

/// What a `$` with letters directly before it means. It is never the US dollar unless it says
/// so: `A$`, `R$`, and `C$` are not.
fn prefixed_dollar(prefix: &str) -> Option<&'static str> {
    match prefix.to_lowercase().as_str() {
        "a" | "au" | "aud" => Some("AUD"),
        "c" | "ca" | "cad" => Some("CAD"),
        "r" => Some("BRL"),
        "nz" | "nzd" => Some("NZD"),
        "hk" | "hkd" => Some("HKD"),
        "s" | "sg" | "sgd" => Some("SGD"),
        "mx" | "mex" => Some("MXN"),
        _ => None,
    }
}

/// The first `$` in a header that is not plainly a US dollar: the letters before it, with what
/// they name if Tonans knows.
fn foreign_dollar(header: &str) -> Option<(String, Option<&'static str>)> {
    for (at, _) in header.match_indices('$') {
        let prefix: String = {
            let reversed: String = header[..at]
                .chars()
                .rev()
                .take_while(|c| c.is_alphabetic())
                .collect();
            reversed.chars().rev().collect()
        };
        let lower = prefix.to_lowercase();
        // `Price$/kWh` is a price and a dollar sign, and `US$` is what it says.
        let plain = lower.is_empty()
            || lower == "us"
            || lower == "usd"
            || PRICE_WORDS.contains(&lower.as_str())
            || IMPORT_WORDS.contains(&lower.as_str())
            || EXPORT_WORDS.contains(&lower.as_str());
        if !plain {
            return Some((format!("{prefix}$"), prefixed_dollar(&prefix)));
        }
    }
    None
}

/// The words of a header with any statement of how long an interval is taken out, which says
/// nothing about what the column measures: `15min`, `15 min`, `hourly`, `per interval`. The `in`
/// of `Load in kW` goes too, when a unit follows it.
fn without_interval_phrases(words: &[String]) -> Vec<&str> {
    let is_minutes = |word: &str| matches!(word, "min" | "mins" | "minute" | "minutes");
    let is_number = |word: &str| !word.is_empty() && word.chars().all(|c| c.is_ascii_digit());
    let mut kept = Vec::with_capacity(words.len());
    let mut index = 0;
    while index < words.len() {
        let word = words[index].as_str();
        let next = words.get(index + 1).map(String::as_str);
        if is_number(word) && next.is_some_and(is_minutes)
            || (word == "per" && next == Some("interval"))
        {
            index += 2;
            continue;
        }
        if word == "in" && next.is_some_and(is_unit_word) {
            index += 1;
            continue;
        }
        let digits = word.trim_end_matches(|c: char| c.is_ascii_alphabetic());
        if matches!(word, "hourly" | "interval")
            || (!digits.is_empty() && is_number(digits) && is_minutes(&word[digits.len()..]))
        {
            index += 1;
            continue;
        }
        kept.push(word);
        index += 1;
    }
    kept
}

/// Whether a word states a unit, of power, energy, temperature, or money.
fn is_unit_word(word: &str) -> bool {
    POWER_UNIT_WORDS.contains(&word)
        || UNREAD_POWER_UNIT_WORDS.contains(&word)
        || TEMPERATURE_UNIT_WORDS.contains(&word)
        || matches!(word, "usd" | "dollars" | "cents" | "mwh")
}

/// The first word that is on none of the lists.
fn first_unknown<'a>(words: &[&'a str], lists: &[&[&str]]) -> Option<&'a str> {
    words
        .iter()
        .find(|word| !lists.iter().any(|list| list.contains(word)))
        .copied()
}

/// The piece of the header a word came from, as the user wrote it.
fn as_written<'a>(header: &'a str, word: &'a str) -> &'a str {
    let runs = || header.split(|c: char| !c.is_alphanumeric());
    runs()
        .find(|run| run.to_lowercase() == word)
        .or_else(|| runs().find(|run| run.to_lowercase().contains(word)))
        .unwrap_or(word)
}

/// Read one column. `Err` holds the note that says why nothing is suggested for it.
fn classify_column(
    header: &str,
    samples: &[&str],
) -> std::result::Result<ColumnSuggestion, String> {
    let tokens = header_tokens(header);
    let words = header_words(header, &tokens);
    let core = without_interval_phrases(&words);
    let has = |options: &[&str]| words.iter().any(|word| options.contains(&word.as_str()));
    let shown = header.trim();
    let suggestion = |role: &str, unit, reason: String, check| ColumnSuggestion {
        header: header.to_string(),
        role: role.to_string(),
        unit,
        reason,
        check,
    };

    // The values settle this one, and the importer reads the same formats.
    if samples
        .first()
        .is_some_and(|first| parses_as_timestamp(first))
    {
        return Ok(suggestion(
            "timestamp",
            None,
            format!("Its first value, {}, reads as a timestamp.", samples[0]),
            None,
        ));
    }

    if let Some((written, name)) = foreign_dollar(header) {
        return Err(match name {
            Some(name) => format!(
                "'{shown}' is in {name}, written {written}, not US dollars. Convert the column \
                 first, then choose its meaning and unit."
            ),
            None => format!(
                "'{shown}' uses {written}, which is not recognized as US dollars. If it is a price, \
                 convert it first, then choose its meaning and unit."
            ),
        });
    }

    // A column of words cannot be power or a price, whatever its header says.
    let not_numbers = !samples.is_empty()
        && samples
            .iter()
            .all(|cell| !cell.is_empty() && cell.parse::<f64>().is_err());

    if has(&PERIOD_WORDS)
        && samples.iter().all(|cell| cell.parse::<f64>().is_err())
        && first_unknown(&core, &[&PERIOD_WORDS, &PERIOD_QUALIFIERS]).is_none()
    {
        return Ok(suggestion(
            "demand_period",
            None,
            format!("The header '{shown}' names a period, and its values are labels."),
            None,
        ));
    }

    if has(&PRICE_WORDS)
        || has(&MONEY_MARKS)
        || has(&CURRENCY_MARKS)
        || cents_word(header, &words).is_some()
    {
        return classify_price(header, &words, samples, not_numbers)
            .map(|(role, unit, reason, check)| suggestion(role, unit, reason, check));
    }

    if has(&TEMPERATURE_WORDS) || (has(&["dry"]) && has(&["bulb"])) {
        if has(&["kelvin", "k"]) {
            return Err(format!(
                "'{shown}' states Kelvin. Convert the column to Celsius or Fahrenheit first."
            ));
        }
        if let Some(word) = first_unknown(&core, &[&TEMPERATURE_WORDS, &TEMPERATURE_QUALIFIERS]) {
            return Err(format!(
                "'{shown}' names a temperature, but '{}' in it is not recognized. Only the air \
                 temperature at the site is read. Choose a meaning if this is that, or ignore it.",
                as_written(header, word)
            ));
        }
        let celsius = has(&["c", "celsius", "centigrade", "degc"]);
        let fahrenheit = has(&["f", "fahrenheit", "degf"]);
        let unit = match (celsius, fahrenheit) {
            (true, false) => ImportUnit::Celsius,
            (false, true) => ImportUnit::Fahrenheit,
            _ => {
                return Err(format!(
                    "'{shown}' names a temperature, but does not state one unit, Celsius or \
                     Fahrenheit. Choose the meaning and unit yourself."
                ))
            }
        };
        return Ok(suggestion(
            "temperature",
            Some(unit),
            format!(
                "The header '{shown}' names a temperature, in {}.",
                unit.label()
            ),
            None,
        ));
    }

    // `Critical Power` is a load by another name. Plain `Power` says nothing.
    let names_load = has(&LOAD_WORDS) || (has(&CRITICAL_WORDS) && has(&["power"]));
    let names_generation = has(&GENERATION_WORDS);
    if !names_load && !names_generation {
        return Err(format!(
            "Nothing in '{shown}' says what it means. Choose a meaning, or ignore it."
        ));
    }
    if names_load && names_generation {
        return Err(format!(
            "'{shown}' names both load and generation. Choose a meaning, or ignore it."
        ));
    }
    let names_critical = names_load && has(&CRITICAL_WORDS);
    let (role, phrase) = if names_critical {
        ("critical_load", "a critical load")
    } else if names_generation {
        ("generation", "on-site generation")
    } else {
        ("load", "a load")
    };

    if has(&["net"]) {
        return Err(format!(
            "'{shown}' says net, which may mean generation was already subtracted. Mapping it as \
             load while also mapping generation would count the generation twice. Choose a meaning \
             if you know which it is, or ignore it."
        ));
    }
    // `P` is the symbol for active power, as in `P_Load_kW`. After a unit, as in `kW(p)`, it is a
    // peak rating, which is not what a column measures.
    let core: Vec<&str> = core
        .iter()
        .enumerate()
        .filter(|(at, word)| {
            **word != "p" || (*at > 0 && POWER_UNIT_WORDS.contains(&core[*at - 1]))
        })
        .map(|(_, word)| *word)
        .collect();
    let unknown = if names_generation {
        first_unknown(
            &core,
            &[
                &GENERATION_WORDS,
                &GENERATION_QUALIFIERS,
                &COLUMN_QUALIFIERS,
                &POWER_UNIT_WORDS,
                &UNREAD_POWER_UNIT_WORDS,
            ],
        )
    } else {
        first_unknown(
            &core,
            &[
                &LOAD_WORDS,
                &LOAD_QUALIFIERS,
                &CRITICAL_WORDS,
                &COLUMN_QUALIFIERS,
                &POWER_UNIT_WORDS,
                &UNREAD_POWER_UNIT_WORDS,
                &["total"],
            ],
        )
    };
    if let Some(word) = unknown {
        // No role is named here: `Non_Critical_Load` is not nearly a critical load.
        return Err(format!(
            "'{}' in '{shown}' is not recognized. Choose a meaning if you know, or ignore it.",
            as_written(header, word)
        ));
    }
    if not_numbers {
        return Err(format!(
            "'{shown}' names {phrase}, but its values are not numbers. Choose a meaning, or ignore \
             it."
        ));
    }

    let (unit, unit_check) = power_unit(&tokens, &words).map_err(|why| {
        format!("'{shown}' names {phrase}, but {why} Choose the meaning and unit yourself.")
    })?;
    let energy = matches!(
        unit,
        ImportUnit::KilowattHourPerInterval | ImportUnit::WattHourPerInterval
    );
    // A total in kW is what the site draws. A total in kWh may be a running total.
    if energy && has(&["total"]) {
        return Err(format!(
            "'{shown}' says total, in energy, which may be a running total rather than the amount \
             in each interval. Convert it to the amount per interval, or choose its meaning \
             yourself."
        ));
    }
    let rising = (energy && strictly_rising(samples)).then(|| {
        format!(
            "The first {} values only rise. A running meter total would look like this, and \
             must be converted to the amount per interval.",
            samples.len()
        )
    });
    let demand = (role == "load" && has(&["demand"])).then(|| {
        "'Demand' is read as the load. Check it is the mean power in each interval, not a \
         billing figure."
            .to_string()
    });
    let gen = (role == "generation" && has(&["gen"])).then(|| {
        "'Gen' is read as on-site generation. Check it is not a generator that runs only when \
         called on."
            .to_string()
    });
    Ok(suggestion(
        role,
        Some(unit),
        format!("The header '{shown}' names {phrase}, in {}.", unit.label()),
        join_checks([unit_check, rising, demand, gen]),
    ))
}

fn join_checks<const N: usize>(checks: [Option<String>; N]) -> Option<String> {
    let found: Vec<String> = checks.into_iter().flatten().collect();
    (!found.is_empty()).then(|| found.join(" "))
}

/// Whether every sample is a number and each is larger than the one before.
fn strictly_rising(samples: &[&str]) -> bool {
    let numbers: Option<Vec<f64>> = samples.iter().map(|cell| cell.parse().ok()).collect();
    numbers.is_some_and(|numbers| {
        numbers.len() >= 3 && numbers.windows(2).all(|pair| pair[0] < pair[1])
    })
}

/// A power or energy column's unit and anything to check about it, or why there is not one.
fn power_unit(
    tokens: &[String],
    words: &[String],
) -> std::result::Result<(ImportUnit, Option<String>), String> {
    let has = |options: &[&str]| words.iter().any(|word| options.contains(&word.as_str()));
    let spelled = |spelling: &str| tokens.iter().any(|token| token == spelling);
    if spelled("mW") || spelled("mWh") {
        return Err("'mW' is milliwatts, which cannot be read.".to_string());
    }
    if let Some((_, name)) = UNREAD_POWER_UNITS.iter().find(|(word, _)| has(&[*word])) {
        return Err(format!(
            "it states {name}, which cannot be read for this column, so convert it to kW or \
             kWh first."
        ));
    }
    let found: Vec<ImportUnit> = [
        (has(&["kw", "kilowatt", "kilowatts"]), ImportUnit::Kilowatt),
        (has(&["w", "watt", "watts"]), ImportUnit::Watt),
        (has(&["mw", "megawatt", "megawatts"]), ImportUnit::Megawatt),
        (has(&["kwh"]), ImportUnit::KilowattHourPerInterval),
        (has(&["wh"]), ImportUnit::WattHourPerInterval),
    ]
    .into_iter()
    .filter_map(|(present, unit)| present.then_some(unit))
    .collect();
    match found.as_slice() {
        [] => Err("it does not state a unit.".to_string()),
        [ImportUnit::Megawatt] if spelled("mw") => Ok((
            ImportUnit::Megawatt,
            Some("'mw' is read as megawatts, not milliwatts; check that.".to_string()),
        )),
        [only] => Ok((*only, None)),
        // Two units in one header is not a statement of either.
        _ => Err("it states more than one unit.".to_string()),
    }
}

/// A money column's role, unit, reason, and anything to check.
type PriceReading = (&'static str, Option<ImportUnit>, String, Option<String>);

/// Read a header that is about money. `Err` holds the note that says why nothing is suggested.
fn classify_price(
    header: &str,
    words: &[String],
    samples: &[&str],
    not_numbers: bool,
) -> std::result::Result<PriceReading, String> {
    let has = |options: &[&str]| words.iter().any(|word| options.contains(&word.as_str()));
    let shown = header.trim();
    let written_cents = cents_word(header, words);
    let mut core = without_interval_phrases(words);
    // `c` means cents only right before `/kWh`, and `net` is a word here only in `net metering`.
    core.retain(|word| Some(*word) != written_cents);
    if let Some(at) = core.iter().position(|word| *word == "metering") {
        if at > 0 && core[at - 1] == "net" {
            core.remove(at - 1);
        }
    }

    if let Some((_, name)) = OTHER_CURRENCIES.iter().find(|(word, _)| has(&[*word])) {
        return Err(format!(
            "'{shown}' is in {name}, not US dollars. Convert the column first, then choose its \
             meaning and unit."
        ));
    }
    if has(&["demand", "charge", "charges", "fixed"])
        || (!has(&["kwh", "wh", "mwh", "gwh"])
            && has(&[
                "kw",
                "kilowatt",
                "kilowatts",
                "mw",
                "w",
                "watt",
                "watts",
                "kva",
            ]))
    {
        return Err(format!(
            "'{shown}' is per kW or is a charge, not an energy price per kWh. Demand charges go on \
             the tariff."
        ));
    }
    if let Some(word) = first_unknown(
        &core,
        &[
            &PRICE_WORDS,
            &IMPORT_WORDS,
            &EXPORT_WORDS,
            &PRICE_QUALIFIERS,
        ],
    ) {
        return Err(format!(
            "'{shown}' looks like money, but '{}' in it is not recognized. Only US dollars can be \
             read. Choose a meaning if you know what it is, or ignore it.",
            as_written(header, word)
        ));
    }
    if has(&IMPORT_WORDS) && has(&EXPORT_WORDS) {
        return Err(format!(
            "'{shown}' names both import and export. Choose a meaning, or ignore it."
        ));
    }
    if not_numbers {
        return Err(format!(
            "'{shown}' names a price, but its values are not numbers. Choose a meaning, or ignore it."
        ));
    }
    if !has(&["kwh", "mwh"]) {
        return Err(format!(
            "'{shown}' does not say per what, such as per kWh, so it may be an amount rather than a \
             price. Choose its meaning and unit yourself."
        ));
    }
    // `Cost_kWh` and `Solar_Credit_kWh` may well be money and energy, not one over the other.
    let says_per = has(&["per"]) || header.contains('/');
    if has(&["cost", "credit"]) && !says_per {
        return Err(format!(
            "'{shown}' does not say per kWh or /kWh, so it may be an amount rather than a price. \
             Choose its meaning and unit yourself."
        ));
    }

    let dollars = has(&["usd", "dollar", "dollars"]);
    let cents = has(&["cents"]) || written_cents.is_some();
    let (unit, unit_check) = if has(&["mwh"]) {
        if !dollars || cents {
            return Err(format!(
                "'{shown}' is per MWh, which can be read only in US dollars. Choose its meaning \
                 and unit yourself, or convert it first."
            ));
        }
        (Some(ImportUnit::UsdPerMegawattHour), None)
    } else if let Some(word) = written_cents {
        // `c` and `ct` are cents, of a currency they do not name.
        (
            Some(ImportUnit::CentsPerKilowattHour),
            Some(format!(
                "'{word}' is read as US cents; check that against a bill."
            )),
        )
    } else if cents {
        (Some(ImportUnit::CentsPerKilowattHour), None)
    } else if dollars {
        (Some(ImportUnit::UsdPerKilowattHour), None)
    } else {
        // The header says per kWh and truly no more.
        let (unit, check) = unit_by_size(samples);
        (unit, Some(check))
    };
    // A bare `$` is most often the US dollar, and it does not say so.
    let bare_dollar =
        (header.contains('$') && !has(&["us"]) && !header.to_lowercase().contains("usd"))
            .then(|| "'$' is read as US dollars; check that against a bill.".to_string());

    let (role, phrase, direction) = if has(&EXPORT_WORDS) {
        ("export_price", "an export credit", None)
    } else if has(&IMPORT_WORDS) {
        ("import_price", "an import price", None)
    } else {
        (
            "import_price",
            "a price",
            Some(
                "It does not say import or export. It is suggested as the import price; check that."
                    .to_string(),
            ),
        )
    };
    Ok((
        role,
        unit,
        format!("The header '{shown}' names {phrase}."),
        join_checks([direction, unit_check, bare_dollar]),
    ))
}

/// The short word for cents in `c/kWh`, `ct/kWh`, or `c per kWh`. Anywhere else it is not one.
fn cents_word<'a>(header: &str, words: &'a [String]) -> Option<&'a str> {
    let short = |word: &str| matches!(word, "c" | "ct" | "cts" | "cent");
    let before_kwh = |index: usize| match words.get(index + 1).map(String::as_str) {
        Some("kwh") => header.contains('/'),
        Some("per") => words.get(index + 2).is_some_and(|word| word == "kwh"),
        _ => false,
    };
    words
        .iter()
        .enumerate()
        .find(|(index, word)| short(word) && before_kwh(*index))
        .map(|(_, word)| word.as_str())
}

/// The unit that the size of a price's sample values points to, with what to say about it.
/// Zeros say nothing: a credit can be 0 overnight in any unit. Under 1 is too small for cents and
/// 2 or more is too large for dollars, so a set of values with any doubt gets no unit.
fn unit_by_size(samples: &[&str]) -> (Option<ImportUnit>, String) {
    let values: Vec<f64> = samples
        .iter()
        .filter_map(|cell| cell.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value != 0.0)
        .collect();
    if values.is_empty() {
        return (
            None,
            "The header gives no currency, and the sample rows hold no value other than zero. \
             Choose the unit."
                .to_string(),
        );
    }
    let listed = values
        .iter()
        .map(f64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let smallest = values.iter().fold(f64::INFINITY, |low, v| low.min(v.abs()));
    let largest = values.iter().fold(0.0_f64, |high, v| high.max(v.abs()));
    let reading = if largest < 1.0 {
        Some((ImportUnit::UsdPerKilowattHour, "dollars"))
    } else if smallest >= 1.0 && largest >= 2.0 {
        Some((ImportUnit::CentsPerKilowattHour, "cents"))
    } else {
        None
    };
    match reading {
        Some((unit, name)) => (
            Some(unit),
            format!(
                "The header gives no currency. Values of {listed} read as {name} per kWh by size \
                 alone; check that against a bill."
            ),
        ),
        None => (
            None,
            format!(
                "The header gives no currency, and values of {listed} do not clearly fit dollars \
                 or cents per kWh. Choose the unit."
            ),
        ),
    }
}

// ---------------------------------------------------------------------------
// Value accumulation
// ---------------------------------------------------------------------------

struct PeriodAccumulator {
    labels: Vec<String>,
    index: Vec<u16>,
}

struct Accumulator {
    series: BTreeMap<SeriesKind, Vec<f64>>,
    periods: Option<PeriodAccumulator>,
    synthesized: Vec<usize>,
    warnings: Vec<Warning>,
    interval_hours: f64,
    /// Whether the gap policy fills by holding the previous value rather than
    /// with zero. Resolved once, so the inner fill loop carries no policy match.
    hold_last: bool,
    saw_negative_import_price: bool,
}

impl Accumulator {
    fn new(plan: &ColumnPlan, interval_hours: f64, gap_policy: GapPolicy) -> Self {
        let mut series = BTreeMap::new();
        for (kind, _, _) in &plan.series {
            series.insert(*kind, Vec::new());
        }
        Self {
            series,
            periods: plan.demand_period.as_ref().map(|_| PeriodAccumulator {
                labels: Vec::new(),
                index: Vec::new(),
            }),
            synthesized: Vec::new(),
            warnings: Vec::new(),
            interval_hours,
            hold_last: matches!(gap_policy, GapPolicy::HoldLastValue { .. }),
            saw_negative_import_price: false,
        }
    }

    fn interval_count(&self) -> usize {
        self.series
            .values()
            .next()
            .map_or(0, Vec::len)
            .max(self.periods.as_ref().map_or(0, |p| p.index.len()))
    }

    /// Insert `count` synthesized intervals ahead of the row about to be pushed.
    ///
    /// Refused *before* anything is allocated. `count` is bounded per gap by the
    /// policy, but the running total is not: a file whose rows sit an allowed gap
    /// apart, or two rows a century apart, would otherwise grow the series until the
    /// process died. Neither the fraction ceiling nor `Dataset::validate` can catch
    /// that — both run at the end, on the memory that was already committed.
    fn synthesize(&mut self, count: usize) -> Result<()> {
        if count == 0 {
            return Ok(());
        }
        if self.periods.is_some() {
            return Err(Error::import(
                "import.demand_period_gap",
                None,
                None,
                "a timestamp gap cannot be filled while a demand-period column is mapped. Repair \
                 the source rows or import without demand-period pricing.",
            ));
        }
        let total = self.interval_count().saturating_add(count);
        if total > limits::MAX_INTERVALS {
            return Err(Error::limit(
                "limit.intervals",
                limits::MAX_INTERVALS as u64,
                total as u64,
                format!(
                    "filling this gap would take the data set to {total} intervals, past the {} \
                     this build holds. The file's timestamps span far more time than its rows \
                     account for.",
                    limits::MAX_INTERVALS
                ),
            ));
        }
        for _ in 0..count {
            let index = self.interval_count();
            self.synthesized.push(index);
            for values in self.series.values_mut() {
                let filled = if self.hold_last {
                    values.last().copied().unwrap_or(0.0)
                } else {
                    0.0
                };
                values.push(filled);
            }
        }
        Ok(())
    }

    fn push_row(&mut self, record: &csv::StringRecord, line: u64, plan: &ColumnPlan) -> Result<()> {
        let mut load: Option<f64> = None;
        let mut critical: Option<f64> = None;

        for (kind, index, column) in &plan.series {
            let raw = field(record, *index, line, &column.header)?;
            let parsed = parse_value(raw, line, &column.header)?;
            let value = column.unit.to_canonical(parsed, self.interval_hours);
            if !value.is_finite() {
                return Err(Error::import(
                    "import.non_finite",
                    Some(line),
                    Some(&column.header),
                    format!("'{raw}' converts to a value that is not finite"),
                ));
            }

            match kind {
                SeriesKind::Load | SeriesKind::CriticalLoad | SeriesKind::Generation => {
                    if value < 0.0 {
                        return Err(Error::import(
                            "import.negative_power",
                            Some(line),
                            Some(&column.header),
                            format!(
                                "{role} is {value} {unit}, which is negative. Load and generation \
                                 must be separate non-negative series; a net-metered column signed \
                                 both ways cannot be separated back into the two.",
                                role = kind.as_str(),
                                unit = kind.unit(),
                            ),
                        ));
                    }
                    if *kind == SeriesKind::Load {
                        load = Some(value);
                    }
                    if *kind == SeriesKind::CriticalLoad {
                        critical = Some(value);
                    }
                }
                SeriesKind::Temperature => {
                    if !(-90.0..=70.0).contains(&value) {
                        return Err(Error::import(
                            "import.value_out_of_range",
                            Some(line),
                            Some(&column.header),
                            format!("temperature {value:.1} °C is outside the plausible range -90 to 70 °C"),
                        ));
                    }
                }
                SeriesKind::ImportPrice => {
                    if value < 0.0 {
                        self.saw_negative_import_price = true;
                    }
                }
                SeriesKind::ExportPrice => {}
            }

            self.series
                .get_mut(kind)
                .expect("series map was built from the same plan")
                .push(value);
        }

        // Critical load is a *subset* of load, by definition. If it exceeds load,
        // one of the two columns is wrong, and no arithmetic downstream can be
        // trusted.
        if let (Some(load), Some(critical)) = (load, critical) {
            if critical > load + 1e-9 {
                return Err(Error::import(
                    "import.critical_exceeds_load",
                    Some(line),
                    None,
                    format!(
                        "critical load ({critical} kW) is greater than total load ({load} kW). \
                         Critical load is the part of the load that must survive an outage, so it \
                         cannot exceed it. Check that the two columns are not swapped."
                    ),
                ));
            }
        }

        if let Some((index, header)) = &plan.demand_period {
            let raw = field(record, *index, line, header)?;
            self.push_period(raw, line, header)?;
        }
        Ok(())
    }

    fn push_period(&mut self, raw: &str, line: u64, header: &str) -> Result<()> {
        if raw.is_empty() {
            return Err(Error::import(
                "import.empty_demand_period",
                Some(line),
                Some(header),
                "the demand-charge period is empty. Every interval must belong to a period.",
            ));
        }
        if raw.chars().count() > MAX_PERIOD_LABEL_CHARS {
            return Err(Error::limit(
                "limit.demand_period_label",
                MAX_PERIOD_LABEL_CHARS as u64,
                raw.chars().count() as u64,
                format!("the demand-charge period label on line {line} is too long"),
            ));
        }
        if !raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ' '))
        {
            return Err(Error::import(
                "import.demand_period_charset",
                Some(line),
                Some(header),
                format!(
                    "the demand-charge period label '{raw}' contains characters outside \
                     letters, digits, spaces, hyphens, and underscores"
                ),
            ));
        }

        let periods = self
            .periods
            .as_mut()
            .expect("plan has a demand period column");
        let position = match periods.labels.iter().position(|l| l == raw) {
            Some(position) => position,
            None => {
                if periods.labels.len() >= limits::MAX_DEMAND_PERIODS {
                    return Err(Error::limit(
                        "limit.demand_periods",
                        limits::MAX_DEMAND_PERIODS as u64,
                        periods.labels.len() as u64 + 1,
                        format!(
                            "the file uses more than {} distinct demand-charge periods",
                            limits::MAX_DEMAND_PERIODS
                        ),
                    ));
                }
                periods.labels.push(raw.to_string());
                periods.labels.len() - 1
            }
        };
        periods.index.push(
            u16::try_from(position).expect("demand period count is bounded well below u16::MAX"),
        );
        Ok(())
    }
}

fn parse_value(raw: &str, line: u64, header: &str) -> Result<f64> {
    if raw.is_empty() {
        return Err(Error::import(
            "import.empty_value",
            Some(line),
            Some(header),
            format!("column '{header}' is empty on this row."),
        ));
    }
    if raw.len() > limits::MAX_CSV_FIELD_BYTES {
        return Err(Error::limit(
            "limit.csv_field",
            limits::MAX_CSV_FIELD_BYTES as u64,
            raw.len() as u64,
            format!("the value in column '{header}' on line {line} is too long"),
        ));
    }

    // `f64::from_str` happily accepts "NaN", "inf", and "-Infinity". Every one of
    // them would poison an energy balance and serialize as `null` in a report.
    let value: f64 = raw.parse().map_err(|_| {
        Error::import(
            "import.bad_number",
            Some(line),
            Some(header),
            format!("'{raw}' is not a number"),
        )
    })?;
    if !value.is_finite() {
        return Err(Error::import(
            "import.non_finite",
            Some(line),
            Some(header),
            format!("'{raw}' is not a finite number."),
        ));
    }
    if value.abs() > limits::MAX_ABS_VALUE {
        return Err(Error::import(
            "import.value_out_of_range",
            Some(line),
            Some(header),
            format!(
                "{value} exceeds the magnitude limit of {}.",
                limits::MAX_ABS_VALUE
            ),
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::fmt::Write as _;

    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    fn mapping() -> ColumnMapping {
        let mut series = BTreeMap::new();
        series.insert(
            SeriesKind::Load,
            MappedColumn {
                header: "load_kw".to_string(),
                unit: ImportUnit::Kilowatt,
            },
        );
        ColumnMapping {
            timestamp: "timestamp".to_string(),
            series,
            demand_period: None,
            ignore_columns: Vec::new(),
        }
    }

    fn request(gap_policy: GapPolicy) -> ImportRequest {
        ImportRequest {
            filename: "meter.csv".to_string(),
            timezone: "America/New_York".to_string(),
            interval_minutes: 60,
            gap_policy,
            mapping: mapping(),
        }
    }

    #[test]
    fn a_clean_file_imports() {
        let csv = "timestamp,load_kw\n2024-01-01T00:00:00-05:00,10\n2024-01-01T01:00:00-05:00,20\n";
        let report = import(csv.as_bytes(), &request(GapPolicy::Reject), now()).unwrap();
        assert_eq!(report.row_count, 2);
        assert_eq!(report.interval_count, 2);
        assert_eq!(report.synthesized_interval_count, 0);
        let values = &report.dataset.series[&SeriesKind::Load].values;
        assert_eq!(values, &[10.0, 20.0]);
        assert_eq!(
            report.dataset.start_utc.to_rfc3339(),
            "2024-01-01T05:00:00+00:00"
        );
    }

    #[test]
    fn one_source_column_cannot_claim_two_meanings() {
        let csv = "timestamp,load_kw\n2024-01-01T00:00:00-05:00,10\n";
        let mut request = request(GapPolicy::Reject);
        request.mapping.series.insert(
            SeriesKind::Generation,
            MappedColumn {
                header: "load_kw".to_string(),
                unit: ImportUnit::Kilowatt,
            },
        );
        let error = import(csv.as_bytes(), &request, now()).unwrap_err();
        assert_eq!(error.code(), "import.column_mapped_twice");

        request.mapping.series.remove(&SeriesKind::Generation);
        request.mapping.ignore_columns.push("load_kw".to_string());
        let error = import(csv.as_bytes(), &request, now()).unwrap_err();
        assert_eq!(error.code(), "import.mapped_column_ignored");
    }

    #[test]
    fn preview_preserves_whitespace_instead_of_quietly_repairing_it() {
        let csv = " timestamp ,load_kw\n 2024-01-01T00:00:00-05:00 , 10 \n";
        let preview = preview(csv.as_bytes()).unwrap();
        assert_eq!(preview.headers, [" timestamp ", "load_kw"]);
        assert_eq!(
            preview.sample_rows[0],
            [" 2024-01-01T00:00:00-05:00 ", " 10 "]
        );
    }

    #[test]
    fn a_utf8_bom_does_not_break_the_first_header() {
        let csv = "\u{FEFF}timestamp,load_kw\n2024-01-01T00:00:00-05:00,10\n";
        let report = import(csv.as_bytes(), &request(GapPolicy::Reject), now()).unwrap();
        assert_eq!(report.interval_count, 1);
    }

    #[test]
    fn units_are_converted_using_the_declared_interval() {
        // 15 kWh in a 60-minute interval is 15 kW. In a 15-minute interval it is 60 kW.
        let csv = "timestamp,load\n2024-01-01T00:00:00-05:00,15\n";
        let mut request = request(GapPolicy::Reject);
        request.mapping.series.insert(
            SeriesKind::Load,
            MappedColumn {
                header: "load".to_string(),
                unit: ImportUnit::KilowattHourPerInterval,
            },
        );

        request.interval_minutes = 60;
        let hourly = import(csv.as_bytes(), &request, now()).unwrap();
        assert!((hourly.dataset.series[&SeriesKind::Load].values[0] - 15.0).abs() < 1e-12);

        request.interval_minutes = 15;
        let quarterly = import(csv.as_bytes(), &request, now()).unwrap();
        assert!((quarterly.dataset.series[&SeriesKind::Load].values[0] - 60.0).abs() < 1e-12);
    }

    #[test]
    fn a_unit_that_cannot_express_a_role_is_rejected() {
        let csv = "timestamp,load_kw\n2024-01-01T00:00:00-05:00,10\n";
        let mut request = request(GapPolicy::Reject);
        request.mapping.series.insert(
            SeriesKind::Load,
            MappedColumn {
                header: "load_kw".to_string(),
                unit: ImportUnit::Celsius,
            },
        );
        let error = import(csv.as_bytes(), &request, now()).unwrap_err();
        assert_eq!(error.code(), "import.unit_mismatch");
    }

    #[test]
    fn an_unmapped_column_stops_the_import() {
        let csv = "timestamp,load_kw,mystery\n2024-01-01T00:00:00-05:00,10,7\n";
        let error = import(csv.as_bytes(), &request(GapPolicy::Reject), now()).unwrap_err();
        assert_eq!(error.code(), "import.unmapped_column");
        assert!(error.to_string().contains("mystery"));

        // ...unless it is explicitly ignored, which is recorded as a warning.
        let mut request = request(GapPolicy::Reject);
        request.mapping.ignore_columns = vec!["mystery".to_string()];
        let report = import(csv.as_bytes(), &request, now()).unwrap();
        assert_eq!(report.dataset.source.ignored_columns, vec!["mystery"]);
        assert!(report
            .warnings
            .iter()
            .any(|w| w.code == "data.ignored_columns"));
    }

    /// One day of hourly readings from `2024-01-01T00:00-05:00`, omitting the hours
    /// in `missing`. Load is `10 + hour`, so a filled interval is always
    /// distinguishable from a measured one. January in `America/New_York` has no DST
    /// transition, and the offsets are explicit, so nothing here depends on timezone
    /// resolution.
    fn hourly_csv(hours: u32, missing: &[u32]) -> String {
        let mut out = String::from("timestamp,load_kw\n");
        for hour in (0..hours).filter(|h| !missing.contains(h)) {
            let _ = writeln!(
                out,
                "2024-01-{day:02}T{clock:02}:00:00-05:00,{load}",
                day = 1 + hour / 24,
                clock = hour % 24,
                load = 10 + hour
            );
        }
        out
    }

    #[test]
    fn a_gap_is_rejected_by_default_and_filled_only_on_request() {
        // A day of hourly data with 05:00 missing: 23 rows, 24 intervals, one of them
        // synthesized. Deliberately large enough that the single hole stays under the
        // synthesized-fraction ceiling, which is a different rule and has its own test.
        let csv = hourly_csv(24, &[5]);

        let error = import(csv.as_bytes(), &request(GapPolicy::Reject), now()).unwrap_err();
        assert_eq!(error.code(), "import.gap");

        let report = import(
            csv.as_bytes(),
            &request(GapPolicy::FillZero {
                max_consecutive_intervals: 2,
            }),
            now(),
        )
        .unwrap();
        assert_eq!(report.row_count, 23);
        assert_eq!(report.interval_count, 24);
        assert_eq!(report.synthesized_interval_count, 1);
        assert_eq!(report.dataset.synthesized_interval_indices, vec![5]);

        let values = &report.dataset.series[&SeriesKind::Load].values;
        assert_eq!(values[4], 14.0, "the reading before the hole is untouched");
        assert_eq!(values[5], 0.0, "FillZero fills the hole with zero");
        assert_eq!(values[6], 16.0, "the reading after the hole is untouched");

        let warning = report
            .warnings
            .iter()
            .find(|w| w.code == "data.synthesized_intervals")
            .expect("a synthesized interval must warn");
        assert_eq!(warning.severity, crate::error::Severity::Critical);

        let report = import(
            csv.as_bytes(),
            &request(GapPolicy::HoldLastValue {
                max_consecutive_intervals: 2,
            }),
            now(),
        )
        .unwrap();
        let values = &report.dataset.series[&SeriesKind::Load].values;
        assert_eq!(values[5], 14.0, "HoldLastValue holds the previous reading");
        assert_eq!(values[6], 16.0);
    }

    #[test]
    fn a_gap_with_demand_periods_is_never_filled_by_guessing_a_label() {
        let csv = "timestamp,load_kw,period\n\
                   2024-01-01T00:00:00-05:00,10,off-peak\n\
                   2024-01-01T02:00:00-05:00,12,on-peak\n";
        let mut request = request(GapPolicy::HoldLastValue {
            max_consecutive_intervals: 2,
        });
        request.mapping.demand_period = Some("period".to_string());
        let error = import(csv.as_bytes(), &request, now()).unwrap_err();
        assert_eq!(error.code(), "import.demand_period_gap");
    }

    #[test]
    fn a_file_that_is_mostly_holes_is_refused_whatever_the_gap_policy_says() {
        // A separate rule from the per-gap limit: every hole here is a single
        // interval, well inside `max_consecutive`, but an eighth of the data set
        // would be invented. Inventing an eighth of a file is not importing it.
        let csv = hourly_csv(24, &[3, 9, 17]); // 3 of 24 = 12.5%, over the 5% ceiling
        let error = import(
            csv.as_bytes(),
            &request(GapPolicy::FillZero {
                max_consecutive_intervals: 4,
            }),
            now(),
        )
        .unwrap_err();
        assert_eq!(error.code(), "limit.synthesized_fraction");
        assert!(error.to_string().contains("3 of 24"), "{error}");
    }

    #[test]
    fn a_gap_policy_cannot_ask_for_an_unbounded_allocation() {
        // The regression. `max_consecutive_intervals` arrives as an unvalidated number
        // in a JSON body, and both the fraction ceiling and `Dataset::validate` run at
        // the *end* — on memory that has already been committed. Two rows far enough
        // apart, with a policy permissive enough to bridge them, used to synthesize
        // the whole span before anything looked at how big it had become.
        let start = Utc.with_ymd_and_hms(2024, 1, 1, 5, 0, 0).unwrap();
        let end = start + TimeDelta::hours(limits::MAX_INTERVALS as i64 + 1);
        let csv = format!(
            "timestamp,load_kw\n{},10\n{},30\n",
            start.to_rfc3339(),
            end.to_rfc3339()
        );

        // A policy bigger than any data set is refused up front, by name.
        let error = import(
            csv.as_bytes(),
            &request(GapPolicy::FillZero {
                max_consecutive_intervals: usize::MAX,
            }),
            now(),
        )
        .unwrap_err();
        assert_eq!(error.code(), "limit.gap_policy");

        // And a policy right at the bound still cannot bridge that span: the running
        // interval cap stops it before the memory is committed.
        let error = import(
            csv.as_bytes(),
            &request(GapPolicy::FillZero {
                max_consecutive_intervals: limits::MAX_INTERVALS,
            }),
            now(),
        )
        .unwrap_err();
        assert_eq!(error.code(), "limit.intervals");
    }

    #[test]
    fn a_gap_longer_than_the_policy_allows_is_rejected() {
        let csv = "timestamp,load_kw\n2024-01-01T00:00:00-05:00,10\n2024-01-01T05:00:00-05:00,30\n";
        let error = import(
            csv.as_bytes(),
            &request(GapPolicy::FillZero {
                max_consecutive_intervals: 2,
            }),
            now(),
        )
        .unwrap_err();
        assert_eq!(error.code(), "import.gap_too_long");
    }

    #[test]
    fn duplicate_and_backwards_timestamps_are_rejected() {
        let duplicate =
            "timestamp,load_kw\n2024-01-01T00:00:00-05:00,10\n2024-01-01T00:00:00-05:00,20\n";
        assert_eq!(
            import(duplicate.as_bytes(), &request(GapPolicy::Reject), now())
                .unwrap_err()
                .code(),
            "import.duplicate_timestamp"
        );

        let backwards =
            "timestamp,load_kw\n2024-01-01T05:00:00-05:00,10\n2024-01-01T04:00:00-05:00,20\n";
        assert_eq!(
            import(backwards.as_bytes(), &request(GapPolicy::Reject), now())
                .unwrap_err()
                .code(),
            "import.non_monotonic_timestamp"
        );
    }

    #[test]
    fn a_timestamp_off_the_declared_grid_is_rejected() {
        let csv = "timestamp,load_kw\n2024-01-01T00:00:00-05:00,10\n2024-01-01T00:37:00-05:00,20\n";
        let error = import(csv.as_bytes(), &request(GapPolicy::Reject), now()).unwrap_err();
        assert_eq!(error.code(), "import.timestamp_not_aligned");
    }

    #[test]
    fn a_subsecond_timestamp_cannot_be_erased_by_any_gap_policy() {
        let csv = "timestamp,load_kw\n2024-01-01T00:00:00Z,10\n2024-01-01T01:00:00.500Z,20\n";
        for policy in [
            GapPolicy::Reject,
            GapPolicy::FillZero {
                max_consecutive_intervals: 1,
            },
            GapPolicy::HoldLastValue {
                max_consecutive_intervals: 1,
            },
        ] {
            let error = import(csv.as_bytes(), &request(policy), now()).unwrap_err();
            assert_eq!(error.code(), "import.timestamp_not_aligned");
        }
    }

    #[test]
    fn ignored_and_preview_fields_still_obey_the_per_field_byte_limit() {
        let oversized = "x".repeat(limits::MAX_CSV_FIELD_BYTES + 1);
        let csv = format!("timestamp,load_kw,notes\n2024-01-01T00:00:00Z,10,{oversized}\n");
        assert_eq!(
            preview(csv.as_bytes()).unwrap_err().code(),
            "limit.csv_field"
        );

        let mut request = request(GapPolicy::Reject);
        request.mapping.ignore_columns.push("notes".to_string());
        assert_eq!(
            import(csv.as_bytes(), &request, now()).unwrap_err().code(),
            "limit.csv_field"
        );
    }

    #[test]
    fn mixed_timestamp_formats_are_rejected() {
        let csv = "timestamp,load_kw\n2024-01-01T00:00:00-05:00,10\n2024-01-01T01:00:00,20\n";
        let error = import(csv.as_bytes(), &request(GapPolicy::Reject), now()).unwrap_err();
        assert_eq!(error.code(), "import.mixed_timestamp_format");
    }

    #[test]
    fn nan_infinity_and_empty_cells_are_rejected() {
        for (value, code) in [
            ("NaN", "import.non_finite"),
            ("nan", "import.non_finite"),
            ("inf", "import.non_finite"),
            ("-Infinity", "import.non_finite"),
            ("", "import.empty_value"),
            ("abc", "import.bad_number"),
            ("1e300", "import.value_out_of_range"),
        ] {
            let csv = format!("timestamp,load_kw\n2024-01-01T00:00:00-05:00,{value}\n");
            let error = import(csv.as_bytes(), &request(GapPolicy::Reject), now()).unwrap_err();
            assert_eq!(error.code(), code, "value {value:?}");
        }
    }

    #[test]
    fn negative_power_is_rejected_with_an_explanation() {
        let csv = "timestamp,load_kw\n2024-01-01T00:00:00-05:00,-5\n";
        let error = import(csv.as_bytes(), &request(GapPolicy::Reject), now()).unwrap_err();
        assert_eq!(error.code(), "import.negative_power");
        assert!(error.to_string().contains("net-metered"));
    }

    #[test]
    fn critical_load_above_total_load_is_rejected() {
        let csv = "timestamp,load_kw,crit_kw\n2024-01-01T00:00:00-05:00,10,20\n";
        let mut request = request(GapPolicy::Reject);
        request.mapping.series.insert(
            SeriesKind::CriticalLoad,
            MappedColumn {
                header: "crit_kw".to_string(),
                unit: ImportUnit::Kilowatt,
            },
        );
        let error = import(csv.as_bytes(), &request, now()).unwrap_err();
        assert_eq!(error.code(), "import.critical_exceeds_load");
        assert!(error.to_string().contains("swapped"));
    }

    #[test]
    fn duplicate_headers_are_rejected() {
        let csv = "timestamp,load_kw,load_kw\n2024-01-01T00:00:00-05:00,10,20\n";
        let error = import(csv.as_bytes(), &request(GapPolicy::Reject), now()).unwrap_err();
        assert_eq!(error.code(), "import.duplicate_header");
    }

    #[test]
    fn a_missing_mapped_column_names_what_is_actually_there() {
        let csv = "when,kilowatts\n2024-01-01T00:00:00-05:00,10\n";
        let error = import(csv.as_bytes(), &request(GapPolicy::Reject), now()).unwrap_err();
        assert_eq!(error.code(), "import.missing_column");
        assert!(error.to_string().contains("when, kilowatts"), "{error}");
    }

    #[test]
    fn an_empty_or_headerless_file_is_rejected() {
        assert_eq!(
            import(b"", &request(GapPolicy::Reject), now())
                .unwrap_err()
                .code(),
            "import.empty_file"
        );
        assert_eq!(
            import(b"timestamp,load_kw\n", &request(GapPolicy::Reject), now())
                .unwrap_err()
                .code(),
            "import.no_data_rows"
        );
    }

    #[test]
    fn non_utf8_bytes_are_rejected_rather_than_mis_decoded() {
        let mut bytes = b"timestamp,load_kw\n".to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe, 0x00]);
        let error = import(&bytes, &request(GapPolicy::Reject), now()).unwrap_err();
        assert_eq!(error.code(), "import.not_utf8");
    }

    #[test]
    fn ragged_rows_are_rejected() {
        let csv = "timestamp,load_kw\n2024-01-01T00:00:00-05:00,10,extra\n";
        let error = import(csv.as_bytes(), &request(GapPolicy::Reject), now()).unwrap_err();
        // The csv crate catches the field-count mismatch first; either code is a
        // correct rejection, and both name the line.
        assert!(
            matches!(error.code(), "import.row_length" | "import.malformed_csv"),
            "{}",
            error.code()
        );
    }

    #[test]
    fn dst_ambiguity_in_naive_local_data_is_rejected() {
        // 01:30 on 2024-11-03 happens twice in America/New_York.
        let csv = "timestamp,load_kw\n2024-11-03T01:00:00,10\n2024-11-03T01:30:00,20\n";
        let mut request = request(GapPolicy::Reject);
        request.interval_minutes = 30;
        let error = import(csv.as_bytes(), &request, now()).unwrap_err();
        assert_eq!(error.code(), "import.ambiguous_local_time");

        // The same instants with explicit offsets are unambiguous and import fine.
        let csv = "timestamp,load_kw\n2024-11-03T01:00:00-04:00,10\n2024-11-03T01:30:00-04:00,20\n";
        assert!(import(csv.as_bytes(), &request, now()).is_ok());
    }

    #[test]
    fn a_nonexistent_local_time_is_rejected() {
        let csv = "timestamp,load_kw\n2024-03-10T02:00:00,10\n";
        let error = import(csv.as_bytes(), &request(GapPolicy::Reject), now()).unwrap_err();
        assert_eq!(error.code(), "import.nonexistent_local_time");
    }

    #[test]
    fn hostile_filenames_are_rejected_before_anything_is_read() {
        let csv = "timestamp,load_kw\n2024-01-01T00:00:00-05:00,10\n";
        for filename in [
            "../../evil.csv",
            "NUL",
            "a\u{202E}b.csv",
            "C:\\Windows\\x.csv",
        ] {
            let mut request = request(GapPolicy::Reject);
            request.filename = filename.to_string();
            let error = import(csv.as_bytes(), &request, now()).unwrap_err();
            assert!(
                error.code().starts_with("path."),
                "{filename} -> {}",
                error.code()
            );
        }
    }

    #[test]
    fn demand_period_labels_are_interned_and_validated() {
        let csv = "timestamp,load_kw,period\n2024-01-01T00:00:00-05:00,10,off-peak\n\
                   2024-01-01T01:00:00-05:00,20,on-peak\n2024-01-01T02:00:00-05:00,30,off-peak\n";
        let mut request = request(GapPolicy::Reject);
        request.mapping.demand_period = Some("period".to_string());
        let report = import(csv.as_bytes(), &request, now()).unwrap();

        let periods = report.dataset.demand_periods.as_ref().unwrap();
        assert_eq!(periods.labels, vec!["off-peak", "on-peak"]);
        assert_eq!(periods.index, vec![0, 1, 0]);

        // A label with a semicolon is not a label.
        let csv = "timestamp,load_kw,period\n2024-01-01T00:00:00-05:00,10,a;b\n";
        let error = import(csv.as_bytes(), &request, now()).unwrap_err();
        assert_eq!(error.code(), "import.demand_period_charset");
    }

    #[test]
    fn negative_import_prices_are_allowed_because_markets_have_them() {
        let csv = "timestamp,load_kw,price\n2024-01-01T00:00:00-05:00,10,-0.02\n";
        let mut request = request(GapPolicy::Reject);
        request.mapping.series.insert(
            SeriesKind::ImportPrice,
            MappedColumn {
                header: "price".to_string(),
                unit: ImportUnit::UsdPerKilowattHour,
            },
        );
        let report = import(csv.as_bytes(), &request, now()).unwrap();
        assert!((report.dataset.series[&SeriesKind::ImportPrice].values[0] + 0.02).abs() < 1e-12);
    }

    fn suggest(csv: &str) -> MappingSuggestion {
        suggest_mapping(&preview(csv.as_bytes()).unwrap())
    }

    fn suggested<'a>(found: &'a MappingSuggestion, header: &str) -> Option<&'a ColumnSuggestion> {
        found.columns.iter().find(|c| c.header == header)
    }

    #[test]
    fn suggestions_read_the_bundled_sample_correctly_and_flag_the_currency() {
        let csv = "timestamp,load_kw,critical_load_kw,pv_kw,import_price_per_kwh,export_price_per_kwh,demand_period,ambient_c\n\
                   2024-01-01T00:00:00-05:00,52.67,16.69,0.00,0.0980,0.0410,off-peak,-2.2\n\
                   2024-01-01T01:00:00-05:00,53.51,18.81,0.00,0.0980,0.0410,off-peak,-4.4\n";
        let found = suggest(csv);
        let role = |h: &str| suggested(&found, h).unwrap().role.as_str();
        let unit = |h: &str| suggested(&found, h).unwrap().unit;

        assert_eq!(role("timestamp"), "timestamp");
        assert_eq!(role("load_kw"), "load");
        assert_eq!(unit("load_kw"), Some(ImportUnit::Kilowatt));
        assert_eq!(role("critical_load_kw"), "critical_load");
        assert_eq!(role("pv_kw"), "generation");
        assert_eq!(role("import_price_per_kwh"), "import_price");
        assert_eq!(role("export_price_per_kwh"), "export_price");
        assert_eq!(role("demand_period"), "demand_period");
        assert_eq!(role("ambient_c"), "temperature");
        assert_eq!(unit("ambient_c"), Some(ImportUnit::Celsius));
        assert_eq!(found.interval_minutes, Some(60));
        assert!(found.notes.is_empty(), "{:?}", found.notes);

        // No currency in the header: the suggestion must say it read the number's size.
        let price = suggested(&found, "import_price_per_kwh").unwrap();
        assert_eq!(price.unit, Some(ImportUnit::UsdPerKilowattHour));
        assert!(price.check.as_deref().unwrap().contains("no currency"));
    }

    #[test]
    fn the_csv_template_is_recognized_in_full() {
        let found = suggest(TEMPLATE_CSV);
        assert!(found.notes.is_empty(), "{:?}", found.notes);
        assert_eq!(found.columns.len(), 8);
    }

    /// Every "should work" header of the review that names a role and a unit, and more of the
    /// same kind. None of these may stop working while the rule below gets stricter.
    #[test]
    fn realistic_good_headers_are_recognized() {
        use ImportUnit::{
            Celsius, CentsPerKilowattHour as Cents, Fahrenheit, Kilowatt as Kw,
            KilowattHourPerInterval as Kwh, Megawatt as Mw, UsdPerKilowattHour as Usd,
            UsdPerMegawattHour as UsdMwh, Watt as W, WattHourPerInterval as Wh,
        };
        for (header, role, unit) in [
            ("Load (kW)", "load", Kw),
            ("Load kW", "load", Kw),
            ("load_kw", "load", Kw),
            ("LOAD_KW", "load", Kw),
            ("LOADKW", "load", Kw),
            ("loadKw", "load", Kw),
            ("kWLoad", "load", Kw),
            ("LoadkW", "load", Kw),
            ("Site Load (kW)", "load", Kw),
            ("SiteLoad_kWh", "load", Kwh),
            ("Total Load (kW)", "load", Kw),
            ("Building Demand (kW)", "load", Kw),
            ("Average Demand (kW)", "load", Kw),
            ("Demand kW (15 min)", "load", Kw),
            ("demand_kw_15min", "load", Kw),
            ("Electricity Usage (kWh)", "load", Kwh),
            ("Usage (kWh)", "load", Kwh),
            ("USAGE (kWh)", "load", Kwh),
            ("Consumption (kWh)", "load", Kwh),
            ("Consumption kWh", "load", Kwh),
            ("Avg Load kW", "load", Kw),
            ("load_kW_avg", "load", Kw),
            ("kWhLoad", "load", Kwh),
            ("loadkWh", "load", Kwh),
            ("load_kwh_per_interval", "load", Kwh),
            ("load kW-hr", "load", Kwh),
            ("load_kilowatt_hours", "load", Kwh),
            ("load_w", "load", W),
            ("load Wh", "load", Wh),
            ("Load (MW)", "load", Mw),
            ("PV Power (kW)", "generation", Kw),
            ("pv_kw", "generation", Kw),
            ("PV AC Power (kW)", "generation", Kw),
            ("Solar Generation kWh", "generation", Kwh),
            ("Solar (kW)", "generation", Kw),
            ("PV Production (kWh)", "generation", Kwh),
            ("Solar Production kWh", "generation", Kwh),
            ("wind_generation_kw", "generation", Kw),
            ("Critical Load (kW)", "critical_load", Kw),
            ("critical_load_w", "critical_load", W),
            ("Essential Load kW", "critical_load", Kw),
            ("Temperature (°C)", "temperature", Celsius),
            ("Temp °F", "temperature", Fahrenheit),
            ("temp_F", "temperature", Fahrenheit),
            ("Ambient Temp C", "temperature", Celsius),
            ("OAT F", "temperature", Fahrenheit),
            ("Dry Bulb (F)", "temperature", Fahrenheit),
            ("Outdoor Air Temperature (C)", "temperature", Celsius),
            ("Import Price (USD/kWh)", "import_price", Usd),
            ("import_price_usd_per_kwh", "import_price", Usd),
            ("import price US$/kWh", "import_price", Usd),
            ("Energy Price ($/kWh)", "import_price", Usd),
            ("import price cents/kWh", "import_price", Cents),
            ("import price ¢/kWh", "import_price", Cents),
            ("Cost_per_kWh", "import_price", Usd),
            ("LMP ($/MWh)", "import_price", UsdMwh),
            ("RT_LMP_$/MWh", "import_price", UsdMwh),
            ("price usd_per_mwh", "import_price", UsdMwh),
            ("Export Credit ($/kWh)", "export_price", Usd),
            ("export_price_per_kwh", "export_price", Usd),
        ] {
            let found = suggest(&format!("{header}\n0.5\n"));
            let column = suggested(&found, header)
                .unwrap_or_else(|| panic!("{header} got nothing: {:?}", found.notes));
            assert_eq!(column.role, role, "{header}");
            assert_eq!(column.unit, Some(unit), "{header}");
        }

        // What a suggestion still has to say out loud.
        for (header, word) in [
            ("Building Demand (kW)", "billing"),
            ("load mw", "milliwatts"),
            ("Energy Price ($/kWh)", "import or export"),
            ("Energy Price ($/kWh)", "read as US dollars"),
            ("rate_per_kwh", "no currency"),
            ("rate_per_kwh", "import or export"),
        ] {
            let found = suggest(&format!("{header}\n9.8\n"));
            let check = suggested(&found, header).unwrap().check.clone().unwrap();
            assert!(check.contains(word), "{header}: {check}");
        }
        assert_eq!(
            suggested(&suggest("rate_per_kwh\n9.8\n"), "rate_per_kwh")
                .unwrap()
                .unit,
            Some(Cents),
            "read by size, and it says so"
        );
        let found = suggest("Import Price (USD/kWh)\n0.1\n");
        assert_eq!(
            suggested(&found, "Import Price (USD/kWh)").unwrap().check,
            None
        );
    }

    /// Every other header in the second review's table and its log, plus the first review's.
    /// None of these names what the column is, so each gets no role, and a note naming it.
    #[test]
    fn a_header_that_is_not_recognized_in_full_gets_no_role_and_says_why() {
        for header in [
            // Not positively a role.
            "Energy (kWh)",
            "kWh",
            "KWH",
            "kW",
            "Power (kW)",
            "Active Power (kW)",
            "Real Power kW",
            "Interval kWh",
            "Avg kW",
            "Production (kWh)",
            "Inverter Output kW",
            "notes",
            // Daily and running totals.
            "PV_Energy_Today_kWh",
            "Solar Energy Today (kWh)",
            "Daily_Yield_PV_kWh",
            "Solar_Yield_Today_kWh",
            "PV Daily Energy (kWh)",
            "Generation_Today_kWh",
            "Load_kWh_Today",
            "Daily_Load_kWh",
            "Load_kWh_Counter",
            "Load_Energy_Index_kWh",
            "Load_kWh_Reg",
            "PV_E_Day_kWh",
            "E_Total_PV_kWh",
            "PV_Etotal_kWh",
            "PV_Lifetime_Energy_kWh",
            "Solar kWh Since Install",
            "load_kwh_cumulative",
            "meter_reading_kwh",
            "total_load_kwh_ytd",
            "total_load_kwh",
            // Not power.
            "Solar_Irradiance_W/m2",
            "POA_Irradiance_W_m2",
            "Solar_Radiation_W/m2",
            "PV_Voltage_V",
            "PV_Current_A",
            "Wind_Speed_m/s",
            "Wind_Speed_mph",
            "Wind_Direction",
            "Load_A",
            "Load_Amps",
            "Load_Current_A",
            "Load_Voltage_V",
            "Load_V",
            "Load_PF",
            "Load_Hz",
            "Load_Power_Factor",
            "Demand_Response_Flag",
            "load_factor",
            "load %",
            // The wrong temperature, and no unit.
            "Battery_Temp_C",
            "Inverter_Temp_C",
            "PV_Module_Temp_C",
            "Transformer_Temp_F",
            "Coolant_Temp_C",
            "Room_Temp_F",
            "temperature",
            "temp_K",
            // Not the load or the output.
            "Non_Critical_Load_kW",
            "NonCriticalLoad_kW",
            "Noncritical_Load_kW",
            "Non-Essential Load (kW)",
            "Load_Limit_kW",
            "Demand_Limit_kW",
            "Demand_Setpoint_kW",
            "Load_Setpoint_kW",
            "Contract_Demand_kW",
            "Billing_Demand_kW",
            "Ratchet_Demand_kW",
            "Load_Shed_kW",
            "Shed_Load_kW",
            "Unserved_Load_kW",
            "Load_Reduction_kW",
            "Load_Delta_kW",
            "Rated_Load_kW",
            "Connected_Load_kW",
            "Load_kW_L1",
            "Load_kW_PhaseA",
            "PV_Capacity_kW",
            "PV_Nameplate_kW",
            "Installed_PV_kW",
            "PV_Curtailed_kW",
            "Curtailed_Solar_kW",
            "Load_Less_PV_kW",
            "Load_kWh/h",
            "Load (kWh/h)",
            "PV_kWh/h",
            "Generation_kWp",
            "Load_kWh_per_day",
            "Load_kWh_month",
            "kWh/m2 Solar",
            "Solar_kWh/m2",
            "peak_demand_kw",
            "min_load_kw",
            "forecast_load_kw",
            "backup_generator_kw",
            "backup_battery_kw",
            "backup_load_kw",
            "generator_kw",
            "standby_diesel_gen_kw",
            "grid_import_kw",
            "load_export_kw",
            "Load_kWhDelivered",
            "net_load_kw",
            "peakWindow_load",
            // A unit Tonans cannot read, or none.
            "Load",
            "load",
            "MWhLoad",
            "load_MWh",
            "load_mW",
            "load_kVA",
            "load_kvar",
            "load GW",
            "load GWh",
            "load kW kWh",
            "load kW/h",
            // Money that is not a price per kWh in a currency Tonans reads.
            "Price (A$/kWh)",
            "Price (AU$/kWh)",
            "Price (R$/kWh)",
            "Price (C$/kWh)",
            "Price (NZ$/kWh)",
            "Price (HK$/kWh)",
            "Price_S$_per_kWh",
            "Price_BRL_per_kWh",
            "Price_ZAR/kWh",
            "Tariff_PLN_kWh",
            "Price_SGD_per_kWh",
            "Price_HKD_per_kWh",
            "Price_ZL_per_kWh",
            "Tariff_Rs_per_kWh",
            "Price_kr_per_kWh",
            "Tarif_Fr_kWh",
            "Price_NIS_per_kWh",
            "Price_AED_per_kWh",
            "price_eur_per_kwh",
            "price_p_per_kwh",
            "price_gbp_per_mwh",
            "EUR/kWh",
            "€/kWh",
            "price £/MWh",
            "import price (CAD/kWh)",
            "tariff_euro_c_per_kwh",
            "COST",
            "Interval_Cost_USD",
            "Cost (USD)",
            "Bill_Amount_USD",
            "Energy_Cost_USD_kWh",
            "Cost_kWh",
            "Energy_Cost_kWh",
            "Rate_Schedule",
            "Price_per_100kWh",
            "Price (USD/100 kWh)",
            "Price (USD/kWh) incl tax",
            "Price_mills_per_kWh",
            "Price (mills/kWh)",
            "Rate_mil_kWh",
            "Price_USD/GWh",
            "Price (USD/kW-month)",
            "Credit_kWh",
            "Export_kWh_Credit",
            "Solar_Credit_kWh",
            "Charge_Rate_kW",
            "Discharge_Rate_kW",
            "price_mwh",
            "price_usd",
            "price_usd_per_kw",
            "demand_rate_usd_per_kw",
            "demand_charge_usd",
            "price_cents_per_mwh",
            "price_per_100_kwh",
            // Capitals inside a word.
            "BACKWARD_LOAD",
            "FARMWIND_POWER",
            "LOWHEAD_LOAD",
        ] {
            let found = suggest(&format!("{header}\n1\n"));
            assert!(
                found.columns.is_empty(),
                "{header} was suggested as {:?}",
                found.columns
            );
            assert_eq!(found.notes.len(), 1, "{header}: {:?}", found.notes);
            assert!(
                found.notes[0].contains(header.trim()),
                "{header}: {:?}",
                found.notes
            );
        }
    }

    #[test]
    fn the_opposite_of_a_critical_load_is_never_called_one() {
        for header in [
            "Non_Critical_Load_kW",
            "NonCriticalLoad_kW",
            "Non-Essential Load (kW)",
        ] {
            let found = suggest(&format!("{header}\n1\n"));
            assert!(
                !found.notes[0].contains("a critical load"),
                "{:?}",
                found.notes
            );
            assert!(
                found.notes[0].contains("is not recognized"),
                "{:?}",
                found.notes
            );
        }
    }

    #[test]
    fn a_foreign_currency_is_named_or_called_unrecognized_and_never_no_currency() {
        for (header, says) in [
            ("Price (A$/kWh)", "AUD"),
            ("Price (AU$/kWh)", "AUD"),
            ("Price (R$/kWh)", "BRL"),
            ("Price (C$/kWh)", "CAD"),
            ("Price (NZ$/kWh)", "NZD"),
            ("Price (HK$/kWh)", "HKD"),
            ("Price_S$_per_kWh", "SGD"),
            ("Price_BRL_per_kWh", "BRL"),
            ("Price_ZAR/kWh", "ZAR"),
            ("Tariff_PLN_kWh", "PLN"),
            ("price_eur_per_kwh", "EUR"),
            ("€/kWh", "EUR"),
            ("price £/MWh", "GBP"),
            ("price_p_per_kwh", "pence"),
            ("Price_ZL_per_kWh", "PLN"),
            ("Price (XX$/kWh)", "not recognized"),
            ("Price_QQQ_per_kWh", "not recognized"),
        ] {
            let found = suggest(&format!("{header}\n0.12\n"));
            assert!(found.columns.is_empty(), "{header}: {:?}", found.columns);
            let note = &found.notes[0];
            assert!(note.contains(says), "{header}: {note}");
            assert!(note.contains("US dollars"), "{header}: {note}");
            assert!(!note.contains("gives no currency"), "{header}: {note}");
        }
    }

    #[test]
    fn good_headers_in_everyday_words_get_a_role_and_a_unit() {
        use ImportUnit::*;
        for (header, role, unit) in [
            ("P_Load_kW", "load", Kilowatt),
            ("Load_P_kW", "load", Kilowatt),
            ("Load in kW", "load", Kilowatt),
            ("Load (in kW)", "load", Kilowatt),
            ("Load Profile (kW)", "load", Kilowatt),
            ("Bldg Load (kW)", "load", Kilowatt),
            ("Campus Load (kW)", "load", Kilowatt),
            ("Feeder Load (kW)", "load", Kilowatt),
            ("Transformer Load (kW)", "load", Kilowatt),
            ("Bus_Load_kW", "load", Kilowatt),
            ("Household Load (kW)", "load", Kilowatt),
            ("Home Consumption (kWh)", "load", KilowattHourPerInterval),
            ("House Consumption (kWh)", "load", KilowattHourPerInterval),
            ("Consumption in kWh", "load", KilowattHourPerInterval),
            ("Load kW-h", "load", KilowattHourPerInterval),
            ("Load (kW h)", "load", KilowattHourPerInterval),
            ("Load (kW·h)", "load", KilowattHourPerInterval),
            ("Load (kW.h)", "load", KilowattHourPerInterval),
            ("Load (W-h)", "load", WattHourPerInterval),
            ("Load (㎾)", "load", Kilowatt),
            ("Load (㎾h)", "load", KilowattHourPerInterval),
            ("Critical Power (kW)", "critical_load", Kilowatt),
            ("P_PV_kW", "generation", Kilowatt),
            ("PV Yield (kWh)", "generation", KilowattHourPerInterval),
            ("Gen_MW", "generation", Megawatt),
            ("GEN_MW", "generation", Megawatt),
            ("Temp (ºC)", "temperature", Celsius),
            ("Temperature (ºF)", "temperature", Fahrenheit),
            ("Temp [℃]", "temperature", Celsius),
            ("Temp (℉)", "temperature", Fahrenheit),
            ("Temp in C", "temperature", Celsius),
            ("Temperature (Centigrade)", "temperature", Celsius),
            ("OA Temp (°F)", "temperature", Fahrenheit),
            ("AmbTemp_C", "temperature", Celsius),
            ("Spot Price ($/MWh)", "import_price", UsdPerMegawattHour),
            ("Wholesale Price ($/MWh)", "import_price", UsdPerMegawattHour),
            ("Market Price ($/MWh)", "import_price", UsdPerMegawattHour),
            ("LBMP ($/MWHr)", "import_price", UsdPerMegawattHour),
            ("TOU Rate ($/kWh)", "import_price", UsdPerKilowattHour),
            ("Hourly Price ($/kWh)", "import_price", UsdPerKilowattHour),
            ("Interval Price ($/kWh)", "import_price", UsdPerKilowattHour),
            ("Period Price ($/kWh)", "import_price", UsdPerKilowattHour),
            ("Usage Cost ($/kWh)", "import_price", UsdPerKilowattHour),
            ("Net Metering Credit ($/kWh)", "export_price", UsdPerKilowattHour),
        ] {
            let found = suggest(&format!("{header}\n1\n"));
            let column = suggested(&found, header).unwrap_or_else(|| panic!("{header}: {found:?}"));
            assert_eq!(column.role, role, "{header}");
            assert_eq!(column.unit, Some(unit), "{header}");
        }
    }

    #[test]
    fn cents_written_short_are_read_beside_a_kwh_and_say_so() {
        for (header, role, word) in [
            ("Retail Price (c/kWh)", "import_price", "'c'"),
            ("Tariff (c/kWh)", "import_price", "'c'"),
            ("Price (ct/kWh)", "import_price", "'ct'"),
            ("tariff_c_per_kwh", "import_price", "'c'"),
            ("c/kWh", "import_price", "'c'"),
            ("Export Credit (c/kWh)", "export_price", "'c'"),
            ("Export Tariff (c/kWh)", "export_price", "'c'"),
            ("Feed-in Tariff (c/kWh)", "export_price", "'c'"),
        ] {
            let found = suggest(&format!("{header}\n8.5\n"));
            let column = suggested(&found, header).unwrap_or_else(|| panic!("{header}: {found:?}"));
            assert_eq!(column.role, role, "{header}");
            assert_eq!(column.unit, Some(ImportUnit::CentsPerKilowattHour), "{header}");
            assert!(
                column.check.as_deref().unwrap().contains(&format!("{word} is read as US cents")),
                "{header}: {:?}",
                column.check
            );
        }
        // Not beside a kWh, or with no `/` or `per` between, it is an unknown word.
        for header in ["Price c", "Price C kWh", "Price (c/MWh)", "c kWh"] {
            let found = suggest(&format!("{header}\n8.5\n"));
            assert!(found.columns.is_empty(), "{header}: {found:?}");
        }
    }

    #[test]
    fn a_new_word_does_not_open_the_door_to_what_it_sits_beside() {
        for header in [
            // Part of a load, a load that is not electric, a forecast, or a limit.
            "HVAC Load (kW)",
            "Lighting Load (kW)",
            "EV Load (kW)",
            "Gas Usage (kWh)",
            "Load Forecast (kW)",
            "Generation Forecast (kW)",
            "Baseline Load (kW)",
            "Base Load (kW)",
            "Critical Load Limit (kW)",
            "Non Critical Power (kW)",
            "Inverter Load (kW)",
            // Energy used on site, not what the site uses.
            "Self Consumption (kWh)",
            // Capacity, not output; a generator's parts; not one number.
            "PV kWp",
            "PV kW(p)",
            "Solar Array Capacity kW",
            "Diesel Gen kW",
            "Gen Load (kW)",
            // Indoors, not one reading, or an outlet.
            "Zone Temp (°F)",
            "Max Temp (C)",
            "Temp_Out_C",
            // A rate of change, and words `in` must not excuse.
            "load kW/h",
            "Load in Building",
            "Load in kW Total Hours",
            // Words that are prices only beside a price word.
            "Spot kW",
            // Money that is not an energy price.
            "On-Peak Rate ($/kWh)",
            "Solar Buyback Rate ($/kWh)",
            "Avoided Cost ($/MWh)",
            "Net Price ($/kWh)",
            "Discharge Rate kWh",
            "Spot Price",
            "Usage Cost",
        ] {
            let found = suggest(&format!("{header}\n1\n"));
            assert!(found.columns.is_empty(), "{header}: {found:?}");
        }
    }

    #[test]
    fn gen_is_generation_and_asks_to_be_checked() {
        let found = suggest("Gen_MW\n1\n");
        let column = suggested(&found, "Gen_MW").unwrap();
        assert!(column.check.as_deref().unwrap().contains("'Gen' is read"));
        let found = suggest("Generation (MW)\n1\n");
        let column = suggested(&found, "Generation (MW)").unwrap();
        assert_eq!(column.check, None);
    }

    #[test]
    fn a_price_naming_both_directions_is_refused() {
        for header in [
            "Import/Export Price ($/kWh)",
            "Import Credit ($/kWh)",
            "Buy/Sell Rate (USD/kWh)",
            "Retail Export Rate ($/kWh)",
        ] {
            let found = suggest(&format!("{header}\n0.08\n"));
            assert!(found.columns.is_empty(), "{header}: {found:?}");
            assert!(found.notes[0].contains("both import and export"), "{found:?}");
        }
        for (header, role) in [
            ("Import Price ($/kWh)", "import_price"),
            ("Export Credit ($/kWh)", "export_price"),
        ] {
            let found = suggest(&format!("{header}\n0.08\n"));
            assert_eq!(suggested(&found, header).unwrap().role, role);
        }
    }

    #[test]
    fn every_spelling_of_a_feed_in_tariff_is_an_export_credit() {
        for header in [
            "Feed-in Tariff ($/kWh)",
            "Feed In Tariff ($/kWh)",
            "FeedIn Tariff ($/kWh)",
            "FeedInTariff ($/kWh)",
            "Feedin Tariff ($/kWh)",
            "feedintariff_usd_per_kwh",
            "feed_in_tariff_usd_per_kwh",
            "FEED-IN TARIFF (USD/KWH)",
            "Feed-in Rate ($/kWh)",
            "Feed-in Credit ($/kWh)",
            "Feed-in Price ($/kWh)",
            "FiT ($/kWh)",
            "FIT (USD/kWh)",
            "fit_usd_per_kwh",
        ] {
            let found = suggest(&format!("{header}\n0.08\n"));
            let column = suggested(&found, header).unwrap_or_else(|| panic!("{header}: {found:?}"));
            assert_eq!(column.role, "export_price", "{header}");
            assert_eq!(column.unit, Some(ImportUnit::UsdPerKilowattHour), "{header}");
        }
        // `in` is a word only after `feed`.
        let found = suggest("Price in Tariff ($/kWh)\n0.08\n");
        assert!(found.columns.is_empty(), "{found:?}");
        // And `fit` needs money beside it, as any other word for a direction does.
        let found = suggest("Fit (kW)\n5\n");
        assert!(found.columns.is_empty(), "{found:?}");
    }

    #[test]
    fn a_price_with_no_currency_is_read_from_all_its_nonzero_values() {
        const HEADER: &str = "Export Credit per kWh";
        let read = |values: &[&str]| {
            let found = suggest(&format!("{HEADER}\n{}\n", values.join("\n")));
            let column = suggested(&found, HEADER).unwrap().clone();
            assert_eq!(column.role, "export_price", "{values:?}");
            (column.unit, column.check.unwrap())
        };
        // Cents, whatever comes first: a zero overnight, or a first value small enough for dollars.
        for values in [
            &["0", "0", "8.5", "8.5", "0"][..],
            &["1.8", "7.5", "12"],
            &["0", "1.8", "8.5"],
        ] {
            let (unit, check) = read(values);
            assert_eq!(unit, Some(ImportUnit::CentsPerKilowattHour), "{values:?}");
            assert!(check.contains("read as cents"), "{check}");
        }
        for values in [&["0.12", "0.08"][..], &["0", "0.1"], &["-0.02", "0.3"]] {
            let (unit, check) = read(values);
            assert_eq!(unit, Some(ImportUnit::UsdPerKilowattHour), "{values:?}");
            assert!(check.contains("read as dollars"), "{check}");
        }
        // Values that fit neither, or both, get no unit, and it says so.
        for values in [&["1.8", "1.8"][..], &["0.12", "8.5"], &["0.5", "1.5"]] {
            let (unit, check) = read(values);
            assert_eq!(unit, None, "{values:?}");
            assert!(check.contains("do not clearly fit"), "{check}");
            assert!(check.contains(values[0]), "{check}");
        }
        let (unit, check) = read(&["0", "0", "0"]);
        assert_eq!(unit, None);
        assert!(check.contains("no value other than zero"), "{check}");
    }

    #[test]
    fn no_currency_is_only_said_when_the_header_truly_has_none() {
        let found = suggest("import_price_per_kwh\n0.1\n");
        let check = suggested(&found, "import_price_per_kwh")
            .unwrap()
            .check
            .clone();
        assert!(check.unwrap().contains("gives no currency"));
        for header in [
            "Import Price (USD/kWh)",
            "import price US$/kWh",
            "import price cents/kWh",
        ] {
            let found = suggest(&format!("{header}\n0.1\n"));
            let column = suggested(&found, header).unwrap();
            assert!(
                !column
                    .check
                    .clone()
                    .unwrap_or_default()
                    .contains("no currency"),
                "{header}"
            );
        }
    }

    #[test]
    fn a_note_says_why_in_the_words_that_apply() {
        let note = |header: &str| suggest(&format!("{header}\n1\n")).notes[0].clone();
        assert!(note("Load").contains("does not state a unit"));
        assert!(note("load_MWh").contains("states MWh"));
        assert!(note("load_mW").contains("milliwatts"));
        assert!(note("load kW kWh").contains("more than one unit"));
        assert!(note("Load_Setpoint_kW").contains("'Setpoint'"));
        assert!(note("Load_kWh/h").contains("'h' in"));
        assert!(note("Solar_Irradiance_W/m2").contains("'Irradiance'"));
        assert!(note("BACKWARD_LOAD").contains("'BACKWARD'"));
        assert!(note("total_load_kwh").contains("running total"));
        assert!(note("net_load_kw").contains("twice"));
        assert!(note("Battery_Temp_C").contains("'Battery'"));
        assert!(note("temp_K").contains("Kelvin"));
        assert!(note("price_usd").contains("per what"));
        assert!(note("Credit_kWh").contains("amount rather than a price"));
        assert!(note("demand_charge_usd").contains("Demand charges go on the tariff"));
        assert!(note("Load_Less_PV_kW").contains("both load and generation"));
        assert!(note("Power (kW)").contains("Nothing in"));
    }

    #[test]
    fn values_decide_what_a_header_cannot() {
        // A column of words cannot be a load, a date is not a timestamp the importer takes, and
        // two good headers for one role are the user's to choose between.
        assert!(suggest("load_kw\noffice\n").columns.is_empty());
        assert!(suggest("import_price_usd_per_kwh\nhigh\n")
            .columns
            .is_empty());
        let found = suggest("date,load_kw\n2024-01-01,1\n");
        assert!(suggested(&found, "date").is_none());
        let found = suggest("Timestamp,Load kW,Site Demand kWh\n2024-01-01 00:00,2,3\n");
        assert_eq!(found.columns.len(), 1, "{found:?}");
        assert!(found
            .notes
            .iter()
            .any(|n| n.contains("'Load kW' and 'Site Demand kWh'")));
    }

    #[test]
    fn a_column_that_only_rises_is_flagged_as_a_possible_running_total() {
        let csv = "load_kwh,pv_kw\n100,1\n110,2\n120,3\n";
        let found = suggest(csv);
        let rising = suggested(&found, "load_kwh").unwrap();
        assert!(
            rising.check.as_deref().unwrap().contains("only rise"),
            "{rising:?}"
        );
        // Power may rise over three intervals: that says nothing about a running total.
        assert_eq!(suggested(&found, "pv_kw").unwrap().check, None);

        let found = suggest("load_kwh\n100\n90\n120\n");
        assert_eq!(suggested(&found, "load_kwh").unwrap().check, None);
        let found = suggest("total_load_kw\n100\n");
        assert_eq!(suggested(&found, "total_load_kw").unwrap().check, None);
    }

    #[test]
    fn the_interval_hint_comes_from_the_timestamps_and_refuses_an_illegal_spacing() {
        let found = suggest("ts,load_kw\n2024-01-01T00:00:00Z,1\n2024-01-01T00:15:00Z,1\n");
        assert_eq!(found.interval_minutes, Some(15));
        let found = suggest("ts,load_kw\n2024-01-01T00:00:00Z,1\n2024-01-01T02:00:00Z,1\n");
        assert_eq!(
            found.interval_minutes, None,
            "two hours is not a legal interval"
        );
        let found = suggest("ts,load_kw\n2024-01-01T01:00:00Z,1\n2024-01-01T00:00:00Z,1\n");
        assert_eq!(found.interval_minutes, None, "backwards is not an interval");
    }

    #[test]
    fn preview_reads_the_header_without_interpreting_it() {
        let csv = "timestamp,load_kw,junk\n2024-01-01T00:00:00-05:00,10,x\n2024-01-01T01:00:00-05:00,20,y\n";
        let preview = preview(csv.as_bytes()).unwrap();
        assert_eq!(preview.headers, vec!["timestamp", "load_kw", "junk"]);
        assert_eq!(preview.row_count, 2);
        assert_eq!(preview.sample_rows.len(), 2);
        assert_eq!(
            preview.sample_rows[0],
            vec!["2024-01-01T00:00:00-05:00", "10", "x"]
        );
        assert!(preview.sha256.as_str().starts_with("sha256:"));
    }

    #[test]
    fn an_oversized_file_is_refused_without_being_parsed() {
        let oversized = vec![b'a'; limits::MAX_CSV_BYTES + 1];
        let error = import(&oversized, &request(GapPolicy::Reject), now()).unwrap_err();
        assert_eq!(error.code(), "limit.csv_bytes");
        assert_eq!(error.http_status(), 413);
    }

    #[test]
    fn too_many_columns_are_refused() {
        let headers: Vec<String> = (0..=limits::MAX_CSV_COLUMNS)
            .map(|i| format!("c{i}"))
            .collect();
        let csv = format!("{}\n", headers.join(","));
        let error = import(csv.as_bytes(), &request(GapPolicy::Reject), now()).unwrap_err();
        assert_eq!(error.code(), "limit.csv_columns");
    }

    #[test]
    fn fahrenheit_converts_to_celsius() {
        assert!((ImportUnit::Fahrenheit.to_canonical(32.0, 1.0) - 0.0).abs() < 1e-12);
        assert!((ImportUnit::Fahrenheit.to_canonical(212.0, 1.0) - 100.0).abs() < 1e-12);
        assert!((ImportUnit::Fahrenheit.to_canonical(68.0, 1.0) - 20.0).abs() < 1e-12);
    }

    #[test]
    fn price_units_convert() {
        assert!((ImportUnit::CentsPerKilowattHour.to_canonical(15.0, 1.0) - 0.15).abs() < 1e-12);
        assert!((ImportUnit::UsdPerMegawattHour.to_canonical(50.0, 1.0) - 0.05).abs() < 1e-12);
    }
}
