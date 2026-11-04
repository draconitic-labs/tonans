/*
 * The rules the page depends on, with no page in them, so that
 * `ui-tests/logic.test.mjs` can run them under Node. Nothing here touches the DOM,
 * the network, or anything the user has not handed it.
 */

// ---------------------------------------------------------------------------
// File intake
// ---------------------------------------------------------------------------

/**
 * A file name as an HTTP header value. `fetch` refuses any character above U+00FF, and
 * WebKit and WebView2 disagree about the ones below it, so the name travels as ASCII.
 * The server's `decode_file_name` undoes this.
 */
export function encodeFileNameHeader(name) {
  // Windows allows a file name that is not valid Unicode, and encodeURIComponent throws on one.
  return encodeURIComponent(name.replace(UNPAIRED_SURROGATE, '\uFFFD'));
}

const UNPAIRED_SURROGATE = /[\uD800-\uDBFF](?![\uDC00-\uDFFF])|(?<![\uD800-\uDBFF])[\uDC00-\uDFFF]/g;

export function formatBytes(bytes) {
  const mib = bytes / (1024 * 1024);
  if (mib >= 1024) return `${Number((mib / 1024).toFixed(1))} GiB`;
  if (mib >= 1) return `${Number(mib.toFixed(1))} MiB`;
  return `${bytes.toLocaleString('en-US')} bytes`;
}

/** Why a file cannot be taken, judged from its size alone, or null if it can. */
export function fileTooLarge(name, size, limit) {
  if (size <= limit) return null;
  return `"${name}" is ${formatBytes(size)}. Tonans reads CSV files up to ${formatBytes(limit)}, so none of it was read.`;
}

// ---------------------------------------------------------------------------
// Which state an answer is about
// ---------------------------------------------------------------------------

export const stampMatches = (a, b) =>
  Boolean(a && b) && a.project === b.project && a.revision === b.revision;

/**
 * Send a request tagged with the project and revision it was made from, and say whether
 * its answer still describes what is on screen. A run takes seconds, and in that time the
 * user can open another project or save a change. `send` resolves with `{ body, revision }`,
 * or null if it failed and has already said so.
 */
export async function sendTagged(currentStamp, send) {
  const stamp = currentStamp();
  const reply = await send(stamp);
  if (!reply) return { stamp, reply: null, late: false };
  const answered = { project: stamp.project, revision: reply.revision ?? stamp.revision };
  return { stamp: answered, reply, late: !stampMatches(answered, currentStamp()) };
}

/** The one place results are called current. */
export function resultsStatus(resultStamp, currentStamp, { unsaved = false, changedElsewhere = false } = {}) {
  if (!resultStamp) return 'Not run';
  if (changedElsewhere || !stampMatches(resultStamp, currentStamp)) return 'Out of date';
  return unsaved ? 'From the last save' : 'Current';
}

// ---------------------------------------------------------------------------
// Scenario edits
// ---------------------------------------------------------------------------

/** Read a field that may be blank on purpose. A blank is null; unreadable is not blank. */
export function readOptionalNumber(raw, badInput = false) {
  const text = String(raw).trim();
  if (badInput) return { ok: false };
  if (text === '') return { ok: true, value: null };
  const value = Number(text);
  return Number.isFinite(value) ? { ok: true, value } : { ok: false };
}

export const TARIFF_PRESETS = [
  { name: 'Prices from my data', needsSeries: true, apply(t) {
    t.import = { source: 'series' };
    t.export = { source: 'series' };
  } },
  { name: 'Flat energy price', apply(t) {
    t.import = { source: 'flat', price_per_kwh_usd: 0.15 };
    t.export = { source: 'flat', price_per_kwh_usd: 0.05 };
    // A flat energy price is the whole tariff. A demand charge left over from an earlier
    // choice would be a cost the name of this button says is not there.
    t.demand_prices_per_kw_month_usd = {};
  } },
  { name: 'Flat energy plus a demand charge', apply(t, periods) {
    t.import = { source: 'flat', price_per_kwh_usd: 0.10 };
    t.export = { source: 'flat', price_per_kwh_usd: 0.04 };
    t.demand_prices_per_kw_month_usd = Object.fromEntries(periods.map((period) => [period, 15]));
  } },
  { name: 'No export credit', apply(t) {
    t.export = { source: 'flat', price_per_kwh_usd: 0 };
    t.export_price_multiplier = 1;
  } },
];

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/** Why there is no payback figure: nothing to repay is not the same as never repaying it. */
export function noPaybackText(capexUsd, notRepaid) {
  return capexUsd > 0 ? notRepaid : 'No capital to repay';
}

/** What the annual figure on a tile really covers. */
export function annualNote(multiplier, hours) {
  if (Math.abs(multiplier - 1) <= 0.01) return 'over the simulated year';
  return `scaled ${multiplier.toLocaleString('en-US', { maximumFractionDigits: 3 })} times from ` +
    `${hours.toLocaleString('en-US', { maximumFractionDigits: 1 })} hours of data`;
}

// ---------------------------------------------------------------------------
// Sensitivity
// ---------------------------------------------------------------------------

/** Read a comma-separated list strictly: one unreadable entry refuses the whole list. */
export function parseValues(raw) {
  const parts = raw.split(',').map((part) => part.trim()).filter((part) => part !== '');
  const values = parts.map(Number);
  const bad = parts.filter((_, index) => !Number.isFinite(values[index]));
  return { ok: bad.length === 0, bad, values };
}

/** Exactly, because this number is compared with results to the cent. */
export function formatAxis(value, locale = undefined) {
  return value.toLocaleString(locale, { maximumFractionDigits: 6 });
}

/** A price is not something the candidate chooses, so the study moves the baseline too. */
const PRICE_AXES = ['import_price_multiplier', 'demand_price_per_kw_month_usd'];
export const hasPriceAxis = (axes) => axes.some((axis) => PRICE_AXES.includes(axis.param));

/** The scenario's own installed cost per kWh, with no rounding, or null if it has none. */
export function exactCostRate(scenario) {
  const nameplate = scenario?.battery?.nameplate_energy_kwh;
  const capex = scenario?.economics?.capex_usd;
  return nameplate > 0 && capex > 0 ? capex / nameplate : null;
}

/** Values around the scenario's own, so a study starts from where the scenario is. */
export function axisDefaults(param, scenario) {
  // The scenario's own value stays exact, so that cell can be compared with its results.
  const around = (value, factors) =>
    factors.map((f) => (f === 1 ? value : Number((value * f).toFixed(0)))).join(', ');
  const b = scenario?.battery;
  switch (param) {
    case 'battery_energy_kwh': return b ? around(b.nameplate_energy_kwh, [0.5, 1, 1.5, 2]) : '200, 400, 600, 800';
    case 'battery_power_kw': return b ? around(b.max_discharge_kw, [0.5, 1, 1.5]) : '50, 100, 150';
    case 'reserve_soc_pct': return '10, 20, 30, 50';
    case 'initial_capacity_pct': return '70, 80, 90, 100';
    case 'import_price_multiplier': return '0.8, 1, 1.2, 1.5';
    case 'demand_price_per_kw_month_usd': return '5, 10, 15, 20';
    case 'calendar_fade_pct_per_year': return '0.5, 1, 2, 3';
    case 'capex_usd': return around(scenario?.economics.capex_usd || 100000, [0.5, 0.75, 1, 1.25]);
    case 'discount_rate_pct_per_year': return '4, 6, 8, 10';
    default: return '';
  }
}

/**
 * The study form, brought into line with the project as it is now. Called before every
 * draw and every run, so nothing on the form can outlive the edit that made it wrong.
 * What the user typed stays, unless the thing it was typed about has gone.
 */
export function studySettings(previous, scenarios, baselineId) {
  const kept = previous ? scenarios.find((s) => s.id === previous.scenario) : null;
  const scenario = kept
    ?? scenarios.find((s) => s.battery && s.id !== baselineId)
    ?? scenarios.find((s) => s.battery)
    ?? scenarios[0];
  if (!scenario) return null;
  const settings = kept ? previous : {
    scenario: scenario.id,
    p1: 'battery_energy_kwh', v1: '', v1Edited: false,
    p2: 'battery_power_kw', v2: '', v2Edited: false,
    scaleChoice: null, rateOverride: null, basis: null,
  };
  if (!settings.v1Edited) settings.v1 = axisDefaults(settings.p1, scenario);
  if (!settings.v2Edited) settings.v2 = settings.p2 ? axisDefaults(settings.p2, scenario) : '';

  // A rate typed for one cost and size says nothing about another.
  const basis = `${scenario.id}|${scenario.economics?.capex_usd}|${scenario.battery?.nameplate_energy_kwh}`;
  if (basis !== settings.basis) {
    settings.basis = basis;
    settings.rateOverride = null;
  }
  const exact = exactCostRate(scenario);
  settings.exactRate = exact;
  settings.rate = settings.rateOverride ?? (exact === null ? '' : String(exact));
  settings.scale = settings.scaleChoice ?? exact !== null;
  return settings;
}

export function chooseStudyScenario(settings, id) {
  Object.assign(settings, {
    scenario: id, v1Edited: false, v2Edited: false, scaleChoice: null, rateOverride: null,
  });
}

export function chooseStudyAxis(settings, which, param) {
  settings[`p${which}`] = param;
  settings[`v${which}Edited`] = false;
}
