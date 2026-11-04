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
                 set holds at most {}. A gap policy patches holes in a file; it cannot be larger \
                 than the file.",
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
                 ({pct:.1}%), above the {limit:.0}% ceiling. A file with this many holes is not \
                 a data set. Fix the export at its source.",
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
            "the import price is negative in at least one interval. That is legitimate in a \
             wholesale market, and Tonans accounts for it, but it means importing energy can earn \
             money — so a dispatch policy that avoids import is not necessarily the cheaper one.",
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
                "the file is not valid UTF-8 (byte {}). Re-export it as UTF-8; a mis-decoded file \
                 would silently corrupt values.",
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
                format!(
                    "the header '{header}' appears more than once. Tonans will not guess which one \
                     you meant."
                ),
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
                     one, or list it under ignore_columns. Tonans will not skip a column you have \
                     not accounted for — the one time it matters, it was the column that held the \
                     real data.",
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
                format!(
                    "timestamp {} appears twice in a row. Tonans will not choose one of them.",
                    instant.to_rfc3339()
                ),
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
                    "{missing} interval(s) are missing between {} and {}. Tonans does not \
                     interpolate. Fix the export, or deliberately choose a gap policy and accept \
                     that the filled intervals are not measurements.",
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

fn parse_naive(raw: &str, line: u64) -> Result<NaiveDateTime> {
    const FORMATS: &[&str] = &[
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ];
    for format in FORMATS {
        if let Ok(naive) = NaiveDateTime::parse_from_str(raw, format) {
            return Ok(naive);
        }
    }
    Err(Error::import(
        "import.bad_timestamp",
        Some(line),
        Some("timestamp"),
        format!(
            "'{raw}' is not a timestamp Tonans understands. Use RFC 3339 with an offset \
             (2024-01-01T00:00:00-05:00), which is unambiguous, or one of: {}.",
            FORMATS.join(", ")
        ),
    ))
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
                "a timestamp gap cannot be filled while a demand-period column is mapped. Tonans \
                 has no tariff calendar from which to reconstruct the missing period label; repair \
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
                                "{role} is {value} {unit}, which is negative. Tonans models load \
                                 and generation as separate non-negative series; a net-metered \
                                 column signed both ways cannot be separated back into the two, and \
                                 Tonans will not try.",
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
            format!(
                "column '{header}' is empty on this row. An empty cell is not a zero and not a gap; \
                 Tonans will not decide which you meant."
            ),
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
            format!("'{raw}' is not a finite number. Tonans will not substitute a value for it."),
        ));
    }
    if value.abs() > limits::MAX_ABS_VALUE {
        return Err(Error::import(
            "import.value_out_of_range",
            Some(line),
            Some(header),
            format!(
                "{value} exceeds the magnitude limit of {}. A real site reading never does.",
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
