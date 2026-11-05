//! A description of imported data, with no model in it.
//!
//! These are sums and extremes of what was imported, so a planner can check the file
//! was read correctly and has a scale to size scenarios against. Nothing here is a
//! result: no battery, tariff, or dispatch rule is involved.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::error::Result;
use crate::schema::{Project, Series, SeriesKind};

/// A power series, described.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PowerProfile {
    /// Energy over the whole data set, kWh.
    pub energy_kwh: f64,
    /// Mean power, kW.
    pub mean_kw: f64,
    /// Highest interval power, kW.
    pub peak_kw: f64,
    /// Start of the first interval at that power.
    pub peak_utc: DateTime<Utc>,
}

/// The range of a series that is not power: a price or a temperature.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RangeProfile {
    /// Lowest value.
    pub min: f64,
    /// Mean value.
    pub mean: f64,
    /// Highest value.
    pub max: f64,
}

/// One local calendar month.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MonthProfile {
    /// Local calendar year.
    pub year: i32,
    /// Local calendar month, 1 to 12.
    pub month: u32,
    /// Load energy in the month, kWh.
    pub load_kwh: f64,
    /// Highest interval load in the month, kW.
    pub peak_load_kw: f64,
    /// Generation available in the month, kWh.
    pub generation_kwh: f64,
}

/// A description of a project's imported data.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DatasetProfile {
    /// Hours the data covers.
    pub covered_hours: f64,
    /// Total site load.
    pub load: PowerProfile,
    /// Critical load, if imported.
    pub critical_load: Option<PowerProfile>,
    /// On-site generation, if imported.
    pub generation: Option<PowerProfile>,
    /// Highest of load minus generation in any interval, kW.
    pub peak_net_load_kw: f64,
    /// Import price, USD/kWh, if imported.
    pub import_price: Option<RangeProfile>,
    /// Export credit, USD/kWh, if imported.
    pub export_price: Option<RangeProfile>,
    /// Ambient temperature, °C, if imported.
    pub temperature: Option<RangeProfile>,
    /// Local calendar months, in order.
    pub months: Vec<MonthProfile>,
}

/// Describe a project's data, or `None` if nothing has been imported.
pub fn describe(project: &Project) -> Result<Option<DatasetProfile>> {
    let (Some(dataset), Some(grid)) = (&project.dataset, project.grid()?) else {
        return Ok(None);
    };
    let Some(load_series) = dataset.series.get(&SeriesKind::Load) else {
        return Ok(None);
    };
    let hours = grid.hours();

    let power = |series: &Series| -> Option<PowerProfile> {
        let values = &series.values;
        if values.is_empty() {
            return None;
        }
        let mut peak_index = 0;
        for (index, value) in values.iter().enumerate() {
            if *value > values[peak_index] {
                peak_index = index;
            }
        }
        let sum: f64 = values.iter().sum();
        Some(PowerProfile {
            energy_kwh: sum * hours,
            mean_kw: sum / values.len() as f64,
            peak_kw: values[peak_index],
            peak_utc: grid.start_of(peak_index),
        })
    };
    let range = |kind: SeriesKind| -> Option<RangeProfile> {
        let values = &dataset.series.get(&kind)?.values;
        let first = *values.first()?;
        Some(RangeProfile {
            min: values.iter().copied().fold(first, f64::min),
            mean: values.iter().sum::<f64>() / values.len() as f64,
            max: values.iter().copied().fold(first, f64::max),
        })
    };

    let Some(load) = power(load_series) else {
        return Ok(None);
    };
    let generation = dataset.series.get(&SeriesKind::Generation);

    let mut months: Vec<MonthProfile> = Vec::new();
    let mut peak_net_load_kw = f64::NEG_INFINITY;
    for (index, load_kw) in load_series.values.iter().enumerate() {
        let generation_kw = generation.map_or(0.0, |series| series.values[index]);
        peak_net_load_kw = peak_net_load_kw.max(load_kw - generation_kw);

        let (year, month) = grid.local_month_of(index);
        if months
            .last()
            .is_none_or(|m| (m.year, m.month) != (year, month))
        {
            months.push(MonthProfile {
                year,
                month,
                load_kwh: 0.0,
                peak_load_kw: *load_kw,
                generation_kwh: 0.0,
            });
        }
        if let Some(current) = months.last_mut() {
            current.load_kwh += load_kw * hours;
            current.peak_load_kw = current.peak_load_kw.max(*load_kw);
            current.generation_kwh += generation_kw * hours;
        }
    }

    Ok(Some(DatasetProfile {
        covered_hours: hours * load_series.values.len() as f64,
        critical_load: dataset
            .series
            .get(&SeriesKind::CriticalLoad)
            .and_then(&power),
        generation: generation.and_then(&power),
        load,
        peak_net_load_kw,
        import_price: range(SeriesKind::ImportPrice),
        export_price: range(SeriesKind::ExportPrice),
        temperature: range(SeriesKind::Temperature),
        months,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csvimport::{self, ColumnMapping, ImportRequest, ImportUnit, MappedColumn};
    use crate::schema::{GapPolicy, Site};
    use std::collections::BTreeMap;

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_767_225_600, 0).unwrap()
    }

    fn project_with(csv: &str) -> Project {
        let mut series = BTreeMap::new();
        for (kind, header) in [(SeriesKind::Load, "load"), (SeriesKind::Generation, "pv")] {
            series.insert(
                kind,
                MappedColumn {
                    header: header.to_string(),
                    unit: ImportUnit::Kilowatt,
                },
            );
        }
        let request = ImportRequest {
            filename: "t.csv".to_string(),
            timezone: "America/New_York".to_string(),
            interval_minutes: 30,
            gap_policy: GapPolicy::Reject,
            mapping: ColumnMapping {
                timestamp: "t".to_string(),
                series,
                demand_period: None,
                ignore_columns: Vec::new(),
            },
        };
        let mut project = Project::new(
            "p",
            "P",
            Site {
                timezone: "America/New_York".to_string(),
                currency: "USD".to_string(),
            },
            now(),
        )
        .unwrap();
        project.dataset = Some(
            csvimport::import(csv.as_bytes(), &request, now())
                .unwrap()
                .dataset,
        );
        project
    }

    #[test]
    fn a_project_with_no_data_has_no_profile() {
        let project = Project::new("p", "P", Site::default(), now()).unwrap();
        assert_eq!(describe(&project).unwrap(), None);
    }

    #[test]
    fn the_profile_matches_a_hand_calculation_across_a_local_month_boundary() {
        // Half-hour intervals either side of local midnight, 1 February in New York.
        let project = project_with(
            "t,load,pv\n\
             2024-01-31T23:00:00-05:00,10,0\n\
             2024-01-31T23:30:00-05:00,30,5\n\
             2024-02-01T00:00:00-05:00,20,4\n\
             2024-02-01T00:30:00-05:00,40,0\n",
        );
        let profile = describe(&project).unwrap().unwrap();

        assert!((profile.covered_hours - 2.0).abs() < 1e-12);
        // (10 + 30 + 20 + 40) kW x 0.5 h
        assert!((profile.load.energy_kwh - 50.0).abs() < 1e-12);
        assert!((profile.load.mean_kw - 25.0).abs() < 1e-12);
        assert!((profile.load.peak_kw - 40.0).abs() < 1e-12);
        assert_eq!(
            profile.load.peak_utc.to_rfc3339(),
            "2024-02-01T05:30:00+00:00"
        );
        // load - pv: 10, 25, 16, 40.
        assert!((profile.peak_net_load_kw - 40.0).abs() < 1e-12);

        let generation = profile.generation.unwrap();
        assert!((generation.energy_kwh - 4.5).abs() < 1e-12);
        assert!((generation.peak_kw - 5.0).abs() < 1e-12);
        assert!(profile.critical_load.is_none());
        assert!(profile.import_price.is_none());

        assert_eq!(profile.months.len(), 2);
        assert_eq!((profile.months[0].year, profile.months[0].month), (2024, 1));
        assert!((profile.months[0].load_kwh - 20.0).abs() < 1e-12);
        assert!((profile.months[0].peak_load_kw - 30.0).abs() < 1e-12);
        assert!((profile.months[0].generation_kwh - 2.5).abs() < 1e-12);
        assert_eq!((profile.months[1].year, profile.months[1].month), (2024, 2));
        assert!((profile.months[1].load_kwh - 30.0).abs() < 1e-12);
        assert!((profile.months[1].peak_load_kw - 40.0).abs() < 1e-12);
    }

    #[test]
    fn the_first_of_equal_peaks_is_the_one_reported() {
        let project = project_with(
            "t,load,pv\n\
             2024-01-01T00:00:00-05:00,7,0\n\
             2024-01-01T00:30:00-05:00,7,0\n",
        );
        let profile = describe(&project).unwrap().unwrap();
        assert_eq!(
            profile.load.peak_utc.to_rfc3339(),
            "2024-01-01T05:00:00+00:00"
        );
    }
}
