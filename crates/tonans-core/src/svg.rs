//! Chart rendering, as inline SVG with no script.
//!
//! The exported report must be a single file that opens from a filesystem with
//! `script-src 'none'`. Every chart is therefore rendered here, server-side, into
//! SVG markup — no plotting library, no runtime, nothing to execute. Hover is
//! provided by native `<title>` elements, which need no script at all.
//!
//! # The palette is computed, not chosen
//!
//! Colour is assigned by the job it does, and the assignment is *verified* rather
//! than eyeballed. [`tests::palette_survives_colour_vision_deficiency`] recomputes
//! the Machado-2009 protanopia and deuteranopia transforms and asserts that every
//! pair of colours that can share a surface stays above ΔE 12. It also asserts the
//! OKLCH lightness band and chroma floor for both light and dark mode. Change a
//! hex here and the test tells you whether you have broken it.
//!
//! Two consequences of that check shaped the design, and are worth stating because
//! they look like arbitrary layout decisions and are not:
//!
//! * **Each timeline panel carries at most two series.** Six mutually-comparable
//!   hues cannot be made colour-vision-safe from this palette in dark mode — the
//!   best available set collapses to ΔE 4.7, well under the floor. Splitting the
//!   timeline into four panels of one or two series means only two colours ever
//!   share a plot area, and the worst such pair is ΔE 48.8.
//! * **The energy-flow diagram is two stacked bars, not a six-way colour key.** In
//!   a stack only neighbours touch, so only adjacent segments need separating. It
//!   also happens to be the more honest picture: energy in and energy out are drawn
//!   to the same total length, so a reader can *see* the balance close.
//!
//! Three light-mode hues sit below 3:1 against the surface. That is permitted only
//! with a relief channel, so every chart in the report is accompanied by the full
//! data table and every series is directly labelled. Neither is decoration.
//!
//! # State of charge is not on a second axis
//!
//! It never shares an axis with power. A chart with two y-scales invites the reader
//! to compare two quantities whose relative height is an artefact of the scaling the
//! author chose. State of charge gets its own panel, on its own percentage axis,
//! aligned to the same time base.

use std::fmt::Write as _;

use crate::limits::CHART_MAX_POINTS;

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

/// A quantity that gets its own colour.
///
/// Colour follows the entity, everywhere, in every chart. Load is the same blue in
/// the timeline, in the flow diagram, and in the outage panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Site load.
    Load,
    /// On-site generation.
    Generation,
    /// Energy from the grid.
    GridImport,
    /// Energy to the grid.
    GridExport,
    /// Energy into the battery.
    BatteryCharge,
    /// Energy out of the battery.
    BatteryDischarge,
    /// Critical load nothing could serve.
    ///
    /// Wears the reserved **status** colour, never a categorical slot, and always
    /// ships with a label — a failure must never be identified by hue alone. No
    /// ordinary series is allowed to be red, so this red is unambiguous.
    ///
    /// This is why curtailment has no colour of its own. Green, the only remaining
    /// slot, sits at ΔE 8.5 from this red under deuteranopia — and "we threw away
    /// free energy" must never be mistakable for "the critical load went unserved".
    /// Curtailment and losses are reported as numbers, in the table and the caption,
    /// where they cannot be misread as anything.
    UnmetCritical,
    /// State of charge. Alone in its panel, so it takes the default first slot.
    StateOfCharge,
}

impl Role {
    /// The CSS custom property this role's colour is read from.
    ///
    /// Indirection through a custom property is what lets one SVG serve both light
    /// and dark mode: the report redefines the properties under a
    /// `prefers-color-scheme` query and the markup never changes.
    #[must_use]
    pub const fn css_var(self) -> &'static str {
        match self {
            Role::Load | Role::StateOfCharge => "var(--c-load)",
            Role::Generation => "var(--c-generation)",
            Role::GridImport => "var(--c-import)",
            Role::GridExport => "var(--c-export)",
            Role::BatteryCharge => "var(--c-charge)",
            Role::BatteryDischarge => "var(--c-discharge)",
            Role::UnmetCritical => "var(--c-unmet)",
        }
    }

    /// The name shown in a legend or a direct label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Role::Load => "Load",
            Role::Generation => "Generation",
            Role::GridImport => "Grid import",
            Role::GridExport => "Grid export",
            Role::BatteryCharge => "Battery charge",
            Role::BatteryDischarge => "Battery discharge",
            Role::UnmetCritical => "Unmet critical load",
            Role::StateOfCharge => "State of charge",
        }
    }
}

/// The validated palette, in both modes.
///
/// Emitted as CSS custom properties by [`palette_css`]. The hexes are the *only*
/// place a colour is written down; nothing downstream hard-codes one.
pub mod palette {
    /// `(role, light, dark)` for every categorical slot, plus the reserved status colour.
    ///
    /// Six entity hues, and that is not an accident: the palette's other two slots are
    /// green and red, and green sits ΔE 8.5 from the reserved failure red under
    /// deuteranopia. Rather than let a seventh series be *nearly* confusable with "the
    /// critical load went unserved", Tonans has six coloured quantities and reports the
    /// rest as numbers. See [`Role::UnmetCritical`].
    pub const ENTITIES: &[(&str, &str, &str)] = &[
        ("--c-load", "#2a78d6", "#3987e5"),
        ("--c-generation", "#1baf7a", "#199e70"),
        ("--c-import", "#4a3aa7", "#9085e9"),
        ("--c-export", "#eda100", "#c98500"),
        ("--c-charge", "#e87ba4", "#d55181"),
        ("--c-discharge", "#eb6834", "#d95926"),
        // Reserved status. No categorical slot is red, so this cannot be mistaken for
        // a series.
        ("--c-unmet", "#d03b3b", "#d03b3b"),
    ];

    /// Surfaces and ink.
    pub const CHROME: &[(&str, &str, &str)] = &[
        ("--surface", "#fcfcfb", "#1a1a19"),
        ("--page", "#f9f9f7", "#0d0d0d"),
        ("--ink", "#0b0b0b", "#ffffff"),
        ("--ink-secondary", "#52514e", "#c3c2b7"),
        ("--ink-muted", "#898781", "#898781"),
        ("--gridline", "#e1e0d9", "#2c2c2a"),
        ("--axis", "#c3c2b7", "#383835"),
        (
            "--hairline",
            "rgba(11,11,11,0.10)",
            "rgba(255,255,255,0.10)",
        ),
        ("--good", "#0ca30c", "#0ca30c"),
        ("--warning", "#fab219", "#fab219"),
        ("--critical", "#d03b3b", "#d03b3b"),
    ];

    /// Sequential blue, light to dark. Magnitude, one hue.
    pub const SEQUENTIAL: &[&str] = &[
        "#cde2fb", "#b7d3f6", "#9ec5f4", "#86b6ef", "#6da7ec", "#5598e7", "#3987e5", "#2a78d6",
        "#256abf", "#1c5cab", "#184f95", "#104281", "#0d366b",
    ];

    /// Diverging blue-to-red with a neutral midpoint. Polarity: which side of zero.
    /// Used for net present value, where the sign is the whole point.
    pub const DIVERGING_NEGATIVE: &[&str] = &["#d03b3b", "#e34948", "#e87b7b", "#f2b8b8"];
    /// See [`DIVERGING_NEGATIVE`].
    pub const DIVERGING_POSITIVE: &[&str] = &["#9ec5f4", "#5598e7", "#2a78d6", "#184f95"];
    /// The neutral midpoint of the diverging ramp: light and dark.
    pub const DIVERGING_MID: (&str, &str) = ("#f0efec", "#383835");

    /// The chart surface each mode is validated against.
    pub const SURFACE_LIGHT: &str = "#fcfcfb";
    /// See [`SURFACE_LIGHT`].
    pub const SURFACE_DARK: &str = "#1a1a19";
}

/// The `:root` custom properties every chart reads its colours from, with a
/// `prefers-color-scheme` block for dark mode.
///
/// Dark is *selected*, not an inversion: each hue is a step chosen for the dark
/// surface and validated against it.
#[must_use]
pub fn palette_css() -> String {
    let mut css = String::from(":root{color-scheme:light dark;");
    for (name, light, _) in palette::CHROME.iter().chain(palette::ENTITIES) {
        let _ = write!(css, "{name}:{light};");
    }
    css.push_str("}@media (prefers-color-scheme:dark){:root{");
    for (name, _, dark) in palette::CHROME.iter().chain(palette::ENTITIES) {
        let _ = write!(css, "{name}:{dark};");
    }
    css.push_str("}}");
    css
}

/// The heat-map ramps and the two inks that sit on a filled mark, as custom properties.
///
/// Separate from [`palette_css`] because only the live UI reads them: the exported
/// report writes its ramp colours inline, and its bytes should not move for this.
#[must_use]
pub fn ramp_css() -> String {
    let mut css = String::from(":root{--on-fill-light:#ffffff;--on-fill-dark:#0b0b0b;");
    for (index, colour) in palette::SEQUENTIAL.iter().enumerate() {
        let _ = write!(css, "--seq-{index}:{colour};");
    }
    for (index, colour) in palette::DIVERGING_NEGATIVE.iter().enumerate() {
        let _ = write!(css, "--div-neg-{index}:{colour};");
    }
    for (index, colour) in palette::DIVERGING_POSITIVE.iter().enumerate() {
        let _ = write!(css, "--div-pos-{index}:{colour};");
    }
    let (light, dark) = palette::DIVERGING_MID;
    let _ = write!(
        css,
        "--div-mid:{light};}}@media (prefers-color-scheme:dark){{:root{{--div-mid:{dark};}}}}"
    );
    css
}

// ---------------------------------------------------------------------------
// Escaping
// ---------------------------------------------------------------------------

/// Escape text for inclusion in XML/HTML content or an attribute value.
///
/// Every string that reaches a chart — a scenario name, a file name, a warning —
/// is under the user's control and some of it is under an attacker's. It all goes
/// through here. See `docs/SECURITY.md`.
#[must_use]
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Decimation
// ---------------------------------------------------------------------------

/// One bucket of a decimated series.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket {
    /// Lowest value in the bucket.
    pub min: f64,
    /// Highest value in the bucket.
    pub max: f64,
    /// Mean of the bucket.
    pub mean: f64,
}

/// Reduce `values` to at most `target` buckets, keeping the extremes.
///
/// A year of 15-minute data is 35,040 points and a chart is perhaps 1,200 pixels
/// wide. Something has to give, and the usual choice — sample every *n*th point —
/// is the one choice that can hide a peak. Since the peak is frequently the entire
/// point of the analysis (it sets the demand charge), each bucket keeps its own
/// minimum and maximum, drawn as a band, with the mean drawn through it. Nothing
/// is hidden; the reader is told the chart is decimated and where the full data is.
#[must_use]
pub fn decimate(values: &[f64], target: usize) -> Vec<Bucket> {
    let target = target.max(1);
    if values.is_empty() {
        return Vec::new();
    }
    if values.len() <= target {
        return values
            .iter()
            .map(|v| Bucket {
                min: *v,
                max: *v,
                mean: *v,
            })
            .collect();
    }

    let mut buckets = Vec::with_capacity(target);
    for index in 0..target {
        // Integer arithmetic on the boundaries, so every point lands in exactly one
        // bucket and no point is dropped by a rounding accident.
        let start = index * values.len() / target;
        let end = ((index + 1) * values.len() / target)
            .max(start + 1)
            .min(values.len());
        let slice = &values[start..end];
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        let mut sum = 0.0;
        for value in slice {
            min = min.min(*value);
            max = max.max(*value);
            sum += *value;
        }
        buckets.push(Bucket {
            min,
            max,
            mean: sum / slice.len() as f64,
        });
    }
    buckets
}

/// Whether a series of this length will be decimated in a chart.
#[must_use]
pub fn is_decimated(len: usize) -> bool {
    len > CHART_MAX_POINTS
}

// ---------------------------------------------------------------------------
// Axis ticks
// ---------------------------------------------------------------------------

/// Round tick values spanning `min..=max`, at most `count` of them.
///
/// Ticks land on 1, 2, or 5 times a power of ten, which is what makes an axis
/// readable without the reader doing arithmetic.
#[must_use]
pub fn nice_ticks(min: f64, max: f64, count: usize) -> Vec<f64> {
    if !min.is_finite() || !max.is_finite() || count == 0 {
        return vec![0.0];
    }
    if (max - min).abs() < f64::EPSILON {
        return vec![min];
    }
    let raw = (max - min) / count as f64;
    let magnitude = 10f64.powi(raw.abs().log10().floor() as i32);
    let normalized = raw / magnitude;
    let step = if normalized <= 1.0 {
        1.0
    } else if normalized <= 2.0 {
        2.0
    } else if normalized <= 5.0 {
        5.0
    } else {
        10.0
    } * magnitude;

    let first = (min / step).floor() * step;
    let mut ticks = Vec::new();
    let mut tick = first;
    // Bounded regardless of floating-point behaviour: `count + 3` is the most a
    // well-formed axis can need, and a malformed one must not spin.
    while tick <= max + step * 0.5 && ticks.len() < count + 3 {
        if tick >= min - step * 0.5 {
            ticks.push(tick);
        }
        tick += step;
    }
    if ticks.is_empty() {
        ticks.push(min);
    }
    ticks
}

/// Format a number for an axis tick or a direct label.
#[must_use]
pub fn format_number(value: f64) -> String {
    let magnitude = value.abs();
    if magnitude >= 1_000_000.0 {
        format!("{:.1}M", value / 1_000_000.0)
    } else if magnitude >= 10_000.0 {
        format!("{:.0}k", value / 1_000.0)
    } else if magnitude >= 100.0 {
        format!("{value:.0}")
    } else if magnitude >= 1.0 {
        format!("{value:.1}")
    } else if magnitude < 1e-9 {
        "0".to_string()
    } else {
        format!("{value:.3}")
    }
}

// ---------------------------------------------------------------------------
// Charts
// ---------------------------------------------------------------------------

/// One series inside a panel.
#[derive(Debug, Clone)]
pub struct Series<'a> {
    /// Which entity this is. Fixes its colour, everywhere.
    pub role: Role,
    /// What to call it in the legend and the direct label.
    pub label: String,
    /// One value per interval.
    pub values: &'a [f64],
}

/// One plot area. At most two series — see the module documentation for why.
#[derive(Debug, Clone)]
pub struct Panel<'a> {
    /// The panel heading.
    pub title: String,
    /// The unit on the y-axis.
    pub axis_unit: String,
    /// The series. One or two.
    pub series: Vec<Series<'a>>,
    /// Whether zero sits in the middle of the axis, so that a signed quantity —
    /// grid exchange, battery power — reads as "which side of nothing".
    pub signed: bool,
}

const PANEL_HEIGHT: f64 = 132.0;
const PANEL_GAP: f64 = 26.0;
const MARGIN_LEFT: f64 = 62.0;
const MARGIN_RIGHT: f64 = 128.0; // room for direct labels at the right edge
const MARGIN_TOP: f64 = 26.0;
const AXIS_HEIGHT: f64 = 34.0;

/// Render a stack of panels sharing one time axis.
///
/// `x_labels` are drawn beneath the last panel; supply four to eight. `source_len`
/// is the true number of intervals, used to tell the reader when the chart is
/// decimated and where the full-resolution data is.
#[must_use]
pub fn timeline(
    panels: &[Panel<'_>],
    x_labels: &[String],
    source_len: usize,
    width: f64,
) -> String {
    if panels.is_empty() {
        return String::new();
    }
    let plot_width = width - MARGIN_LEFT - MARGIN_RIGHT;
    let height = MARGIN_TOP + panels.len() as f64 * (PANEL_HEIGHT + PANEL_GAP) + AXIS_HEIGHT;

    let mut svg = String::with_capacity(16_384);
    let _ = write!(
        svg,
        r#"<svg class="chart" viewBox="0 0 {width:.0} {height:.0}" width="100%" role="img" xmlns="http://www.w3.org/2000/svg" aria-label="Energy timeline">"#
    );

    for (index, panel) in panels.iter().enumerate() {
        let top = MARGIN_TOP + index as f64 * (PANEL_HEIGHT + PANEL_GAP);
        render_panel(&mut svg, panel, top, plot_width);
    }

    // The shared time axis, once, beneath everything.
    let axis_y = MARGIN_TOP + panels.len() as f64 * (PANEL_HEIGHT + PANEL_GAP) - PANEL_GAP + 4.0;
    for (index, label) in x_labels.iter().enumerate() {
        let x = if x_labels.len() > 1 {
            MARGIN_LEFT + plot_width * index as f64 / (x_labels.len() - 1) as f64
        } else {
            MARGIN_LEFT
        };
        let anchor = match index {
            0 => "start",
            i if i == x_labels.len() - 1 => "end",
            _ => "middle",
        };
        let _ = write!(
            svg,
            r#"<text x="{x:.1}" y="{y:.1}" text-anchor="{anchor}" font-size="11" fill="var(--ink-muted)">{label}</text>"#,
            y = axis_y + 16.0,
            label = escape(label)
        );
    }

    if is_decimated(source_len) {
        let _ = write!(
            svg,
            r#"<text x="{x:.1}" y="{y:.1}" text-anchor="start" font-size="10" fill="var(--ink-muted)">{note}</text>"#,
            x = MARGIN_LEFT,
            y = axis_y + 32.0,
            note = escape(&format!(
                "{source_len} intervals decimated to {CHART_MAX_POINTS} buckets. The shaded band is each bucket's full range, so no peak is hidden. Full resolution is in the timeseries CSV."
            ))
        );
    }

    svg.push_str("</svg>");
    svg
}

fn render_panel(svg: &mut String, panel: &Panel<'_>, top: f64, plot_width: f64) {
    let bottom = top + PANEL_HEIGHT;

    // Domain across every series in the panel, so the two share one scale.
    let mut low = f64::INFINITY;
    let mut high = f64::NEG_INFINITY;
    for series in &panel.series {
        for value in series.values {
            low = low.min(*value);
            high = high.max(*value);
        }
    }
    if !low.is_finite() || !high.is_finite() {
        low = 0.0;
        high = 1.0;
    }
    if panel.signed {
        let extent = low.abs().max(high.abs()).max(1e-6);
        low = -extent;
        high = extent;
    } else {
        low = low.min(0.0);
        if (high - low).abs() < 1e-9 {
            high = low + 1.0;
        }
    }

    let scale_y = |value: f64| bottom - (value - low) / (high - low) * PANEL_HEIGHT;

    let _ = write!(
        svg,
        r#"<text x="{x:.1}" y="{y:.1}" font-size="12" font-weight="600" fill="var(--ink)">{title}</text>"#,
        x = MARGIN_LEFT,
        y = top - 8.0,
        title = escape(&panel.title)
    );

    // Recessive gridlines and a labelled y-axis.
    for tick in nice_ticks(low, high, 3) {
        let y = scale_y(tick);
        if !(top - 1.0..=bottom + 1.0).contains(&y) {
            continue;
        }
        let emphasis = if tick.abs() < 1e-9 {
            "var(--axis)"
        } else {
            "var(--gridline)"
        };
        let _ = write!(
            svg,
            r#"<line x1="{x1:.1}" y1="{y:.1}" x2="{x2:.1}" y2="{y:.1}" stroke="{emphasis}" stroke-width="1"/>"#,
            x1 = MARGIN_LEFT,
            x2 = MARGIN_LEFT + plot_width
        );
        let _ = write!(
            svg,
            r#"<text x="{x:.1}" y="{ty:.1}" text-anchor="end" font-size="10" fill="var(--ink-muted)" style="font-variant-numeric:tabular-nums">{label}</text>"#,
            x = MARGIN_LEFT - 8.0,
            ty = y + 3.5,
            label = escape(&format_number(tick))
        );
    }
    let _ = write!(
        svg,
        r#"<text x="{x:.1}" y="{y:.1}" text-anchor="end" font-size="10" fill="var(--ink-muted)">{unit}</text>"#,
        x = MARGIN_LEFT - 8.0,
        y = top - 6.0,
        unit = escape(&panel.axis_unit)
    );

    for series in &panel.series {
        let buckets = decimate(series.values, CHART_MAX_POINTS);
        if buckets.is_empty() {
            continue;
        }
        let x_of = |i: usize| {
            if buckets.len() > 1 {
                MARGIN_LEFT + plot_width * i as f64 / (buckets.len() - 1) as f64
            } else {
                MARGIN_LEFT + plot_width / 2.0
            }
        };
        let colour = series.role.css_var();

        // The band: each bucket's full range. Collapses to nothing when the data is
        // not decimated, which is exactly right.
        if buckets.iter().any(|b| (b.max - b.min).abs() > 1e-9) {
            let mut band = String::with_capacity(buckets.len() * 16);
            for (i, bucket) in buckets.iter().enumerate() {
                let _ = write!(
                    band,
                    "{}{:.1} {:.1} ",
                    if i == 0 { "M" } else { "L" },
                    x_of(i),
                    scale_y(bucket.max)
                );
            }
            for (i, bucket) in buckets.iter().enumerate().rev() {
                let _ = write!(band, "L{:.1} {:.1} ", x_of(i), scale_y(bucket.min));
            }
            band.push('Z');
            let _ = write!(
                svg,
                r#"<path d="{band}" fill="{colour}" fill-opacity="0.28" stroke="none"/>"#
            );
        }

        // The mean, as a 2px line.
        let mut line = String::with_capacity(buckets.len() * 12);
        for (i, bucket) in buckets.iter().enumerate() {
            let _ = write!(
                line,
                "{}{:.1} {:.1} ",
                if i == 0 { "M" } else { "L" },
                x_of(i),
                scale_y(bucket.mean)
            );
        }
        let _ = write!(
            svg,
            r#"<path d="{line}" fill="none" stroke="{colour}" stroke-width="2" stroke-linejoin="round" stroke-linecap="round"><title>{label}</title></path>"#,
            label = escape(&series.label)
        );

        // The direct label, at the right edge. This is the relief channel that makes
        // the sub-3:1 light-mode hues legible, so it is not optional.
        let last = buckets[buckets.len() - 1].mean;
        let _ = write!(
            svg,
            r#"<circle cx="{cx:.1}" cy="{cy:.1}" r="3.5" fill="{colour}" stroke="var(--surface)" stroke-width="2"/><text x="{tx:.1}" y="{ty:.1}" font-size="11" font-weight="500" fill="var(--ink-secondary)">{label}</text>"#,
            cx = MARGIN_LEFT + plot_width,
            cy = scale_y(last),
            tx = MARGIN_LEFT + plot_width + 9.0,
            ty = scale_y(last) + 3.5,
            label = escape(&series.label)
        );
    }
}

/// One segment of the energy-flow diagram.
#[derive(Debug, Clone)]
pub struct FlowSegment {
    /// Which entity.
    pub role: Role,
    /// What to call it.
    pub label: String,
    /// How much energy, in kWh.
    pub value_kwh: f64,
}

/// The energy-flow diagram: what arrived at the bus, and what left it.
///
/// Two stacked bars, drawn to the same scale. They are the two sides of the bus
/// balance:
///
/// ```text
/// generation used + grid import + battery discharge
///     = load served + grid export + battery charge
/// ```
///
/// The engine asserts that identity every interval, so the bars come out *exactly*
/// the same length — and a reader can see that they do. That is the whole point: the
/// diagram is a picture of the invariant holding, not a decoration.
///
/// Curtailed generation and conversion losses are deliberately **not** segments
/// here. Neither is on the bus — curtailed energy never reached it, and losses happen
/// inside the battery — so adding them would make the bars unequal and quietly
/// destroy the one thing the figure exists to show. They are reported as numbers in
/// the caption and the energy table.
///
/// This is an **energy** flow diagram. It is not an AC power-flow study: no voltage,
/// no phase angle, no impedance, and it says nothing whatever about whether the
/// site's conductors can carry these flows.
#[must_use]
pub fn energy_flow(inputs: &[FlowSegment], outputs: &[FlowSegment], width: f64) -> String {
    let total_in: f64 = inputs.iter().map(|s| s.value_kwh.max(0.0)).sum();
    let total_out: f64 = outputs.iter().map(|s| s.value_kwh.max(0.0)).sum();
    let scale_total = total_in.max(total_out);
    if scale_total <= 0.0 {
        return String::new();
    }

    let left = 92.0;
    let bar_width = width - left - 24.0;
    let height = 172.0;
    let mut svg = String::with_capacity(4_096);
    let _ = write!(
        svg,
        r#"<svg class="chart" viewBox="0 0 {width:.0} {height:.0}" width="100%" role="img" xmlns="http://www.w3.org/2000/svg" aria-label="Energy flow: sources and sinks">"#
    );

    for (row, (title, segments, total)) in [
        ("Energy in", inputs, total_in),
        ("Energy out", outputs, total_out),
    ]
    .iter()
    .enumerate()
    {
        let y = 30.0 + row as f64 * 76.0;
        let _ = write!(
            svg,
            r#"<text x="0" y="{ty:.1}" font-size="12" font-weight="600" fill="var(--ink)">{title}</text><text x="0" y="{sy:.1}" font-size="11" fill="var(--ink-muted)" style="font-variant-numeric:tabular-nums">{total} kWh</text>"#,
            ty = y + 16.0,
            sy = y + 31.0,
            title = escape(title),
            total = escape(&format_number(*total))
        );

        let mut x = left;
        for segment in segments.iter() {
            let value = segment.value_kwh.max(0.0);
            if value <= 0.0 {
                continue;
            }
            // A 2px surface gap between segments, so two fills never touch directly.
            let segment_width = (value / scale_total * bar_width - 2.0).max(0.0);
            if segment_width <= 0.0 {
                x += value / scale_total * bar_width;
                continue;
            }
            let share = value / *total * 100.0;
            let _ = write!(
                svg,
                r#"<rect x="{x:.1}" y="{y:.1}" width="{w:.1}" height="34" rx="4" fill="{colour}"><title>{label}: {value} kWh ({share:.1}% of {title})</title></rect>"#,
                w = segment_width,
                colour = segment.role.css_var(),
                label = escape(&segment.label),
                value = escape(&format_number(value)),
                title = escape(title)
            );
            // Direct-label anything wide enough to hold text; the rest is in the table.
            if segment_width > 74.0 {
                let _ = write!(
                    svg,
                    r#"<text x="{tx:.1}" y="{ty:.1}" font-size="10" font-weight="600" fill="var(--surface)">{label}</text><text x="{tx:.1}" y="{vy:.1}" font-size="10" fill="var(--surface)" style="font-variant-numeric:tabular-nums">{value}</text>"#,
                    tx = x + 7.0,
                    ty = y + 15.0,
                    vy = y + 27.0,
                    label = escape(&segment.label),
                    value = escape(&format_number(value))
                );
            }
            x += value / scale_total * bar_width;
        }
    }

    let _ = write!(
        svg,
        r#"<text x="0" y="{y:.1}" font-size="10" fill="var(--ink-muted)">Both bars are the two sides of the bus balance and are drawn to the same scale, so they match exactly. This is an energy balance, not an AC power-flow study.</text>"#,
        y = height - 6.0
    );
    svg.push_str("</svg>");
    svg
}

/// How a heatmap's numbers should be coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scale {
    /// Magnitude: one hue, light to dark. For quantities where only "how much"
    /// matters, such as peak import.
    Sequential,
    /// Polarity: two hues around a neutral zero. For net present value, where the
    /// sign is the entire question and a reader must see the boundary.
    Diverging,
}

/// What to draw a heatmap from.
///
/// A struct rather than eight positional arguments, because four of them are
/// `&str, &[String], &str, &[String]` in a row: a transposed pair would compile,
/// render, and be wrong, and nothing downstream would notice.
#[derive(Debug, Clone, Copy)]
pub struct Heatmap<'a> {
    /// Label of the axis running down the rows.
    pub row_label: &'a str,
    /// Row tick labels, top to bottom.
    pub row_values: &'a [String],
    /// Label of the axis running across the columns.
    pub column_label: &'a str,
    /// Column tick labels, left to right.
    pub column_values: &'a [String],
    /// Cell values, row-major.
    pub cells: &'a [f64],
    /// Unit symbol rendered next to each cell.
    pub unit: &'a str,
    /// How the numbers are coloured.
    pub scale: Scale,
    /// Overall width, in user units.
    pub width: f64,
}

/// A two-axis sensitivity matrix.
#[must_use]
pub fn heatmap(spec: &Heatmap<'_>) -> String {
    let Heatmap {
        row_label,
        row_values,
        column_label,
        column_values,
        cells,
        unit,
        scale,
        width,
    } = *spec;
    if cells.is_empty() || row_values.is_empty() || column_values.is_empty() {
        return String::new();
    }
    let left = 96.0;
    let top = 44.0;
    let cell_width = ((width - left - 16.0) / column_values.len() as f64).min(120.0);
    let cell_height = 40.0;
    let height = top + row_values.len() as f64 * cell_height + 44.0;

    let low = cells.iter().copied().fold(f64::MAX, f64::min);
    let high = cells.iter().copied().fold(f64::MIN, f64::max);
    let extent = low.abs().max(high.abs()).max(1e-9);

    let mut svg = String::with_capacity(4_096);
    let _ = write!(
        svg,
        r#"<svg class="chart" viewBox="0 0 {width:.0} {height:.0}" width="100%" role="img" xmlns="http://www.w3.org/2000/svg" aria-label="Sensitivity matrix">"#
    );
    let _ = write!(
        svg,
        r#"<text x="0" y="14" font-size="11" fill="var(--ink-secondary)">{row_label}  (rows)  |  {column_label}  (columns)  |  values in {unit}</text>"#,
        row_label = escape(row_label),
        column_label = escape(column_label),
        unit = escape(unit)
    );

    for (c, value) in column_values.iter().enumerate() {
        let _ = write!(
            svg,
            r#"<text x="{x:.1}" y="{y:.1}" text-anchor="middle" font-size="10" fill="var(--ink-muted)" style="font-variant-numeric:tabular-nums">{value}</text>"#,
            x = left + c as f64 * cell_width + cell_width / 2.0,
            y = top - 8.0,
            value = escape(value)
        );
    }

    for (r, row) in row_values.iter().enumerate() {
        let y = top + r as f64 * cell_height;
        let _ = write!(
            svg,
            r#"<text x="{x:.1}" y="{ty:.1}" text-anchor="end" font-size="10" fill="var(--ink-muted)" style="font-variant-numeric:tabular-nums">{row}</text>"#,
            x = left - 10.0,
            ty = y + cell_height / 2.0 + 3.5,
            row = escape(row)
        );

        for (c, _) in column_values.iter().enumerate() {
            let Some(value) = cells.get(r * column_values.len() + c) else {
                continue;
            };
            let fill = match scale {
                Scale::Sequential => {
                    let span = (high - low).max(1e-9);
                    let t = ((value - low) / span).clamp(0.0, 1.0);
                    let index = ((t * (palette::SEQUENTIAL.len() - 1) as f64).round() as usize)
                        .min(palette::SEQUENTIAL.len() - 1);
                    palette::SEQUENTIAL[index].to_string()
                }
                Scale::Diverging => {
                    let t = (value.abs() / extent).clamp(0.0, 1.0);
                    if value.abs() < extent * 0.02 {
                        // A neutral, unsaturated midpoint. A hue here would tell the
                        // reader that "about zero" is a kind of something.
                        "var(--gridline)".to_string()
                    } else {
                        let ramp = if *value >= 0.0 {
                            palette::DIVERGING_POSITIVE
                        } else {
                            palette::DIVERGING_NEGATIVE
                        };
                        let index =
                            ((t * (ramp.len() - 1) as f64).round() as usize).min(ramp.len() - 1);
                        ramp[index].to_string()
                    }
                }
            };
            // Values are printed in every cell, so the matrix is readable without
            // decoding the colour at all.
            let _ = write!(
                svg,
                r#"<rect x="{x:.1}" y="{y:.1}" width="{w:.1}" height="{h:.1}" rx="3" fill="{fill}" stroke="var(--surface)" stroke-width="2"><title>{row} / {col}: {value} {unit}</title></rect><text x="{tx:.1}" y="{ty:.1}" text-anchor="middle" font-size="10" font-weight="600" fill="var(--ink)" style="font-variant-numeric:tabular-nums">{shown}</text>"#,
                x = left + c as f64 * cell_width,
                w = cell_width,
                h = cell_height,
                row = escape(row),
                col = escape(&column_values[c]),
                value = escape(&format_number(*value)),
                unit = escape(unit),
                tx = left + c as f64 * cell_width + cell_width / 2.0,
                ty = y + cell_height / 2.0 + 3.5,
                shown = escape(&format_number(*value))
            );
        }
    }

    let _ = write!(
        svg,
        r#"<text x="0" y="{y:.1}" font-size="10" fill="var(--ink-muted)">Every cell is a full simulation, not an interpolation. Cell values are printed, so the matrix reads without relying on colour.</text>"#,
        y = height - 8.0
    );
    svg.push_str("</svg>");
    svg
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- The palette check, recomputed rather than trusted -----------------
    //
    // A port of the Machado-2009 colour-vision transform and the OKLCH/WCAG
    // measures. It exists so that a future change to a hex above cannot silently
    // produce a chart two readers in a hundred cannot decode.

    fn srgb(hex: &str) -> [f64; 3] {
        let hex = hex.trim_start_matches('#');
        let channel = |i: usize| {
            let byte = u8::from_str_radix(&hex[i..i + 2], 16).expect("palette hex is well-formed");
            f64::from(byte) / 255.0
        };
        [channel(0), channel(2), channel(4)]
    }

    fn linearize(hex: &str) -> [f64; 3] {
        srgb(hex).map(|c| {
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        })
    }

    fn relative_luminance(hex: &str) -> f64 {
        let [r, g, b] = linearize(hex);
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    fn contrast(a: &str, b: &str) -> f64 {
        let (mut hi, mut lo) = (relative_luminance(a), relative_luminance(b));
        if hi < lo {
            std::mem::swap(&mut hi, &mut lo);
        }
        (hi + 0.05) / (lo + 0.05)
    }

    fn oklch(hex: &str) -> (f64, f64) {
        let [r, g, b] = linearize(hex);
        let l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b).cbrt();
        let m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b).cbrt();
        let s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b).cbrt();
        let lightness = 0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s;
        let a_axis = 1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s;
        let b_axis = 0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s;
        (lightness, a_axis.hypot(b_axis))
    }

    fn lab(linear: [f64; 3]) -> [f64; 3] {
        let [r, g, b] = linear;
        let x = 0.4124564 * r + 0.3575761 * g + 0.1804375 * b;
        let y = 0.2126729 * r + 0.7151522 * g + 0.0721750 * b;
        let z = 0.0193339 * r + 0.1191920 * g + 0.9503041 * b;
        let f = |t: f64| {
            if t > 0.008856 {
                t.cbrt()
            } else {
                7.787 * t + 16.0 / 116.0
            }
        };
        let (fx, fy, fz) = (f(x / 0.95047), f(y), f(z / 1.08883));
        [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
    }

    /// Machado, Oliveira & Fernandes (2009), severity 1.0, in linear RGB.
    const PROTAN: [[f64; 3]; 3] = [
        [0.152286, 1.052583, -0.204868],
        [0.114503, 0.786281, 0.099216],
        [-0.003882, -0.048116, 1.051998],
    ];
    const DEUTAN: [[f64; 3]; 3] = [
        [0.367322, 0.860646, -0.227968],
        [0.280085, 0.672501, 0.047413],
        [-0.011820, 0.042940, 0.968881],
    ];

    fn simulate(hex: &str, matrix: [[f64; 3]; 3]) -> [f64; 3] {
        let [r, g, b] = linearize(hex);
        let apply = |row: [f64; 3]| (row[0] * r + row[1] * g + row[2] * b).clamp(0.0, 1.0);
        [apply(matrix[0]), apply(matrix[1]), apply(matrix[2])]
    }

    fn delta_e(a: &str, b: &str, matrix: [[f64; 3]; 3]) -> f64 {
        let (x, y) = (lab(simulate(a, matrix)), lab(simulate(b, matrix)));
        ((x[0] - y[0]).powi(2) + (x[1] - y[1]).powi(2) + (x[2] - y[2]).powi(2)).sqrt()
    }

    fn worst_cvd(a: &str, b: &str) -> f64 {
        delta_e(a, b, PROTAN).min(delta_e(a, b, DEUTAN))
    }

    fn colour(name: &str, dark: bool) -> &'static str {
        let entry = palette::ENTITIES
            .iter()
            .find(|(role, _, _)| *role == name)
            .unwrap_or_else(|| panic!("no palette entry named {name}"));
        if dark {
            entry.2
        } else {
            entry.1
        }
    }

    /// Every pair of colours that can share a surface, given the chart designs.
    ///
    /// A timeline panel holds two series. A stacked bar's segments touch only their
    /// neighbours. The outage panel puts served critical load beside the unmet
    /// remainder. That is the complete list — and it is short precisely *because*
    /// the charts were designed around this constraint.
    const TOUCHING: &[(&str, &str)] = &[
        // Timeline panels. Two series each — see the module docs for why.
        ("--c-load", "--c-generation"),
        ("--c-import", "--c-export"),
        ("--c-charge", "--c-discharge"),
        // Energy-in stack: generation | import | discharge. Only neighbours touch.
        ("--c-generation", "--c-import"),
        ("--c-import", "--c-discharge"),
        // Energy-out stack: load | export | charge.
        ("--c-load", "--c-export"),
        ("--c-export", "--c-charge"),
        // Outage panel: served critical load beside the unmet remainder.
        ("--c-load", "--c-unmet"),
    ];

    #[test]
    fn palette_survives_colour_vision_deficiency() {
        const TARGET: f64 = 12.0;
        for dark in [false, true] {
            for (a, b) in TOUCHING {
                let separation = worst_cvd(colour(a, dark), colour(b, dark));
                assert!(
                    separation >= TARGET,
                    "{a} and {b} collapse to dE {separation:.1} under protanopia or deuteranopia \
                     in {} mode; the target is {TARGET}. These two colours share a surface, so a \
                     reader with colour-vision deficiency could not tell them apart.",
                    if dark { "dark" } else { "light" }
                );
            }
        }
    }

    #[test]
    fn no_series_may_impersonate_the_reserved_failure_colour() {
        // Unmet critical load is the one thing on the page that means "this does not
        // work". If a routine series could be mistaken for it, the chart lies about the
        // worst thing it has to say.
        //
        // This test is why curtailment has no hue. Green — the only slot left after the
        // six entities — sits at dE 8.5 from this red under deuteranopia, and "we threw
        // away free energy" must never be mistakable for "the hospital went dark".
        const TARGET_STATUS: f64 = 12.0;
        let unmet = colour("--c-unmet", false);
        for (role, light, dark) in palette::ENTITIES {
            if *role == "--c-unmet" {
                continue;
            }
            for candidate in [light, dark] {
                let separation = worst_cvd(candidate, unmet);
                assert!(
                    separation >= TARGET_STATUS,
                    "{role} ({candidate}) is only dE {separation:.1} from the reserved unmet-critical \
                     red under colour-vision deficiency; the floor is {TARGET_STATUS}"
                );
            }
        }
    }

    #[test]
    fn every_categorical_hue_sits_in_its_lightness_band_and_clears_the_chroma_floor() {
        // Below the chroma floor a hue reads as grey and stops carrying identity;
        // outside the lightness band it stops reading as a mark on the surface.
        const CHROMA_FLOOR: f64 = 0.10;
        for (role, light, dark) in palette::ENTITIES {
            if *role == "--c-unmet" {
                continue; // reserved status, not a categorical slot
            }
            for (hex, band, mode) in [(light, (0.43, 0.77), "light"), (dark, (0.48, 0.67), "dark")]
            {
                let (lightness, chroma) = oklch(hex);
                assert!(
                    lightness >= band.0 && lightness <= band.1,
                    "{role} ({hex}) has OKLCH L {lightness:.3}, outside the {mode} band {band:?}"
                );
                assert!(
                    chroma >= CHROMA_FLOOR,
                    "{role} ({hex}) has OKLCH C {chroma:.3}, below the {CHROMA_FLOOR} floor: it \
                     would read as grey"
                );
            }
        }
    }

    #[test]
    fn the_energy_flow_bars_are_the_two_sides_of_the_bus_balance() {
        // The figure's entire claim is that the bars match. If a segment were added
        // that is not on the bus — curtailment, say, or losses — they would not, and
        // the figure would quietly become a lie. The roles it draws are fixed here so
        // that cannot happen by accident.
        let inputs = [Role::Generation, Role::GridImport, Role::BatteryDischarge];
        let outputs = [Role::Load, Role::GridExport, Role::BatteryCharge];
        for role in inputs.iter().chain(&outputs) {
            assert!(
                !matches!(role, Role::UnmetCritical),
                "the failure colour is not a flow"
            );
        }
        assert_eq!(inputs.len() + outputs.len(), 6, "six flows, six hues");
    }

    #[test]
    fn dark_mode_marks_all_clear_the_contrast_minimum() {
        // Light mode has three hues below 3:1 by construction, which is why the
        // report always ships the data table and direct labels. Dark mode has none,
        // and must keep it that way.
        for (role, _, dark) in palette::ENTITIES {
            let ratio = contrast(dark, palette::SURFACE_DARK);
            assert!(
                ratio >= 3.0,
                "{role} ({dark}) is only {ratio:.2}:1 against the dark surface"
            );
        }
    }

    #[test]
    fn the_sequential_ramp_is_monotone_in_lightness() {
        // A magnitude ramp that is not monotone is a rainbow, and a rainbow invents
        // boundaries the data does not have.
        let lightness: Vec<f64> = palette::SEQUENTIAL.iter().map(|c| oklch(c).0).collect();
        for pair in lightness.windows(2) {
            assert!(
                pair[0] > pair[1],
                "the sequential ramp is not monotonically darkening: {pair:?}"
            );
        }
    }

    #[test]
    fn ramp_css_carries_every_ramp_step_and_leaves_the_report_palette_alone() {
        let css = ramp_css();
        for index in 0..palette::SEQUENTIAL.len() {
            assert!(css.contains(&format!("--seq-{index}:")));
        }
        for index in 0..palette::DIVERGING_POSITIVE.len() {
            assert!(css.contains(&format!("--div-pos-{index}:")));
            assert!(css.contains(&format!("--div-neg-{index}:")));
        }
        assert!(css.contains("--div-mid:"));
        assert!(
            !palette_css().contains("--seq-"),
            "the report does not read these"
        );
    }

    #[test]
    fn palette_css_defines_every_role_in_both_modes() {
        let css = palette_css();
        for (name, _, _) in palette::ENTITIES.iter().chain(palette::CHROME) {
            assert!(css.contains(name), "{name} is missing from the palette CSS");
        }
        assert!(css.contains("prefers-color-scheme:dark"));
        // Every role a chart can ask for must resolve.
        for role in [
            Role::Load,
            Role::Generation,
            Role::GridImport,
            Role::GridExport,
            Role::BatteryCharge,
            Role::BatteryDischarge,
            Role::UnmetCritical,
            Role::StateOfCharge,
        ] {
            let var = role
                .css_var()
                .trim_start_matches("var(")
                .trim_end_matches(')');
            assert!(css.contains(var), "{var} is not defined");
        }
    }

    // ---- Decimation --------------------------------------------------------

    #[test]
    fn decimation_never_hides_a_peak() {
        // The whole point: a spike in the middle of a flat series must survive,
        // because that spike is what sets the demand charge.
        let mut values = vec![10.0; 1000];
        values[437] = 500.0;

        let buckets = decimate(&values, 20);
        assert_eq!(buckets.len(), 20);
        let highest = buckets.iter().map(|b| b.max).fold(f64::MIN, f64::max);
        assert!(
            (highest - 500.0).abs() < 1e-12,
            "the peak was lost: highest bucket maximum was {highest}"
        );
        // ...and the mean is not distorted into claiming the spike was typical.
        let peak_bucket = buckets.iter().find(|b| b.max > 400.0).unwrap();
        assert!(
            peak_bucket.mean < 30.0,
            "the bucket mean overstates a single spike"
        );
    }

    #[test]
    fn decimation_covers_every_point_exactly_once() {
        for len in [1usize, 2, 7, 99, 1000, 35_040] {
            for target in [1usize, 3, 10, 1200] {
                let values: Vec<f64> = (0..len).map(|i| i as f64).collect();
                let buckets = decimate(&values, target);
                assert!(!buckets.is_empty(), "len {len} target {target}");
                assert!(
                    buckets.len() <= target.max(1).min(len),
                    "len {len} target {target}"
                );
                // The extremes of the whole series must appear somewhere.
                let min = buckets.iter().map(|b| b.min).fold(f64::MAX, f64::min);
                let max = buckets.iter().map(|b| b.max).fold(f64::MIN, f64::max);
                assert!(
                    (min - 0.0).abs() < 1e-9,
                    "len {len} target {target}: lost the minimum"
                );
                assert!(
                    (max - (len - 1) as f64).abs() < 1e-9,
                    "len {len} target {target}: lost the maximum"
                );
            }
        }
    }

    #[test]
    fn a_short_series_is_not_decimated_at_all() {
        let values = vec![1.0, 2.0, 3.0];
        let buckets = decimate(&values, 1200);
        assert_eq!(buckets.len(), 3);
        assert_eq!(buckets[1].min, 2.0);
        assert_eq!(buckets[1].max, 2.0);
        assert!(!is_decimated(3));
        assert!(is_decimated(CHART_MAX_POINTS + 1));
    }

    #[test]
    fn decimating_nothing_yields_nothing() {
        assert!(decimate(&[], 10).is_empty());
    }

    // ---- Axes and escaping -------------------------------------------------

    #[test]
    fn ticks_land_on_round_numbers() {
        let ticks = nice_ticks(0.0, 100.0, 5);
        assert!(ticks.contains(&0.0) && ticks.contains(&100.0), "{ticks:?}");
        assert!(
            ticks.iter().all(|t| (t / 20.0).fract().abs() < 1e-9),
            "{ticks:?}"
        );

        // A flat series must not produce an infinite axis.
        assert_eq!(nice_ticks(5.0, 5.0, 5), vec![5.0]);
        // Nor must a degenerate one loop forever.
        assert!(!nice_ticks(f64::NAN, 1.0, 5).is_empty());
        assert!(nice_ticks(-50.0, 50.0, 4).len() <= 8);
    }

    #[test]
    fn markup_is_escaped_so_a_name_cannot_become_an_element() {
        let hostile = r#"<script>alert("x")</script>&'"#;
        let escaped = escape(hostile);
        assert!(!escaped.contains('<'), "{escaped}");
        assert!(!escaped.contains('>'), "{escaped}");
        assert!(!escaped.contains('"'), "{escaped}");
        assert_eq!(
            escaped,
            "&lt;script&gt;alert(&quot;x&quot;)&lt;/script&gt;&amp;&#39;"
        );
    }

    #[test]
    fn numbers_are_formatted_for_a_human() {
        assert_eq!(format_number(0.0), "0");
        assert_eq!(format_number(1.5), "1.5");
        assert_eq!(format_number(250.0), "250");
        assert_eq!(format_number(35_000.0), "35k");
        assert_eq!(format_number(2_500_000.0), "2.5M");
        assert_eq!(format_number(0.145), "0.145");
    }
}
