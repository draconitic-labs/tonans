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
            "'{raw}' is not a timestamp Tonans understands. Use RFC 3339 with an offset \
             (2024-01-01T00:00:00-05:00), which is unambiguous, or one of: {}.",
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
/// the rule that matters is that Tonans never guesses *silently*. A unit is suggested
/// only if the file states it, with one flagged exception: a price per kWh with no
/// currency is read by size as dollars or cents, and says that it was. A header whose
/// unit, currency or meaning is unclear gets no suggestion, or a visible check.
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
                "{} could each be {}. Tonans left them for you to choose.",
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
    if UNIT_WORDS.contains(&raw.to_lowercase().as_str()) {
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
        .replace('₹', " inr ");
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
/// `kW-hr`, and `kWhr` are all `kwh`.
fn header_words(tokens: &[String]) -> Vec<String> {
    let mut words: Vec<String> = Vec::with_capacity(tokens.len());
    for token in tokens {
        let lower = token.to_lowercase();
        if matches!(lower.as_str(), "hr" | "hrs" | "hour" | "hours") {
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

/// Words that mean a running total: the column is not the amount in each interval.
const RUNNING_TOTAL_WORDS: [&str; 15] = [
    "cumulative",
    "cum",
    "cumul",
    "accumulated",
    "accum",
    "totalizer",
    "totaliser",
    "register",
    "reading",
    "readings",
    "lifetime",
    "ytd",
    "mtd",
    "running",
    "odometer",
];

/// Words for a share or a ratio rather than an amount of power or money.
const RATIO_WORDS: [&str; 5] = ["pct", "percent", "percentage", "factor", "ratio"];

const BATTERY_WORDS: [&str; 6] = ["battery", "batt", "bess", "soc", "storage", "ess"];

/// Words for a machine that runs when called on, rather than output that simply arrives.
const STANDBY_WORDS: [&str; 5] = ["backup", "standby", "diesel", "emergency", "genset"];

const FORECAST_WORDS: [&str; 10] = [
    "forecast",
    "forecasted",
    "predicted",
    "prediction",
    "estimate",
    "estimated",
    "expected",
    "modeled",
    "modelled",
    "simulated",
];

/// Words that say a column is money. Spellings such as `pound` and `p` count only once
/// something else has said so, and are in [`OTHER_CURRENCIES`].
const MONEY_WORDS: [&str; 19] = [
    "price", "rate", "tariff", "cost", "credit", "usd", "dollar", "dollars", "cents", "eur",
    "euro", "euros", "gbp", "pence", "jpy", "cad", "aud", "chf", "inr",
];

/// Currencies other than the US dollar, and how to name them to the user.
const OTHER_CURRENCIES: [(&str, &str); 28] = [
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
    ("cny", "CNY"),
    ("rmb", "CNY"),
    ("yuan", "CNY"),
    ("inr", "INR"),
    ("rupee", "INR"),
    ("rupees", "INR"),
    ("sek", "SEK"),
    ("nok", "NOK"),
    ("dkk", "DKK"),
    ("krw", "KRW"),
    ("mxn", "MXN"),
];

/// Read one column. `Err` holds the note that says why nothing is suggested for it.
fn classify_column(
    header: &str,
    samples: &[&str],
) -> std::result::Result<ColumnSuggestion, String> {
    let tokens = header_tokens(header);
    let words = header_words(&tokens);
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

    if has(&RUNNING_TOTAL_WORDS) {
        return Err(format!(
            "'{shown}' looks like a running total or a meter reading, not the amount in each \
             interval. Convert it to the amount per interval, or ignore it."
        ));
    }
    if has(&RATIO_WORDS) || shown.contains('%') {
        return Err(format!(
            "'{shown}' looks like a share or a ratio, not an amount. Choose a meaning only if \
             you know how it converts, or ignore it."
        ));
    }
    // A column of words cannot be power or a price, whatever its header says.
    let not_numbers = !samples.is_empty()
        && samples
            .iter()
            .all(|cell| !cell.is_empty() && cell.parse::<f64>().is_err());

    if has(&MONEY_WORDS) {
        if has(&["demand", "charge", "charges", "fixed"])
            || (!has(&["kwh", "wh", "mwh", "gwh"])
                && has(&["kw", "kilowatt", "mw", "w", "watt", "kva"]))
        {
            // A demand charge is USD per kW, not an energy price. Tonans asks for those on
            // the tariff, not in a column.
            return Err(format!(
                "'{shown}' looks like a demand charge or a price per kW. Tonans reads energy \
                 prices from a file; demand charges go on the tariff."
            ));
        }
        if not_numbers {
            return Err(format!(
                "'{shown}' names a price, but its values are not numbers. Choose a meaning, or \
                 ignore it."
            ));
        }
        let (role, phrase, direction) = if has(&[
            "export", "sell", "sold", "feed", "feedin", "credit",
        ]) {
            ("export_price", "an export credit", None)
        } else if has(&["import", "buy", "purchase", "purchased", "retail"]) {
            ("import_price", "an import price", None)
        } else {
            (
                    "import_price",
                    "a price",
                    Some("It does not say import or export. It is suggested as the import price; check that.".to_string()),
                )
        };
        let (unit, unit_check) = price_unit(&words, samples);
        return Ok(suggestion(
            role,
            unit,
            format!("The header '{shown}' names {phrase}."),
            join_checks([direction, unit_check]),
        ));
    }

    if has(&["period", "tou", "season"]) && samples.iter().all(|cell| cell.parse::<f64>().is_err())
    {
        return Ok(suggestion(
            "demand_period",
            None,
            format!("The header '{shown}' names a period, and its values are labels."),
            None,
        ));
    }
    if has(&["temperature", "temp", "ambient", "oat"]) {
        let (unit, check) = if has(&["kelvin", "k"]) {
            (
                None,
                Some("The header states Kelvin. Tonans reads Celsius or Fahrenheit; convert the column first.".to_string()),
            )
        } else if has(&["c", "celsius", "degc"]) {
            (Some(ImportUnit::Celsius), None)
        } else if has(&["f", "fahrenheit", "degf"]) {
            (Some(ImportUnit::Fahrenheit), None)
        } else {
            (None, None)
        };
        return Ok(suggestion(
            "temperature",
            unit,
            format!(
                "The header '{shown}' names a temperature{}.",
                unit.map_or(String::new(), |unit| format!(", in {}", unit.label()))
            ),
            check,
        ));
    }

    if has(&BATTERY_WORDS) {
        return Err(format!(
            "'{shown}' describes a battery. Tonans models batteries from each scenario, not \
             from a file, so it is left for you to ignore or map."
        ));
    }
    let machine = has(&["generator", "generators", "gen", "genset"]);
    if machine && has(&STANDBY_WORDS) {
        return Err(format!(
            "'{shown}' looks like a backup generator. On-site generation here means output that \
             arrives whether or not it is called on, so it is left for you to map or ignore."
        ));
    }
    if has(&["import", "imported", "export", "exported"]) {
        return Err(format!(
            "'{shown}' looks like power through the grid connection, which is neither the load \
             nor generation on its own. Choose a meaning, or ignore it."
        ));
    }

    let backup_load = has(&["backup"]) && has(&["load"]);
    let (role, phrase) = if has(&["critical", "essential"]) || backup_load {
        ("critical_load", "a critical load")
    } else if machine
        || has(&[
            "pv",
            "solar",
            "photovoltaic",
            "generation",
            "wind",
            "renewable",
            "renewables",
        ])
    {
        ("generation", "on-site generation")
    } else if has(&["load", "consumption", "usage", "demand"]) {
        ("load", "a load")
    } else {
        return Err(format!(
            "Nothing in '{shown}' says what it means. Choose a meaning, or ignore it."
        ));
    };

    let is_limit = words
        .iter()
        .enumerate()
        .any(|(i, word)| match word.as_str() {
            "peak" | "max" | "maximum" | "minimum" | "kwp" => true,
            // `15_min` is a length of time, not a minimum.
            "min" => i == 0 || words[i - 1].parse::<u32>().is_err(),
            _ => false,
        });
    if is_limit {
        return Err(format!(
            "'{shown}' looks like a peak or a limit, not the mean power in each interval. Choose \
             a meaning only if it is that, or ignore it."
        ));
    }
    if not_numbers {
        return Err(format!(
            "'{shown}' names {phrase}, but its values are not numbers. Choose a meaning, or \
             ignore it."
        ));
    }

    let (unit, unit_check) = power_unit(&tokens, &words);
    let energy = matches!(
        unit,
        Some(ImportUnit::KilowattHourPerInterval | ImportUnit::WattHourPerInterval)
    ) || has(&["mwh", "gwh"]);
    let net = has(&["net", "grid", "delivered", "received"]).then(|| {
        "'Net', 'grid', 'delivered' or 'received' may mean generation was already subtracted. \
         Mapping it as load while also mapping generation would count the generation twice."
            .to_string()
    });
    let total = (has(&["total", "totals"]) && (energy || unit.is_none())).then(|| {
        "'Total' may mean a running total rather than the amount in each interval; check that."
            .to_string()
    });
    let rising = (energy && strictly_rising(samples)).then(|| {
        format!(
            "The first {} values only rise. A running meter total would look like this, and \
             must be converted to the amount per interval.",
            samples.len()
        )
    });
    let forecast = has(&FORECAST_WORDS).then(|| {
        "The header says these are forecast or estimated values, not measured ones. The results \
         would describe the forecast."
            .to_string()
    });
    let demand = (role == "load" && has(&["demand"])).then(|| {
        "'Demand' is read as the load. Check it is the mean power in each interval, not a \
         billing figure."
            .to_string()
    });
    let backup = backup_load.then(|| {
        "'Backup' load is read as the critical load, the part that must stay on in an outage; \
         check that."
            .to_string()
    });
    let called_on = machine.then(|| {
        "'Generator' is read as on-site output that arrives regardless. A machine that runs \
         only when called on is not that; check it."
            .to_string()
    });
    let in_unit = unit.map_or(String::new(), |unit| format!(", in {}", unit.label()));
    Ok(suggestion(
        role,
        unit,
        format!("The header '{shown}' names {phrase}{in_unit}."),
        join_checks([
            unit_check, net, total, rising, forecast, demand, backup, called_on,
        ]),
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

/// A power or energy column's unit, and anything about it the user must check.
fn power_unit(tokens: &[String], words: &[String]) -> (Option<ImportUnit>, Option<String>) {
    let has = |options: &[&str]| words.iter().any(|word| options.contains(&word.as_str()));
    let spelled = |spelling: &str| tokens.iter().any(|token| token == spelling);
    if spelled("mW") || spelled("mWh") {
        return (
            None,
            Some(
                "'mW' is milliwatts, which Tonans does not read. If it means megawatts, convert \
                 the column or choose the unit yourself."
                    .to_string(),
            ),
        );
    }
    let unread = [
        (has(&["mwh"]), "MWh"),
        (has(&["gw"]), "GW"),
        (has(&["gwh"]), "GWh"),
        (has(&["kva"]), "kVA"),
        (has(&["kvah"]), "kVAh"),
        (has(&["kvar"]), "kvar"),
        (has(&["kvarh"]), "kvarh"),
    ]
    .into_iter()
    .find_map(|(present, name)| present.then_some(name));
    if let Some(name) = unread {
        return (
            None,
            Some(format!(
                "The header states {name}, which Tonans does not read for this column. Convert \
                 it to kW or kWh first."
            )),
        );
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
        [] => (None, None),
        // A power unit and a time, as in `kW/h`, states neither power nor energy.
        [ImportUnit::Kilowatt | ImportUnit::Watt | ImportUnit::Megawatt]
            if has(&["h", "hr", "hrs", "hour", "hours"]) =>
        {
            (
                None,
                Some(
                    "The header states a power unit and a time, so it could be power or energy. \
                     Choose the unit yourself."
                        .to_string(),
                ),
            )
        }
        [ImportUnit::Megawatt] if spelled("mw") => (
            Some(ImportUnit::Megawatt),
            Some("'mw' is read as megawatts, not milliwatts; check that.".to_string()),
        ),
        [only] => (Some(*only), None),
        // Two units in one header is not a statement of either.
        _ => (
            None,
            Some(
                "The header states more than one unit, so neither is suggested. Choose the \
                 unit yourself."
                    .to_string(),
            ),
        ),
    }
}

/// A price's unit, and anything about it the user must check.
fn price_unit(words: &[String], samples: &[&str]) -> (Option<ImportUnit>, Option<String>) {
    let has = |options: &[&str]| words.iter().any(|word| options.contains(&word.as_str()));
    let other_currency = OTHER_CURRENCIES
        .iter()
        .find(|(word, _)| has(&[*word]))
        .map(|(_, name)| *name);
    if let Some(name) = other_currency {
        return (
            None,
            Some(format!(
                "The header names {name}, and Tonans works in US dollars. Convert the column \
                 first; choose a unit only if the values really are dollars or cents."
            )),
        );
    }
    if words.iter().any(|word| word.parse::<u32>().is_ok()) {
        return (
            None,
            Some(
                "A number in the header may scale the price, as in 'per 100 kWh'. Choose the \
                 unit yourself."
                    .to_string(),
            ),
        );
    }
    if has(&["mwh"]) {
        return if has(&["usd", "dollar", "dollars"]) {
            (Some(ImportUnit::UsdPerMegawattHour), None)
        } else {
            (
                None,
                Some(
                    "The header says per MWh but gives no currency in dollars. Choose the unit \
                     yourself, or convert the column first."
                        .to_string(),
                ),
            )
        };
    }
    if !has(&["kwh"]) {
        return (
            None,
            Some(
                "The header does not say per what, such as per kWh. Choose the unit from a bill."
                    .to_string(),
            ),
        );
    }
    if has(&["cents"]) {
        return (Some(ImportUnit::CentsPerKilowattHour), None);
    }
    if has(&["c", "ct"]) {
        return (
            Some(ImportUnit::CentsPerKilowattHour),
            Some("'c' is read as US cents; check that against a bill.".to_string()),
        );
    }
    if has(&["usd", "dollar", "dollars"]) {
        return (Some(ImportUnit::UsdPerKilowattHour), None);
    }
    // The header says per kWh and no more. Say so, and read the size of the number.
    let Some(first) = samples.iter().find_map(|cell| cell.parse::<f64>().ok()) else {
        return (
            None,
            Some(
                "The header gives no currency and no value to read. Choose the unit from a bill."
                    .to_string(),
            ),
        );
    };
    let (unit, reading) = if first.abs() < 2.0 {
        (ImportUnit::UsdPerKilowattHour, "dollars")
    } else {
        (ImportUnit::CentsPerKilowattHour, "cents")
    };
    (
        Some(unit),
        Some(format!(
            "The header gives no currency. The first value, {first}, reads as {reading} per kWh \
             by its size alone; check that against a bill."
        )),
    )
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
    fn a_unit_is_suggested_only_when_the_file_states_it() {
        let found = suggest("Timestamp,Load,Solar (kW),Site Demand kWh\n2024-01-01 00:00,1,2,3\n");
        assert_eq!(found.columns.len(), 2, "{found:?}");
        // 'Load' and 'Site Demand kWh' both claim the load, so neither is suggested.
        assert!(suggested(&found, "Load").is_none());
        assert!(suggested(&found, "Site Demand kWh").is_none());
        assert!(found
            .notes
            .iter()
            .any(|n| n.contains("'Load' and 'Site Demand kWh'")));
        assert_eq!(
            suggested(&found, "Solar (kW)").unwrap().unit,
            Some(ImportUnit::Kilowatt)
        );

        // A role with no stated unit keeps the role and leaves the unit for the user.
        let found = suggest("t,load\n2024-01-01T00:00:00Z,1\n");
        assert_eq!(suggested(&found, "load").unwrap().role, "load");
        assert_eq!(suggested(&found, "load").unwrap().unit, None);
    }

    #[test]
    fn unit_spellings_are_read_from_the_header() {
        let unit = |header: &str| {
            let csv = format!("{header}\n1\n");
            suggested(&suggest(&csv), header).and_then(|column| column.unit)
        };
        assert_eq!(unit("Load (kW)"), Some(ImportUnit::Kilowatt));
        assert_eq!(unit("loadKw"), Some(ImportUnit::Kilowatt));
        assert_eq!(unit("load_w"), Some(ImportUnit::Watt));
        assert_eq!(unit("load MW"), Some(ImportUnit::Megawatt));
        assert_eq!(unit("load_kwh"), Some(ImportUnit::KilowattHourPerInterval));
        assert_eq!(unit("load Wh"), Some(ImportUnit::WattHourPerInterval));
        assert_eq!(unit("load kW kWh"), None, "two units state neither");
        assert_eq!(unit("price $/kWh"), Some(ImportUnit::UsdPerKilowattHour));
        assert_eq!(unit("price c/kWh"), Some(ImportUnit::CentsPerKilowattHour));
        assert_eq!(unit("price ¢/kWh"), Some(ImportUnit::CentsPerKilowattHour));
        assert_eq!(
            unit("price usd_per_mwh"),
            Some(ImportUnit::UsdPerMegawattHour)
        );
        assert_eq!(unit("temp_F"), Some(ImportUnit::Fahrenheit));
        assert_eq!(unit("temperature"), None);
    }

    #[test]
    fn a_price_with_no_currency_is_read_by_size_and_says_so() {
        let found = suggest("rate_per_kwh\n9.8\n");
        let column = suggested(&found, "rate_per_kwh").unwrap();
        assert_eq!(column.unit, Some(ImportUnit::CentsPerKilowattHour));
        assert!(column.check.as_deref().unwrap().contains("cents"));
        // No direction either: the check says both things.
        assert!(column
            .check
            .as_deref()
            .unwrap()
            .contains("import or export"));
    }

    #[test]
    fn suggestions_stay_silent_rather_than_guess() {
        // Grid import in kW is not the load, and a demand charge is not an energy price.
        let found = suggest("grid_import_kw,demand_charge_usd,notes\n1,2,x\n");
        assert!(found.columns.is_empty(), "{found:?}");
        assert_eq!(found.notes.len(), 3);
        // A date with no time is not a timestamp the importer accepts.
        let found = suggest("date,load_kw\n2024-01-01,1\n");
        assert!(suggested(&found, "date").is_none());
    }

    #[test]
    fn net_load_is_suggested_with_a_warning_about_double_counting() {
        let found = suggest("net_load_kw\n1\n");
        let column = suggested(&found, "net_load_kw").unwrap();
        assert_eq!(column.role, "load");
        assert!(column.check.as_deref().unwrap().contains("twice"));
    }

    /// Each header the review probed: the role and unit suggested, and a word the visible
    /// check must hold. `None` for the role means nothing is suggested and a note says why.
    #[test]
    fn an_unclear_header_gets_no_suggestion_or_a_visible_check() {
        use ImportUnit::{
            CentsPerKilowattHour as Cents, Kilowatt as Kw, KilowattHourPerInterval as Kwh,
            Megawatt as Mw, Watt as W,
        };
        type Case<'a> = (
            &'a str,
            Option<&'a str>,
            Option<ImportUnit>,
            Option<&'a str>,
        );
        let cases: &[Case] = &[
            // A camelCase unit must not lose its k or M: these were 1000x wrong.
            ("kWhLoad", Some("load"), Some(Kwh), None),
            (
                "Load_kWhDelivered",
                Some("load"),
                Some(Kwh),
                Some("delivered"),
            ),
            ("kWLoad", Some("load"), Some(Kw), None),
            ("LoadkW", Some("load"), Some(Kw), None),
            ("loadkWh", Some("load"), Some(Kwh), None),
            ("SiteLoad_kWh", Some("load"), Some(Kwh), None),
            // Tonans has no MWh-per-interval unit, so it says so instead of going quiet.
            ("MWhLoad", Some("load"), None, Some("MWh")),
            ("load_MWh", Some("load"), None, Some("MWh")),
            ("load mw", Some("load"), Some(Mw), Some("milliwatts")),
            ("Load (MW)", Some("load"), Some(Mw), None),
            ("gen_MW", Some("generation"), Some(Mw), Some("called on")),
            // Another currency is never dollars, and never "no currency".
            ("price_eur_per_kwh", Some("import_price"), None, Some("EUR")),
            ("price_p_per_kwh", Some("import_price"), None, Some("pence")),
            ("price_gbp_per_mwh", Some("import_price"), None, Some("GBP")),
            ("price_mwh", Some("import_price"), None, Some("no currency")),
            (
                "tariff_c_per_kwh",
                Some("import_price"),
                Some(Cents),
                Some("US cents"),
            ),
            // A backup machine or a battery is not generation or a critical load.
            ("backup_generator_kw", None, None, None),
            ("backup_battery_kw", None, None, None),
            ("critical_load_w", Some("critical_load"), Some(W), None),
            ("load_kW_avg", Some("load"), Some(Kw), None),
            // A running total is not the amount in each interval.
            ("load_kwh_cumulative", None, None, None),
            ("meter_reading_kwh", None, None, None),
            ("total_load_kwh_ytd", None, None, None),
            ("demand_kw_15min", Some("load"), Some(Kw), Some("billing")),
            ("peak_demand_kw", None, None, None),
            // A price per kW is a demand charge.
            ("demand_rate_usd_per_kw", None, None, None),
            ("price_usd_per_kw", None, None, None),
        ];
        for (header, role, unit, check) in cases {
            // Name the column by its own header, so one is never mistaken for a rival.
            let found = suggest(&format!("{header}\n1\n"));
            let column = suggested(&found, header);
            assert_eq!(
                column.map(|c| c.role.as_str()),
                *role,
                "{header}: {found:?}"
            );
            assert_eq!(column.and_then(|c| c.unit), *unit, "{header}: {found:?}");
            match (column, check) {
                (Some(column), Some(word)) => assert!(
                    column.check.as_deref().unwrap_or("").contains(word),
                    "{header}: {column:?}"
                ),
                (Some(column), None) => assert_eq!(column.check, None, "{header}"),
                (None, _) => assert!(
                    found.notes.iter().any(|note| note.contains(header)),
                    "{header}: {found:?}"
                ),
            }
        }
    }

    #[test]
    fn another_currency_is_never_read_as_dollars_or_as_no_currency() {
        for header in [
            "price_eur_per_kwh",
            "EUR/kWh",
            "€/kWh",
            "price £/MWh",
            "import price (CAD/kWh)",
            "tariff_euro_c_per_kwh",
        ] {
            let found = suggest(&format!("{header}\n0.12\n"));
            let column = suggested(&found, header).unwrap_or_else(|| panic!("{header}: {found:?}"));
            assert_eq!(column.unit, None, "{header}");
            let check = column.check.as_deref().unwrap();
            assert!(check.contains("US dollars"), "{header}: {check}");
            assert!(!check.contains("gives no currency"), "{header}: {check}");
        }
    }

    #[test]
    fn units_are_told_apart_by_their_capitals_and_never_cut_from_a_plain_word() {
        let column = |header: &str| {
            let found = suggest(&format!("{header}\n1\n"));
            suggested(&found, header).cloned()
        };
        let unit = |header: &str| column(header).and_then(|c| c.unit);
        assert_eq!(unit("KWhLoad"), Some(ImportUnit::KilowattHourPerInterval));
        assert_eq!(unit("kwhLoad"), Some(ImportUnit::KilowattHourPerInterval));
        assert_eq!(unit("LoadKWH"), Some(ImportUnit::KilowattHourPerInterval));
        assert_eq!(unit("load kWH"), Some(ImportUnit::KilowattHourPerInterval));
        assert_eq!(unit("LoadWh"), Some(ImportUnit::WattHourPerInterval));
        assert_eq!(unit("LoadW"), Some(ImportUnit::Watt));
        assert_eq!(
            unit("load kW-hr"),
            Some(ImportUnit::KilowattHourPerInterval)
        );
        assert_eq!(
            unit("load_kilowatt_hours"),
            Some(ImportUnit::KilowattHourPerInterval)
        );
        assert_eq!(unit("load_kWhr"), Some(ImportUnit::KilowattHourPerInterval));

        // Milliwatts are not megawatts, and kVA and GW are not units Tonans reads.
        for (header, word) in [
            ("load_mW", "milliwatts"),
            ("load_kVA", "kVA"),
            ("load_kvar", "kvar"),
            ("load GW", "GW"),
            ("load GWh", "GWh"),
            ("load kW kWh", "more than one"),
            ("load kW/h", "power or energy"),
        ] {
            let found = column(header).unwrap_or_else(|| panic!("{header}"));
            assert_eq!(found.unit, None, "{header}");
            assert!(
                found.check.as_deref().unwrap().contains(word),
                "{header}: {found:?}"
            );
        }

        // A capital in an ordinary word holds no unit, and 15_min is not a minimum.
        assert_eq!(unit("peakWindow_load"), None);
        assert!(column("demand_kw_15_min").is_some());
        assert!(column("min_load_kw").is_none());
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
        let column = suggested(&found, "total_load_kw").unwrap();
        assert_eq!(
            column.check, None,
            "a total in kW is a rate, not a running total"
        );
        let found = suggest("total_load_kwh\n100\n");
        assert!(suggested(&found, "total_load_kwh")
            .unwrap()
            .check
            .as_deref()
            .unwrap()
            .contains("running total"));
    }

    #[test]
    fn other_unclear_headers_are_left_or_flagged() {
        // Neither a ratio nor a column of words can be a load.
        for csv in ["load_factor\n0.5\n", "load %\n50\n", "load_type\noffice\n"] {
            let found = suggest(csv);
            assert!(found.columns.is_empty(), "{csv}: {found:?}");
            assert_eq!(found.notes.len(), 1, "{csv}");
        }
        // Grid flows are not the load, and a price needs a per-unit and no scale.
        assert!(suggest("load_export_kw\n1\n").columns.is_empty());
        for (header, word) in [
            ("price_usd", "per what"),
            ("price_per_100_kwh", "scale"),
            ("price_cents_per_mwh", "per MWh"),
            ("temp_K", "Kelvin"),
            ("forecast_load_kw", "forecast"),
            ("generator_kw", "called on"),
            ("backup_load_kw", "critical"),
        ] {
            let found = suggest(&format!("{header}\n1\n"));
            let column = suggested(&found, header).unwrap_or_else(|| panic!("{header}"));
            assert!(
                column.check.as_deref().unwrap().contains(word),
                "{header}: {column:?}"
            );
        }
        assert_eq!(
            suggested(&suggest("backup_load_kw\n1\n"), "backup_load_kw")
                .unwrap()
                .role,
            "critical_load"
        );
        assert_eq!(
            suggested(&suggest("temp_K\n1\n"), "temp_K").unwrap().unit,
            None
        );
        assert!(suggest("standby_diesel_gen_kw\n1\n").columns.is_empty());
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
