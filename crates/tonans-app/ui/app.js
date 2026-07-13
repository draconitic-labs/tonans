/*
 * Tonans UI.
 *
 * Everything here runs on the user's machine and talks only to the loopback server
 * that served it. There is no analytics, no remote asset, and no code path that
 * fetches anything from the network.
 *
 * Two rules this file keeps, deliberately:
 *
 *  1. **Nothing is ever assigned to innerHTML.** Every string that reaches the DOM
 *     — a scenario name, a warning, a source filename — is set through textContent.
 *     Some of those strings came out of a CSV that Tonans does not control, and a
 *     report is a document other people read. `assets.rs` has a test that fails the
 *     build if `innerHTML` appears in this file.
 *
 *  2. **Colour is never written down here.** Every hue is read from a CSS custom
 *     property defined in /palette.css, which is generated from the one validated
 *     table in tonans-core. Canvas needs literal colours, so it resolves them at
 *     draw time from the same properties — which means dark mode and the exported
 *     report cannot drift away from the UI.
 */

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

const state = {
  token: null,
  session: null,
  projects: [],
  project: null,
  run: null,
  timelines: {},
  study: null,
  studyRequest: null,
  bundle: null,
  view: 'data',
  upload: null,
};

/** Take the launch token out of the URL and keep it in memory only. */
function claimToken() {
  const params = new URLSearchParams(window.location.search);
  const token = params.get('token');
  if (token) {
    state.token = token;
    // Strip it from the address bar immediately: the token should not sit in
    // history, in a bookmark, or on screen over someone's shoulder.
    window.history.replaceState({}, '', window.location.pathname);
  }
}

async function api(path, { method = 'GET', body, raw, filename } = {}) {
  const headers = { Authorization: `Bearer ${state.token}` };
  let payload;
  if (raw) {
    headers['Content-Type'] = 'text/csv';
    headers['X-Tonans-Filename'] = filename || 'upload.csv';
    payload = raw;
  } else if (body !== undefined) {
    headers['Content-Type'] = 'application/json';
    payload = JSON.stringify(body);
  }

  const response = await fetch(path, { method, headers, body: payload });
  const text = await response.text();
  let parsed = null;
  try { parsed = text ? JSON.parse(text) : null; } catch { parsed = null; }

  if (!response.ok) {
    const error = new Error(parsed?.message || `Request failed (${response.status})`);
    error.code = parsed?.code || `http.${response.status}`;
    throw error;
  }
  return parsed;
}

// ---------------------------------------------------------------------------
// DOM helpers — textContent only, never innerHTML
// ---------------------------------------------------------------------------

function el(tag, props = {}, children = []) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (value === undefined || value === null || value === false) continue;
    if (key === 'text') node.textContent = String(value);
    else if (key === 'class') node.className = value;
    else if (key === 'style') Object.assign(node.style, value);
    else if (key === 'dataset') Object.assign(node.dataset, value);
    else if (key.startsWith('on')) node.addEventListener(key.slice(2).toLowerCase(), value);
    else node.setAttribute(key, String(value));
  }
  for (const child of [].concat(children)) {
    if (child === null || child === undefined || child === false) continue;
    node.appendChild(typeof child === 'string' ? document.createTextNode(child) : child);
  }
  return node;
}

function clear(node) { while (node.firstChild) node.removeChild(node.firstChild); }

function css(name) {
  return getComputedStyle(document.documentElement).getPropertyValue(name).trim();
}

/** The one place an entity's colour is looked up in the browser. */
const COLOUR = {
  load: '--c-load',
  critical: '--c-load',
  generation: '--c-generation',
  import: '--c-import',
  export: '--c-export',
  charge: '--c-charge',
  discharge: '--c-discharge',
  curtailed: '--c-curtailed',
  loss: '--c-loss',
  unmet: '--c-unmet',
  soc: '--c-load',
};
const paint = (role) => css(COLOUR[role] || '--ink');

// ---------------------------------------------------------------------------
// Formatting. Every displayed quantity carries its unit.
// ---------------------------------------------------------------------------

const fmt = {
  n(value, digits = 1) {
    if (value === null || value === undefined || Number.isNaN(value)) return '—';
    const abs = Math.abs(value);
    if (abs >= 1e6) return `${(value / 1e6).toFixed(2)}M`;
    if (abs >= 1e4) return `${(value / 1e3).toFixed(1)}k`;
    if (abs < 1e-9) return '0';
    return value.toFixed(digits);
  },
  kwh: (v) => `${fmt.n(v, 0)} kWh`,
  kw: (v) => `${fmt.n(v, 1)} kW`,
  pct: (v) => `${fmt.n(v, 1)} %`,
  usd(v) {
    if (v === null || v === undefined) return '—';
    const abs = Math.abs(v);
    const body = abs >= 1e4 ? `${(v / 1e3).toFixed(1)}k` : v.toFixed(2);
    return `${body} USD`;
  },
  years: (v) => (v === null || v === undefined ? 'never' : `${v.toFixed(1)} years`),
  when: (iso) => new Date(iso).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' }),
};

function toast(message, code, ok = false) {
  const node = document.getElementById('toast');
  clear(node);
  if (code) node.appendChild(el('span', { class: 't-code', text: code }));
  node.appendChild(document.createTextNode(message));
  node.className = ok ? 'toast ok' : 'toast';
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => node.classList.add('hidden'), ok ? 3200 : 9000);
}

async function guard(work) {
  try {
    return await work();
  } catch (error) {
    toast(error.message, error.code);
    return null;
  }
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

async function boot() {
  claimToken();
  if (!state.token) {
    toast('No launch token. Open the link Tonans printed when it started.', 'server.unauthorized');
    return;
  }

  const session = await guard(() => api('/api/session'));
  if (!session) return;
  state.session = session;

  document.getElementById('tier-badge').textContent = `${session.tier} tier`;
  document.getElementById('version-badge').textContent =
    `v${session.app_version} · model ${session.model_version} · schema v${session.schema_version}`;
  document.getElementById('workspace-path').textContent = session.workspace;

  const limits = session.entitlements;
  document.getElementById('entitlement-note').textContent =
    `${session.tier} tier: ${limits.max_user_projects ?? 'unlimited'} user project(s), ` +
    `${limits.max_scenarios_per_project ?? 'unlimited'} scenarios each, ` +
    `${limits.max_sensitivity_cells} sensitivity cells. ` +
    `The bundled demonstration project does not count against this.`;

  document.getElementById('quit').addEventListener('click', async () => {
    await guard(() => api('/api/shutdown', { method: 'POST' }));
    document.body.textContent = 'Tonans has stopped. You can close this tab.';
  });

  document.getElementById('new-project').addEventListener('submit', async (event) => {
    event.preventDefault();
    const name = document.getElementById('new-name').value.trim();
    const timezone = document.getElementById('new-timezone').value.trim();
    const project = await guard(() => api('/api/projects', { method: 'POST', body: { name, timezone } }));
    if (!project) return;
    document.getElementById('new-name').value = '';
    await refreshProjects();
    await openProject(project.name);
    toast(`Created "${project.name}".`, null, true);
  });

  for (const tab of document.querySelectorAll('.tab')) {
    tab.addEventListener('click', () => show(tab.dataset.view));
  }

  await refreshProjects();
  if (state.projects.length) await openProject(state.projects[0].name);
}

async function refreshProjects() {
  const listing = await guard(() => api('/api/projects'));
  if (!listing) return;
  state.projects = listing.projects;

  const list = document.getElementById('project-list');
  clear(list);
  for (const entry of listing.projects) {
    const button = el('button', {
      type: 'button',
      'aria-current': state.project && state.project.name === entry.name ? 'true' : 'false',
      onclick: () => openProject(entry.name),
    }, [
      el('span', { class: 'p-name', text: entry.name }),
      el('span', {
        class: 'p-meta',
        text: `${entry.scenario_count} scenario(s) · ${entry.has_dataset ? 'data imported' : 'no data yet'}`,
      }),
    ]);
    list.appendChild(el('li', {}, button));
  }
}

async function openProject(name) {
  const project = await guard(() => api(`/api/projects/${encodeURIComponent(name)}`));
  if (!project) return;
  state.project = project;
  state.run = null;
  state.study = null;
  state.studyRequest = null;
  state.bundle = null;
  state.upload = null;

  document.getElementById('empty-state').classList.add('hidden');
  document.getElementById('project-header').classList.remove('hidden');
  document.getElementById('tabs').classList.remove('hidden');
  document.getElementById('project-name').textContent = project.name;

  const dataset = project.dataset;
  document.getElementById('project-summary').textContent = dataset
    ? `${dataset.interval_count.toLocaleString()} intervals of ${dataset.interval_minutes}-minute data · ` +
      `${project.site.timezone} · ${project.scenarios.length} scenario(s) · imported from ${dataset.source.filename}`
    : `${project.site.timezone} · no data imported yet`;

  await refreshProjects();
  show(dataset ? 'scenarios' : 'data');
}

function show(view) {
  state.view = view;
  for (const tab of document.querySelectorAll('.tab')) {
    tab.setAttribute('aria-selected', tab.dataset.view === view ? 'true' : 'false');
  }
  for (const name of ['data', 'scenarios', 'results', 'sensitivity', 'export']) {
    document.getElementById(`view-${name}`).classList.toggle('hidden', name !== view);
  }
  const render = { data: renderData, scenarios: renderScenarios, results: renderResults,
                   sensitivity: renderSensitivity, export: renderExport }[view];
  if (render) render();
}

async function saveProject() {
  const saved = await guard(() => api(`/api/projects/${encodeURIComponent(state.project.name)}`, {
    method: 'PUT',
    body: state.project,
  }));
  if (saved) {
    state.project = saved;
    // Results were produced from the old assumptions and are now stale. Throwing
    // them away is the only honest thing to do: a chart next to assumptions it was
    // not produced from is worse than no chart.
    state.run = null;
    state.study = null;
    state.studyRequest = null;
    toast('Saved. Re-run to refresh results.', null, true);
  }
  return saved;
}

// ---------------------------------------------------------------------------
// 1 — Data
// ---------------------------------------------------------------------------

const ROLES = [
  ['load', 'Load (required)', ['kilowatt', 'watt', 'megawatt', 'kilowatt_hour_per_interval', 'watt_hour_per_interval']],
  ['critical_load', 'Critical load', ['kilowatt', 'watt', 'megawatt', 'kilowatt_hour_per_interval', 'watt_hour_per_interval']],
  ['generation', 'On-site generation', ['kilowatt', 'watt', 'megawatt', 'kilowatt_hour_per_interval', 'watt_hour_per_interval']],
  ['import_price', 'Import price', ['usd_per_kilowatt_hour', 'cents_per_kilowatt_hour', 'usd_per_megawatt_hour']],
  ['export_price', 'Export credit', ['usd_per_kilowatt_hour', 'cents_per_kilowatt_hour', 'usd_per_megawatt_hour']],
  ['temperature', 'Ambient temperature', ['celsius', 'fahrenheit']],
];

const UNIT_LABELS = {
  kilowatt: 'kW', watt: 'W', megawatt: 'MW',
  kilowatt_hour_per_interval: 'kWh per interval', watt_hour_per_interval: 'Wh per interval',
  usd_per_kilowatt_hour: 'USD/kWh', cents_per_kilowatt_hour: 'cents/kWh', usd_per_megawatt_hour: 'USD/MWh',
  celsius: '°C', fahrenheit: '°F',
};

function renderData() {
  const root = document.getElementById('view-data');
  clear(root);
  const project = state.project;

  if (project.dataset) {
    root.appendChild(datasetSummary(project.dataset));
  }

  const card = el('div', { class: 'card' });
  card.appendChild(el('h3', { text: project.dataset ? 'Replace the interval data' : 'Import interval data' }));
  card.appendChild(el('p', { class: 'muted', text:
    'Tonans reads the file you choose and nothing else. It does not guess a delimiter, infer the ' +
    'interval from the first two rows, or assume a column is kilowatts because it is called "load". ' +
    'You state the timezone, the interval, and the unit of every column — and every column in the ' +
    'file must be either mapped or explicitly ignored.' }));

  const picker = el('input', { type: 'file', accept: '.csv,text/csv', id: 'csv-file' });
  picker.addEventListener('change', async (event) => {
    const file = event.target.files[0];
    if (!file) return;
    const bytes = new Uint8Array(await file.arrayBuffer());
    const receipt = await guard(() => api('/api/upload', { method: 'POST', raw: bytes, filename: file.name }));
    if (!receipt) return;
    state.upload = receipt;
    renderData();
  });
  card.appendChild(el('div', { class: 'row', style: { marginTop: '10px' } }, picker));
  root.appendChild(card);

  if (state.upload) root.appendChild(mappingCard(state.upload));
}

function datasetSummary(dataset) {
  const card = el('div', { class: 'card' });
  card.appendChild(el('h3', { text: 'Imported data' }));

  const rows = [
    ['Source file', dataset.source.filename],
    ['Digest', dataset.source.sha256],
    ['Size', `${dataset.source.size_bytes.toLocaleString()} bytes · ${dataset.source.row_count.toLocaleString()} data rows`],
    ['Interval', `${dataset.interval_minutes} minutes · ${dataset.interval_count.toLocaleString()} intervals`],
    ['Starts', fmt.when(dataset.start_utc)],
    ['Gap policy', dataset.gap_policy.mode.replace(/_/g, ' ')],
    ['Synthesized intervals', dataset.synthesized_interval_count === 0
      ? 'none — every interval is a measurement'
      : `${dataset.synthesized_interval_count} of ${dataset.interval_count} were NOT measured`],
    ['Imported', fmt.when(dataset.source.imported_utc)],
  ];
  const table = el('table');
  const body = el('tbody');
  for (const [key, value] of rows) {
    body.appendChild(el('tr', {}, [
      el('th', { scope: 'row', text: key }),
      el('td', { class: key === 'Digest' ? 'mono tiny' : '', text: value }),
    ]));
  }
  table.appendChild(body);
  card.appendChild(el('div', { class: 'scroll' }, table));

  card.appendChild(el('h4', { text: 'Column mapping' }));
  const map = el('table');
  const mapBody = el('tbody');
  for (const [role, column] of Object.entries(dataset.source.column_mapping)) {
    mapBody.appendChild(el('tr', {}, [el('th', { scope: 'row', text: role }), el('td', { text: column })]));
  }
  map.appendChild(mapBody);
  card.appendChild(el('div', { class: 'scroll' }, map));

  if (dataset.import_warnings?.length) card.appendChild(ledger(dataset.import_warnings));
  return card;
}

function mappingCard(receipt) {
  const card = el('div', { class: 'card' });
  card.appendChild(el('h3', { text: `Map the columns of ${receipt.filename}` }));
  card.appendChild(el('p', { class: 'muted', text:
    `${receipt.row_count.toLocaleString()} data rows, ${receipt.headers.length} columns. ` +
    `Digest ${receipt.sha256}. Nothing has been interpreted yet.` }));

  const settings = el('div', { class: 'grid' }, [
    field('Timezone (IANA)', el('input', { type: 'text', id: 'imp-tz', value: state.project.site.timezone })),
    field('Interval (minutes)', el('input', { type: 'number', id: 'imp-interval', value: '60', min: '1', max: '60' })),
    field('Missing intervals', select('imp-gap', [
      ['reject', 'Reject the file (default, and the safe choice)'],
      ['fill_zero', 'Fill with zero — the filled intervals are NOT measurements'],
      ['hold_last_value', 'Hold the previous value — the filled intervals are NOT measurements'],
    ], 'reject')),
    field('Longest run to fill', el('input', { type: 'number', id: 'imp-gap-max', value: '2', min: '1', max: '24' })),
  ]);
  card.appendChild(settings);

  const table = el('table', { class: 'map-table' });
  table.appendChild(el('thead', {}, el('tr', {}, [
    el('th', { text: 'Column in your file' }),
    el('th', { text: 'First value' }),
    el('th', { text: 'Means' }),
    el('th', { text: 'Unit' }),
  ])));
  const body = el('tbody');

  receipt.headers.forEach((header, index) => {
    const roleSelect = select(`role-${index}`, [
      ['__unmapped', 'Choose meaning…'],
      ['__timestamp', 'Timestamp'],
      ...ROLES.map(([id, label]) => [id, label]),
      ['demand_period', 'Demand-charge period label'],
      ['__ignore', 'Ignore this column'],
    ], '__unmapped');

    const unitSelect = select(`unit-${index}`, [['', '—']], '');
    const syncUnits = () => {
      const role = roleSelect.value;
      const spec = ROLES.find(([id]) => id === role);
      clear(unitSelect);
      if (!spec) {
        unitSelect.appendChild(el('option', { value: '', text: '—' }));
        unitSelect.disabled = true;
        return;
      }
      unitSelect.disabled = false;
      unitSelect.appendChild(el('option', { value: '', text: 'Choose unit…' }));
      for (const unit of spec[2]) {
        unitSelect.appendChild(el('option', { value: unit, text: UNIT_LABELS[unit] }));
      }
      unitSelect.value = '';
    };
    roleSelect.addEventListener('change', syncUnits);
    syncUnits();

    body.appendChild(el('tr', {}, [
      el('td', { class: 'mono', text: header }),
      el('td', { class: 'sample', text: receipt.sample_rows[0]?.[index] ?? '' }),
      el('td', {}, roleSelect),
      el('td', {}, unitSelect),
    ]));
  });
  table.appendChild(body);
  card.appendChild(el('div', { class: 'scroll' }, table));

  const go = el('button', { class: 'primary', type: 'button', text: 'Import' });
  go.addEventListener('click', () => runImport(receipt));
  card.appendChild(el('div', { class: 'row', style: { marginTop: '12px' } }, [
    go,
    el('button', { type: 'button', text: 'Cancel', onclick: () => { state.upload = null; renderData(); } }),
  ]));
  return card;
}

async function runImport(receipt) {
  const series = {};
  const ignore = [];
  const unresolved = [];
  const missingUnits = [];
  let timestamp = null;
  let demandPeriod = null;

  receipt.headers.forEach((header, index) => {
    const role = document.getElementById(`role-${index}`).value;
    const unit = document.getElementById(`unit-${index}`).value;
    if (role === '__unmapped') unresolved.push(header);
    else if (role === '__timestamp') timestamp = header;
    else if (role === '__ignore') ignore.push(header);
    else if (role === 'demand_period') demandPeriod = header;
    else {
      if (!unit) missingUnits.push(header);
      series[role] = { header, unit };
    }
  });

  if (unresolved.length) {
    toast(`Choose a meaning or Ignore for: ${unresolved.join(', ')}`, 'ui.unmapped_columns');
    return;
  }
  if (missingUnits.length) {
    toast(`Choose a unit for: ${missingUnits.join(', ')}`, 'ui.missing_units');
    return;
  }
  if (!timestamp) { toast('One column must be the timestamp.', 'ui.no_timestamp'); return; }
  if (!series.load) { toast('One column must be the load. Tonans cannot run without it.', 'ui.no_load'); return; }

  const mode = document.getElementById('imp-gap').value;
  const max = Number(document.getElementById('imp-gap-max').value);
  const gap_policy = mode === 'reject' ? { mode: 'reject' } : { mode, max_consecutive_intervals: max };

  const result = await guard(() => api(`/api/projects/${encodeURIComponent(state.project.name)}/import`, {
    method: 'POST',
    body: {
      upload_id: receipt.upload_id,
      request: {
        filename: receipt.filename,
        timezone: document.getElementById('imp-tz').value.trim(),
        interval_minutes: Number(document.getElementById('imp-interval').value),
        gap_policy,
        mapping: { timestamp, series, demand_period: demandPeriod, ignore_columns: ignore },
      },
    },
  }));
  if (!result) return;

  state.project = result.project;
  state.upload = null;
  state.run = null;
  toast(`Imported ${result.import.interval_count.toLocaleString()} intervals.`, null, true);
  await openProject(state.project.name);
}

// ---------------------------------------------------------------------------
// 2 — Scenarios
// ---------------------------------------------------------------------------

function renderScenarios() {
  const root = document.getElementById('view-scenarios');
  clear(root);
  const project = state.project;

  const head = el('div', { class: 'card' });
  head.appendChild(el('h3', { text: 'Scenarios' }));
  head.appendChild(el('p', { class: 'muted', text:
    'Each scenario is self-describing: what you read here is exactly what the engine used. ' +
    'Every scenario is compared against the baseline. Dispatch policies are named rules evaluated ' +
    'interval by interval with no view of the future — Tonans does not optimise and never claims an ' +
    'optimal result.' }));

  const controls = el('div', { class: 'row', style: { marginTop: '10px' } });
  const max = state.session.entitlements.max_scenarios_per_project;
  const add = el('button', { type: 'button', text: '+ Add scenario' });
  add.disabled = max !== null && project.scenarios.length >= max;
  add.addEventListener('click', () => {
    const id = `scenario-${project.scenarios.length + 1}`;
    const template = structuredClone(project.scenarios.find((s) => s.battery) || project.scenarios[0]);
    template.id = id;
    template.name = `Scenario ${project.scenarios.length + 1}`;
    if (!template.battery) {
      template.battery = {
        nameplate_energy_kwh: 200, max_charge_kw: 100, max_discharge_kw: 100,
        soc_min_pct: 10, soc_max_pct: 100, reserve_soc_pct: 25,
        charge_efficiency_pct: 95, discharge_efficiency_pct: 95, self_discharge_pct_per_month: 1.5,
      };
      template.inverter.battery_power_kw = 100;
      template.initial_soc_pct = 50;
      template.economics.capex_usd = 110000;
    }
    project.scenarios.push(template);
    renderScenarios();
  });
  controls.appendChild(add);
  if (max !== null && project.scenarios.length >= max) {
    controls.appendChild(el('span', { class: 'tiny muted', text:
      `The ${state.session.tier} tier runs ${max} scenarios per project.` }));
  }
  const save = el('button', { class: 'primary', type: 'button', text: 'Save project' });
  save.addEventListener('click', () => guard(saveProject));
  controls.appendChild(save);
  head.appendChild(controls);
  root.appendChild(head);

  const baselineSelect = select('baseline-pick',
    project.scenarios.map((s) => [s.id, s.name]), project.baseline_scenario_id);
  baselineSelect.addEventListener('change', () => { project.baseline_scenario_id = baselineSelect.value; });
  root.appendChild(el('div', { class: 'card' }, [
    el('h3', { text: 'Baseline' }),
    el('p', { class: 'muted', text: 'Every saving, delta, and NPV in this project is measured against this scenario.' }),
    baselineSelect,
  ]));

  project.scenarios.forEach((scenario, index) => root.appendChild(scenarioEditor(scenario, index)));
}

function scenarioEditor(scenario, index) {
  const card = el('div', { class: 'card' });
  const isBaseline = scenario.id === state.project.baseline_scenario_id;

  const header = el('div', { class: 'row spread' });
  header.appendChild(el('h3', { text: scenario.name + (isBaseline ? '  (baseline)' : '') }));
  if (state.project.scenarios.length > 1) {
    const remove = el('button', { class: 'danger', type: 'button', text: 'Remove' });
    remove.addEventListener('click', () => {
      state.project.scenarios.splice(index, 1);
      if (isBaseline) state.project.baseline_scenario_id = state.project.scenarios[0].id;
      renderScenarios();
    });
    header.appendChild(remove);
  }
  card.appendChild(header);

  const bind = (obj, key) => (event) => {
    const value = event.target.type === 'number' ? Number(event.target.value)
                : event.target.type === 'checkbox' ? event.target.checked
                : event.target.value;
    obj[key] = value;
  };
  const num = (obj, key, step = 'any') => {
    const input = el('input', { type: 'number', step, value: String(obj[key] ?? 0) });
    input.addEventListener('input', bind(obj, key));
    return input;
  };
  const text = (obj, key) => {
    const input = el('input', { type: 'text', value: String(obj[key] ?? '') });
    input.addEventListener('input', bind(obj, key));
    return input;
  };
  const check = (obj, key, label) => {
    const input = el('input', { type: 'checkbox' });
    input.checked = Boolean(obj[key]);
    input.addEventListener('change', bind(obj, key));
    return el('label', { class: 'field-row' }, [input, el('span', { text: label })]);
  };

  const identity = el('fieldset', {}, [
    el('legend', { text: 'Identity' }),
    el('div', { class: 'grid' }, [
      field('Name', text(scenario, 'name')),
      field('Id (stable, lowercase)', text(scenario, 'id')),
    ]),
  ]);
  card.appendChild(identity);

  // -- Battery ------------------------------------------------------------
  const batteryBox = el('fieldset', {}, el('legend', { text: 'Battery' }));
  const toggle = el('input', { type: 'checkbox' });
  toggle.checked = Boolean(scenario.battery);
  toggle.addEventListener('change', () => {
    if (toggle.checked) {
      scenario.battery = {
        nameplate_energy_kwh: 200, max_charge_kw: 100, max_discharge_kw: 100,
        soc_min_pct: 10, soc_max_pct: 100, reserve_soc_pct: 25,
        charge_efficiency_pct: 95, discharge_efficiency_pct: 95, self_discharge_pct_per_month: 1.5,
      };
      scenario.inverter.battery_power_kw = 100;
      scenario.initial_soc_pct = 50;
    } else {
      scenario.battery = null;
      scenario.inverter.battery_power_kw = 0;
      scenario.initial_soc_pct = 0;
    }
    renderScenarios();
  });
  batteryBox.appendChild(el('label', { class: 'field-row', style: { marginBottom: '8px' } },
    [toggle, el('span', { text: 'This scenario has a battery' })]));

  if (scenario.battery) {
    const b = scenario.battery;
    batteryBox.appendChild(el('div', { class: 'grid' }, [
      field('Nameplate energy (kWh)', num(b, 'nameplate_energy_kwh')),
      field('Max charge (kW, at the AC bus)', num(b, 'max_charge_kw')),
      field('Max discharge (kW, at the AC bus)', num(b, 'max_discharge_kw')),
      field('Inverter rating (kW)', num(scenario.inverter, 'battery_power_kw')),
      field('Minimum state of charge (%)', num(b, 'soc_min_pct')),
      field('Maximum state of charge (%)', num(b, 'soc_max_pct')),
      field('Outage reserve floor (%)', num(b, 'reserve_soc_pct')),
      field('Charge efficiency (%)', num(b, 'charge_efficiency_pct')),
      field('Discharge efficiency (%)', num(b, 'discharge_efficiency_pct')),
      field('Self-discharge (% per month)', num(b, 'self_discharge_pct_per_month')),
      field('Initial state of charge (%)', num(scenario, 'initial_soc_pct')),
      field('Initial capacity (% of nameplate)', num(scenario, 'initial_capacity_pct')),
    ]));
    batteryBox.appendChild(el('p', { class: 'tiny muted', text:
      `Round trip: ${(b.charge_efficiency_pct * b.discharge_efficiency_pct / 100).toFixed(1)} %. ` +
      'All conversion loss lives in these two efficiencies; the inverter contributes a power ceiling ' +
      'and no loss of its own, so the two are never double-counted.' }));
  }
  card.appendChild(batteryBox);

  // -- Dispatch -----------------------------------------------------------
  const dispatchBox = el('fieldset', {}, el('legend', { text: 'Dispatch policy' }));
  const policy = select(`policy-${index}`, [
    ['self_consumption', 'Self-consumption'],
    ['peak_limit', 'Peak limit'],
    ['reserve_first', 'Reserve first'],
    ['manual_schedule', 'Manual schedule'],
  ], scenario.dispatch.policy);
  policy.addEventListener('change', () => {
    const chosen = policy.value;
    if (chosen === 'self_consumption') scenario.dispatch = { policy: chosen };
    else if (chosen === 'peak_limit') {
      scenario.dispatch = { policy: chosen, import_limit_kw: 90, allow_grid_charge: false,
                            grid_charge_target_soc_pct: scenario.battery?.soc_max_pct ?? 100 };
    } else if (chosen === 'reserve_first') {
      scenario.dispatch = { policy: chosen,
                            reserve_target_soc_pct: scenario.battery?.reserve_soc_pct ?? 25,
                            allow_grid_charge: false };
    } else scenario.dispatch = { policy: chosen, entries: [] };
    renderScenarios();
  });
  dispatchBox.appendChild(policy);

  const d = scenario.dispatch;
  const explain = {
    self_consumption: 'Stores surplus generation and discharges to cover net load. Never charges from ' +
      'the grid; never exports from the battery.',
    peak_limit: 'Discharges only as much as it takes to hold grid import at or below the ceiling, and ' +
      'charges from surplus generation. It does not otherwise self-consume — holding the peak down is ' +
      'the whole job.',
    reserve_first: 'Below the target the battery only ever charges: refilling the outage reserve outranks ' +
      'every saving. At or above it, it behaves as self-consumption and will not fall back below the target ' +
      'while the grid is up.',
    manual_schedule: 'Follows your setpoints exactly, and is idle outside them. Setpoints are clipped to ' +
      'what the hardware can do, and any clipping is reported as a warning — the schedule you wrote is not ' +
      'necessarily the schedule that ran.',
  }[d.policy];
  dispatchBox.appendChild(el('p', { class: 'tiny muted', style: { marginTop: '6px' }, text: explain }));

  if (d.policy === 'peak_limit') {
    dispatchBox.appendChild(el('div', { class: 'grid' }, [
      field('Import ceiling (kW)', num(d, 'import_limit_kw')),
      field('Grid charge target (% state of charge)', num(d, 'grid_charge_target_soc_pct')),
      el('div', { class: 'field' }, check(d, 'allow_grid_charge', 'May charge from the grid')),
    ]));
  } else if (d.policy === 'reserve_first') {
    dispatchBox.appendChild(el('div', { class: 'grid' }, [
      field('Reserve target (% state of charge)', num(d, 'reserve_target_soc_pct')),
      el('div', { class: 'field' }, check(d, 'allow_grid_charge', 'May charge from the grid')),
    ]));
  } else if (d.policy === 'manual_schedule') {
    dispatchBox.appendChild(scheduleEditor(d, index));
  }
  card.appendChild(dispatchBox);

  // -- Tariff -------------------------------------------------------------
  const t = scenario.tariff;
  const tariffBox = el('fieldset', {}, el('legend', { text: 'Tariff' }));
  const importSource = select(`imp-src-${index}`, [
    ['series', 'Per-interval price from the imported data'],
    ['flat', 'One flat price'],
  ], t.import.source);
  importSource.addEventListener('change', () => {
    t.import = importSource.value === 'flat' ? { source: 'flat', price_per_kwh_usd: 0.15 } : { source: 'series' };
    renderScenarios();
  });
  const exportSource = select(`exp-src-${index}`, [
    ['series', 'Per-interval credit from the imported data'],
    ['flat', 'One flat credit'],
  ], t.export.source);
  exportSource.addEventListener('change', () => {
    t.export = exportSource.value === 'flat' ? { source: 'flat', price_per_kwh_usd: 0.05 } : { source: 'series' };
    renderScenarios();
  });

  const tariffFields = [
    field('Import price source', importSource),
    t.import.source === 'flat' ? field('Import price (USD/kWh)', num(t.import, 'price_per_kwh_usd')) : null,
    field('Export credit source', exportSource),
    t.export.source === 'flat' ? field('Export credit (USD/kWh)', num(t.export, 'price_per_kwh_usd')) : null,
    field('Import price multiplier (x)', num(t, 'import_price_multiplier')),
    field('Export credit multiplier (x)', num(t, 'export_price_multiplier')),
    field('Fixed charge (USD per month)', num(t, 'fixed_charge_usd_per_month')),
    field('Site export limit (kW, blank = unlimited)', exportLimit(scenario)),
  ].filter(Boolean);
  tariffBox.appendChild(el('div', { class: 'grid' }, tariffFields));

  const demand = t.demand_prices_per_kw_month_usd || {};
  const periods = state.project.dataset?.demand_periods?.labels ?? ['all'];
  tariffBox.appendChild(el('h4', { text: 'Demand charge (USD per kW of monthly peak)' }));
  tariffBox.appendChild(el('div', { class: 'grid' }, periods.map((period) => {
    const input = el('input', { type: 'number', step: 'any', value: String(demand[period] ?? 0) });
    input.addEventListener('input', (e) => {
      t.demand_prices_per_kw_month_usd = t.demand_prices_per_kw_month_usd || {};
      t.demand_prices_per_kw_month_usd[period] = Number(e.target.value);
    });
    return field(`${period} (USD/kW-month)`, input);
  })));
  card.appendChild(tariffBox);

  // -- Economics ----------------------------------------------------------
  const k = scenario.economics;
  const economicsBox = el('fieldset', {}, [
    el('legend', { text: 'Economics' }),
    el('div', { class: 'grid' }, [
      field('Installed cost (USD)', num(k, 'capex_usd')),
      field('Operating cost (USD per year)', num(k, 'annual_opex_usd')),
      field('Horizon (years)', num(k, 'horizon_years', '1')),
      field('Discount rate (% per year)', num(k, 'discount_rate_pct_per_year')),
      field('Energy escalation (% per year)', num(k, 'energy_price_escalation_pct_per_year')),
    ]),
  ]);
  card.appendChild(economicsBox);

  // -- Degradation --------------------------------------------------------
  const g = scenario.degradation;
  const degradationBox = el('fieldset', {}, [
    el('legend', { text: 'Degradation — a transparent approximation, not a lifetime model' }),
    el('div', { class: 'grid' }, [
      field('Calendar fade (% per year)', num(g, 'calendar_fade_pct_per_year')),
      field('Cycle fade (% per equivalent full cycle)', num(g, 'cycle_fade_pct_per_cycle')),
      field('Throughput cost (USD per kWh discharged)', num(g, 'throughput_cost_per_kwh_throughput_usd')),
    ]),
    el('p', { class: 'tiny muted', text:
      'A linear fade in two terms. It knows nothing about depth of discharge, C-rate, or thermal ' +
      'history. Where a vendor gives a warranted capacity curve, use that instead.' }),
  ]);
  card.appendChild(degradationBox);

  card.appendChild(outageEditor(scenario, index));
  return card;
}

function exportLimit(scenario) {
  const input = el('input', {
    type: 'number', step: 'any',
    value: scenario.inverter.site_export_limit_kw === null || scenario.inverter.site_export_limit_kw === undefined
      ? '' : String(scenario.inverter.site_export_limit_kw),
  });
  input.addEventListener('input', (event) => {
    const raw = event.target.value.trim();
    scenario.inverter.site_export_limit_kw = raw === '' ? null : Number(raw);
  });
  return input;
}

function scheduleEditor(dispatch, index) {
  const box = el('div', {});
  dispatch.entries = dispatch.entries || [];
  const table = el('table');
  table.appendChild(el('thead', {}, el('tr', {}, [
    el('th', { text: 'From (UTC)' }), el('th', { text: 'To (UTC)' }),
    el('th', { text: 'Battery power (kW, + discharges)' }), el('th', {}),
  ])));
  const body = el('tbody');
  dispatch.entries.forEach((entry, i) => {
    const from = el('input', { type: 'datetime-local', value: (entry.start_utc || '').slice(0, 16) });
    from.addEventListener('input', (e) => { entry.start_utc = new Date(`${e.target.value}Z`).toISOString(); });
    const to = el('input', { type: 'datetime-local', value: (entry.end_utc || '').slice(0, 16) });
    to.addEventListener('input', (e) => { entry.end_utc = new Date(`${e.target.value}Z`).toISOString(); });
    const kw = el('input', { type: 'number', step: 'any', value: String(entry.battery_power_kw ?? 0) });
    kw.addEventListener('input', (e) => { entry.battery_power_kw = Number(e.target.value); });
    const drop = el('button', { class: 'danger', type: 'button', text: 'Remove' });
    drop.addEventListener('click', () => { dispatch.entries.splice(i, 1); renderScenarios(); });
    body.appendChild(el('tr', {}, [el('td', {}, from), el('td', {}, to), el('td', {}, kw), el('td', {}, drop)]));
  });
  table.appendChild(body);
  box.appendChild(el('div', { class: 'scroll' }, table));

  const add = el('button', { type: 'button', text: '+ Add setpoint window' });
  add.addEventListener('click', () => {
    const start = state.project.dataset?.start_utc || new Date().toISOString();
    dispatch.entries.push({ start_utc: start, end_utc: start, battery_power_kw: 0 });
    renderScenarios();
  });
  box.appendChild(el('div', { class: 'row', style: { marginTop: '8px' } }, add));
  box.appendChild(el('p', { class: 'tiny muted', text:
    'Windows must start and end exactly on an interval boundary inside the imported data. Tonans will ' +
    'not round a window to fit — that would be a silent repair.' }));
  return box;
}

function outageEditor(scenario, index) {
  const box = el('fieldset', {}, el('legend', { text: 'Outages to replay' }));
  scenario.outage.windows = scenario.outage.windows || [];

  const table = el('table');
  table.appendChild(el('thead', {}, el('tr', {}, [
    el('th', { text: 'From (UTC)' }), el('th', { text: 'To (UTC)' }), el('th', { text: 'Label' }), el('th', {}),
  ])));
  const body = el('tbody');
  scenario.outage.windows.forEach((window, i) => {
    const from = el('input', { type: 'datetime-local', value: (window.start_utc || '').slice(0, 16) });
    from.addEventListener('input', (e) => { window.start_utc = new Date(`${e.target.value}Z`).toISOString(); });
    const to = el('input', { type: 'datetime-local', value: (window.end_utc || '').slice(0, 16) });
    to.addEventListener('input', (e) => { window.end_utc = new Date(`${e.target.value}Z`).toISOString(); });
    const label = el('input', { type: 'text', value: window.label || '' });
    label.addEventListener('input', (e) => { window.label = e.target.value; });
    const drop = el('button', { class: 'danger', type: 'button', text: 'Remove' });
    drop.addEventListener('click', () => { scenario.outage.windows.splice(i, 1); renderScenarios(); });
    body.appendChild(el('tr', {}, [el('td', {}, from), el('td', {}, to), el('td', {}, label), el('td', {}, drop)]));
  });
  table.appendChild(body);
  box.appendChild(el('div', { class: 'scroll' }, table));

  const add = el('button', { type: 'button', text: '+ Add outage window' });
  add.addEventListener('click', () => {
    const start = state.project.dataset?.start_utc || new Date().toISOString();
    scenario.outage.windows.push({ start_utc: start, end_utc: start, label: '' });
    renderScenarios();
  });
  box.appendChild(el('div', { class: 'row', style: { marginTop: '8px' } }, add));
  box.appendChild(el('p', { class: 'tiny muted', text:
    'During an outage Tonans assumes non-critical load is shed by a transfer switch, and lets the ' +
    'battery discharge below its reserve floor down to its hard minimum — the reserve exists for exactly ' +
    'this moment.' }));
  return box;
}

// ---------------------------------------------------------------------------
// 3 — Results
// ---------------------------------------------------------------------------

function renderResults() {
  const root = document.getElementById('view-results');
  clear(root);

  if (!state.project.dataset) {
    root.appendChild(el('div', { class: 'banner', text: 'Import interval data first.' }));
    return;
  }

  const bar = el('div', { class: 'card' });
  bar.appendChild(el('h3', { text: 'Run' }));
  const go = el('button', { class: 'primary', type: 'button', text: 'Run all scenarios' });
  go.addEventListener('click', async () => {
    go.disabled = true;
    go.textContent = 'Running…';
    const result = await guard(() => api(`/api/projects/${encodeURIComponent(state.project.name)}/run`, { method: 'POST' }));
    go.disabled = false;
    go.textContent = 'Run all scenarios';
    if (!result) return;
    state.run = result.run;
    state.timelines = Object.fromEntries(result.timelines.map((t) => [t.scenario_id, t]));
    renderResults();
  });
  bar.appendChild(el('div', { class: 'row' }, [
    go,
    el('span', { class: 'tiny muted', text:
      'Every scenario is simulated over every interval. Identical inputs always produce identical ' +
      'results — the model input hash below proves it.' }),
  ]));
  root.appendChild(bar);

  if (!state.run) return;
  const run = state.run;

  root.appendChild(el('div', { class: 'card' }, [
    el('h3', { text: 'Provenance' }),
    kv([
      ['Model input hash', run.model_input_hash],
      ['Project hash', run.project_hash],
      ['Generated', fmt.when(run.generated_utc)],
      ['Model version', run.model_version],
    ]),
  ]));

  const warnings = allWarnings(run);
  if (warnings.length) {
    root.appendChild(el('div', { class: 'card' }, [
      el('h3', { text: `Warning ledger (${warnings.length})` }),
      el('p', { class: 'muted', text: 'Most serious first. Nothing is filtered out for being inconvenient.' }),
      ledger(warnings),
    ]));
  }

  root.appendChild(comparisonCard(run));
  for (const scenario of run.scenarios) root.appendChild(scenarioResult(run, scenario));
}

function allWarnings(run) {
  const seen = new Set();
  const out = [];
  const rank = { critical: 0, caution: 1, info: 2 };
  for (const w of [...(run.warnings || []), ...run.scenarios.flatMap((s) => s.warnings || [])]) {
    if (seen.has(w.code)) continue;
    seen.add(w.code);
    out.push(w);
  }
  out.sort((a, b) => rank[a.severity] - rank[b.severity] || a.code.localeCompare(b.code));
  return out;
}

function ledger(warnings) {
  const list = el('ul', { class: 'ledger' });
  const icon = { info: 'i', caution: '!', critical: 'x' };
  for (const w of warnings) {
    list.appendChild(el('li', { class: w.severity }, [
      el('span', { class: 'sev', text: `${icon[w.severity]} ${w.severity}` }),
      el('span', { class: 'code', text: w.code }),
      document.createTextNode(w.message),
    ]));
  }
  return list;
}

function comparisonCard(run) {
  const card = el('div', { class: 'card' });
  card.appendChild(el('h3', { text: 'Scenario comparison' }));
  card.appendChild(el('p', { class: 'muted', text:
    'A negative cost change is a saving. Everything is measured against the baseline.' }));

  const table = el('table');
  table.appendChild(el('thead', {}, el('tr', {}, [
    'Scenario', 'Policy', 'Grid cost (annual)', 'Change', 'Peak import', 'Self-sufficiency',
    'Unmet critical', 'NPV', 'Payback',
  ].map((h) => el('th', { class: h === 'Scenario' || h === 'Policy' ? '' : 'num', text: h })))));

  const body = el('tbody');
  for (const s of run.scenarios) {
    const c = s.comparison;
    const k = s.economics;
    const delta = c.is_baseline ? null : c.grid_cost_delta_usd * k.annualization_multiplier;
    body.appendChild(el('tr', { class: c.is_baseline ? 'is-baseline' : '' }, [
      el('td', {}, [
        el('span', { class: 'swatch', style: { background: paint('load') } }),
        document.createTextNode(s.scenario_name),
      ]),
      el('td', { text: s.dispatch_policy }),
      el('td', { class: 'num', text: fmt.usd(k.scenario_annual_grid_cost_usd) }),
      el('td', { class: `num ${delta === null ? '' : delta < 0 ? 'good' : 'bad'}`,
                 text: delta === null ? '—' : fmt.usd(delta) }),
      el('td', { class: 'num', text: fmt.kw(s.energy.peak_import_kw) }),
      el('td', { class: 'num', text: fmt.pct(s.energy.self_sufficiency_pct) }),
      el('td', { class: `num ${s.energy.unmet_critical_kwh > 1e-9 ? 'bad' : ''}`,
                 text: fmt.kwh(s.energy.unmet_critical_kwh) }),
      el('td', { class: `num ${k.npv_usd >= 0 ? 'good' : 'bad'}`,
                 text: s.scenario.battery || k.capex_usd > 0 ? fmt.usd(k.npv_usd) : '—' }),
      el('td', { class: 'num', text: fmt.years(k.simple_payback_years) }),
    ]));
  }
  table.appendChild(body);
  card.appendChild(el('div', { class: 'scroll' }, table));
  return card;
}

function scenarioResult(run, s) {
  const card = el('div', { class: 'card' });
  card.appendChild(el('h3', { text: s.scenario_name }));
  card.appendChild(el('p', { class: 'muted', text: `Dispatch: ${s.dispatch_policy}` }));

  const e = s.energy;
  const k = s.economics;
  card.appendChild(el('div', { class: 'tiles' }, [
    tile('Grid cost (annual)', fmt.usd(k.scenario_annual_grid_cost_usd), 'over the simulated year'),
    tile('Peak grid import', fmt.kw(e.peak_import_kw), 'sets the demand charge'),
    tile('Self-sufficiency', fmt.pct(e.self_sufficiency_pct),
      'of load met by onsite generation, directly or after storage'),
    tile('Net present value', fmt.usd(k.npv_usd), `${k.horizon_years} years at ${k.discount_rate_pct_per_year}%`),
  ]));

  card.appendChild(el('h4', { text: 'Energy flow' }));
  card.appendChild(energyFlow(e));

  const timeline = state.timelines[s.scenario_id];
  if (timeline) {
    card.appendChild(el('h4', { text: 'Timelines' }));
    card.appendChild(timelineCharts(timeline, Boolean(s.scenario.battery)));
  }

  if (s.outages?.length) {
    card.appendChild(el('h4', { text: 'Outages' }));
    card.appendChild(outageTable(s.outages));
  }

  card.appendChild(el('h4', { text: 'Energy' }));
  card.appendChild(kv([
    ['Site load', fmt.kwh(e.load_kwh)],
    ['Load served', fmt.kwh(e.served_load_kwh)],
    ['Generation available', fmt.kwh(e.generation_available_kwh)],
    ['Generation curtailed', fmt.kwh(e.curtailed_kwh)],
    ['Grid import', fmt.kwh(e.grid_import_kwh)],
    ['Grid export', fmt.kwh(e.grid_export_kwh)],
    ['Battery charged', fmt.kwh(e.battery_charge_kwh)],
    ['Battery discharged', fmt.kwh(e.battery_discharge_kwh)],
    ['Conversion loss', fmt.kwh(e.conversion_loss_kwh)],
    ['Standing loss', fmt.kwh(e.self_discharge_loss_kwh)],
    ['Equivalent full cycles', fmt.n(e.equivalent_full_cycles, 1)],
    ['Energy balance residual', `${e.energy_balance_residual_kwh.toExponential(2)} kWh`],
  ]));
  card.appendChild(el('p', { class: 'tiny muted', text:
    'The residual is the model checking itself: generation used, plus import, plus discharge, minus load ' +
    'served, export, and charge, summed over every interval. It should be zero to within floating-point ' +
    'rounding. If it is not, nothing above is trustworthy.' }));

  if (s.scenario.battery || k.capex_usd > 0) {
    card.appendChild(el('h4', { text: 'Economics' }));
    card.appendChild(cashFlowTable(k));
    card.appendChild(el('p', { class: 'tiny muted', text: k.degradation_method }));
  }
  return card;
}

function tile(label, value, note) {
  return el('div', { class: 'tile' }, [
    el('div', { class: 'k', text: label }),
    el('div', { class: 'v', text: value }),
    el('div', { class: 'n', text: note }),
  ]);
}

function kv(rows) {
  const table = el('table');
  const body = el('tbody');
  for (const [key, value] of rows) {
    body.appendChild(el('tr', {}, [
      el('th', { scope: 'row', text: key }),
      el('td', { class: 'num mono', text: String(value) }),
    ]));
  }
  table.appendChild(body);
  return el('div', { class: 'scroll' }, table);
}

/**
 * The energy-flow diagram: two stacked bars, drawn to the same scale.
 *
 * Because the model conserves energy, they come out the same length — so a reader
 * can *see* the balance close. This is an energy balance. It is not an AC
 * power-flow study: there is no voltage, no phase angle, and no impedance anywhere
 * in Tonans.
 */
function energyFlow(e) {
  const box = el('div', { class: 'chart-wrap flow' });
  const losses = e.conversion_loss_kwh + e.self_discharge_loss_kwh;
  const stored = e.final_stored_energy_kwh - e.initial_stored_energy_kwh;

  const inputs = [
    ['generation', 'Generation', e.generation_used_kwh],
    ['import', 'Grid import', e.grid_import_kwh],
    ['discharge', 'Battery out', e.battery_discharge_kwh],
  ];
  const outputs = [
    ['load', 'Load served', e.served_load_kwh],
    ['export', 'Grid export', e.grid_export_kwh],
    ['charge', 'Battery in', e.battery_charge_kwh],
    ['curtailed', 'Curtailed', e.curtailed_kwh],
  ];
  const totalIn = inputs.reduce((sum, [, , v]) => sum + Math.max(v, 0), 0);
  const totalOut = outputs.reduce((sum, [, , v]) => sum + Math.max(v, 0), 0);
  const scale = Math.max(totalIn, totalOut, 1);

  for (const [title, segments, total] of [['Energy in', inputs, totalIn], ['Energy out', outputs, totalOut]]) {
    box.appendChild(el('div', { class: 'flow-label' }, [
      el('span', { text: title }),
      el('span', { text: fmt.kwh(total) }),
    ]));
    const bar = el('div', { class: 'flow-bar' });
    for (const [role, label, value] of segments) {
      if (value <= 0) continue;
      const width = (value / scale) * 100;
      const seg = el('div', {
        class: 'flow-seg',
        style: { flex: `0 0 ${width}%`, background: paint(role) },
        title: `${label}: ${fmt.kwh(value)} (${((value / total) * 100).toFixed(1)}% of ${title.toLowerCase()})`,
      });
      if (width > 9) seg.textContent = label;
      bar.appendChild(seg);
    }
    box.appendChild(bar);
  }

  box.appendChild(el('p', { class: 'chart-note', text:
    `Both bars are drawn to the same scale. Stored energy changed by ${fmt.kwh(stored)} and ` +
    `${fmt.kwh(losses)} was lost in conversion and standing loss, which is why they are not identical. ` +
    'This is an energy balance, not an AC power-flow study.' }));
  return box;
}

function outageTable(outages) {
  const table = el('table');
  table.appendChild(el('thead', {}, el('tr', {}, [
    'Window', 'Duration', 'Critical load', 'Served', 'Unmet', 'SOC at start', 'Lowest SOC', 'Outcome',
  ].map((h) => el('th', { text: h })))));
  const body = el('tbody');
  for (const o of outages) {
    body.appendChild(el('tr', {}, [
      el('td', {}, [
        o.label ? el('strong', { text: o.label }) : null,
        el('div', { class: 'tiny muted', text: `${fmt.when(o.start_utc)} → ${fmt.when(o.end_utc)}` }),
      ].filter(Boolean)),
      el('td', { class: 'num', text: `${o.duration_hours.toFixed(1)} h` }),
      el('td', { class: 'num', text: fmt.kwh(o.critical_load_kwh) }),
      el('td', { class: 'num', text: fmt.kwh(o.critical_served_kwh) }),
      el('td', { class: `num ${o.survived ? '' : 'bad'}`, text: fmt.kwh(o.unmet_critical_kwh) }),
      el('td', { class: 'num', text: fmt.pct(o.soc_at_start_pct) }),
      el('td', { class: 'num', text: fmt.pct(o.min_soc_pct) }),
      el('td', { class: o.survived ? 'good' : 'bad', text: o.survived
        ? '+ survived the full window'
        : `x critical load lost after ${o.survival_hours.toFixed(1)} h` }),
    ]));
  }
  table.appendChild(body);
  return el('div', { class: 'scroll' }, table);
}

function cashFlowTable(k) {
  const table = el('table');
  table.appendChild(el('thead', {}, el('tr', {}, [
    'Year', 'Usable capacity', 'Gross saving', 'Operating', 'Throughput', 'Replacement',
    'Net cash flow', 'Discounted', 'Cumulative',
  ].map((h) => el('th', { class: 'num', text: h })))));
  const body = el('tbody');
  body.appendChild(el('tr', {}, [
    el('td', { class: 'num', text: '0' }), el('td', { class: 'num', text: '—' }),
    el('td', { class: 'num', text: '—' }), el('td', { class: 'num', text: '—' }),
    el('td', { class: 'num', text: '—' }), el('td', { class: 'num', text: fmt.usd(k.capex_usd) }),
    el('td', { class: 'num bad', text: fmt.usd(-k.capex_usd) }),
    el('td', { class: 'num bad', text: fmt.usd(-k.capex_usd) }),
    el('td', { class: 'num bad', text: fmt.usd(-k.capex_usd) }),
  ]));
  for (const y of k.cash_flow) {
    body.appendChild(el('tr', {}, [
      el('td', { class: 'num', text: String(y.year) }),
      el('td', { class: 'num', text: fmt.pct(y.capacity_retention_pct) }),
      el('td', { class: 'num', text: fmt.usd(y.gross_savings_usd) }),
      el('td', { class: 'num', text: fmt.usd(-y.opex_usd) }),
      el('td', { class: 'num', text: fmt.usd(-y.throughput_cost_usd) }),
      el('td', { class: 'num', text: y.replacement_usd > 0 ? fmt.usd(-y.replacement_usd) : '—' }),
      el('td', { class: `num ${y.net_cash_flow_usd >= 0 ? 'good' : 'bad'}`, text: fmt.usd(y.net_cash_flow_usd) }),
      el('td', { class: 'num', text: fmt.usd(y.discounted_cash_flow_usd) }),
      el('td', { class: 'num', text: fmt.usd(y.cumulative_discounted_usd - k.capex_usd) }),
    ]));
  }
  table.appendChild(body);
  return el('div', { class: 'scroll' }, table);
}

// ---------------------------------------------------------------------------
// Canvas timelines
// ---------------------------------------------------------------------------

/**
 * Four stacked panels sharing one time axis.
 *
 * State of charge gets its own panel rather than a second y-axis on the power
 * chart. A chart with two y-scales invites a reader to compare two quantities whose
 * relative height is an artefact of the scaling the author chose, and that is not a
 * comparison Tonans is willing to imply.
 */
function timelineCharts(timeline, hasBattery) {
  const panels = [
    { title: 'Load and on-site generation', unit: 'kW', signed: false, series: [
      { key: 'load_kw', role: 'load', label: 'Load' },
      { key: 'generation_kw', role: 'generation', label: 'Generation' },
    ] },
    { title: 'Grid exchange — import above zero, export below', unit: 'kW', signed: true, series: [
      { key: 'grid_import_kw', role: 'import', label: 'Import', sign: 1 },
      { key: 'grid_export_kw', role: 'export', label: 'Export', sign: -1 },
    ] },
  ];
  if (hasBattery) {
    panels.push({ title: 'Battery power — discharge above zero, charge below', unit: 'kW', signed: true, series: [
      { key: 'battery_discharge_kw', role: 'discharge', label: 'Discharge', sign: 1 },
      { key: 'battery_charge_kw', role: 'charge', label: 'Charge', sign: -1 },
    ] });
    panels.push({ title: 'State of charge', unit: '%', signed: false, series: [
      { key: 'soc_pct', role: 'soc', label: 'State of charge' },
    ] });
  }

  const wrap = el('div', { class: 'chart-wrap' });

  const key = el('div', { class: 'legend' });
  for (const panel of panels) {
    for (const s of panel.series) {
      key.appendChild(el('span', {}, [
        el('span', { class: 'swatch', style: { background: paint(s.role) } }),
        document.createTextNode(s.label),
      ]));
    }
  }
  wrap.appendChild(key);

  const canvas = el('canvas');
  wrap.appendChild(canvas);
  const tip = el('div', { class: 'tooltip hidden' });
  wrap.appendChild(tip);

  wrap.appendChild(el('p', { class: 'chart-note', text: timeline.decimated
    ? `${timeline.source_interval_count.toLocaleString()} intervals shown as ` +
      `${timeline.bucket_start_utc.length.toLocaleString()} buckets. The shaded band is each bucket's ` +
      'full range, so no peak is hidden. The exported timeseries CSV holds every interval.'
    : `${timeline.source_interval_count.toLocaleString()} intervals, at full resolution.` }));

  const draw = () => drawTimeline(canvas, tip, timeline, panels);
  requestAnimationFrame(draw);
  window.addEventListener('resize', draw);
  // Dark mode changes every colour; the canvas has to be repainted, since unlike
  // SVG it cannot reference a CSS custom property.
  window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', draw);
  return wrap;
}

function drawTimeline(canvas, tip, timeline, panels) {
  const dpr = window.devicePixelRatio || 1;
  const width = canvas.clientWidth || 800;
  const panelHeight = 110;
  const gap = 30;
  const marginLeft = 54;
  const marginRight = 14;
  const marginTop = 18;
  const axisHeight = 26;
  const height = marginTop + panels.length * (panelHeight + gap) + axisHeight;

  canvas.width = width * dpr;
  canvas.height = height * dpr;
  canvas.style.height = `${height}px`;
  const ctx = canvas.getContext('2d');
  ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  ctx.clearRect(0, 0, width, height);

  const plotWidth = width - marginLeft - marginRight;
  const n = timeline.bucket_start_utc.length;
  if (!n) return;

  const ink = css('--ink');
  const muted = css('--ink-muted');
  const gridline = css('--gridline');
  const axis = css('--axis');
  const font = '11px system-ui, -apple-system, "Segoe UI", sans-serif';

  const geometry = [];

  panels.forEach((panel, index) => {
    const top = marginTop + index * (panelHeight + gap);
    const bottom = top + panelHeight;

    let low = Infinity;
    let high = -Infinity;
    for (const s of panel.series) {
      const data = timeline.series[s.key];
      if (!data) continue;
      const sign = s.sign ?? 1;
      for (let i = 0; i < n; i += 1) {
        low = Math.min(low, data.min[i] * sign, data.max[i] * sign);
        high = Math.max(high, data.min[i] * sign, data.max[i] * sign);
      }
    }
    if (!Number.isFinite(low) || !Number.isFinite(high)) { low = 0; high = 1; }
    if (panel.signed) {
      const extent = Math.max(Math.abs(low), Math.abs(high), 1e-6);
      low = -extent; high = extent;
    } else {
      low = Math.min(low, 0);
      if (high - low < 1e-9) high = low + 1;
    }
    const y = (value) => bottom - ((value - low) / (high - low)) * panelHeight;
    geometry.push({ panel, top, bottom, y, low, high });

    ctx.font = font;
    ctx.fillStyle = ink;
    ctx.textAlign = 'left';
    ctx.fillText(panel.title, marginLeft, top - 6);

    for (const tick of niceTicks(low, high, 3)) {
      const ty = y(tick);
      if (ty < top - 1 || ty > bottom + 1) continue;
      ctx.strokeStyle = Math.abs(tick) < 1e-9 ? axis : gridline;
      ctx.lineWidth = 1;
      ctx.beginPath();
      ctx.moveTo(marginLeft, Math.round(ty) + 0.5);
      ctx.lineTo(marginLeft + plotWidth, Math.round(ty) + 0.5);
      ctx.stroke();
      ctx.fillStyle = muted;
      ctx.textAlign = 'right';
      ctx.fillText(fmt.n(tick, Math.abs(tick) < 10 ? 1 : 0), marginLeft - 6, ty + 3.5);
    }
    ctx.fillStyle = muted;
    ctx.textAlign = 'right';
    ctx.fillText(panel.unit, marginLeft - 6, top - 6);

    // Outage shading, so a reader can see which part of the year is islanded.
    ctx.fillStyle = css('--c-unmet');
    ctx.globalAlpha = 0.10;
    for (let i = 0; i < n; i += 1) {
      if (!timeline.outage[i]) continue;
      const x = marginLeft + (plotWidth * i) / Math.max(n - 1, 1);
      ctx.fillRect(x, top, Math.max(plotWidth / n, 1.5), panelHeight);
    }
    ctx.globalAlpha = 1;

    for (const s of panel.series) {
      const data = timeline.series[s.key];
      if (!data) continue;
      const sign = s.sign ?? 1;
      const colour = paint(s.role);
      const xs = (i) => marginLeft + (plotWidth * i) / Math.max(n - 1, 1);

      // The band: each bucket's full range. Collapses to a line when the data is not
      // decimated, which is exactly right.
      ctx.fillStyle = colour;
      ctx.globalAlpha = 0.26;
      ctx.beginPath();
      for (let i = 0; i < n; i += 1) ctx.lineTo(xs(i), y(Math.max(data.max[i] * sign, data.min[i] * sign)));
      for (let i = n - 1; i >= 0; i -= 1) ctx.lineTo(xs(i), y(Math.min(data.max[i] * sign, data.min[i] * sign)));
      ctx.closePath();
      ctx.fill();
      ctx.globalAlpha = 1;

      ctx.strokeStyle = colour;
      ctx.lineWidth = 1.6;
      ctx.lineJoin = 'round';
      ctx.beginPath();
      for (let i = 0; i < n; i += 1) {
        const py = y(data.mean[i] * sign);
        if (i === 0) ctx.moveTo(xs(i), py); else ctx.lineTo(xs(i), py);
      }
      ctx.stroke();
    }
  });

  // The shared time axis.
  ctx.font = font;
  ctx.fillStyle = muted;
  const ticks = 6;
  for (let t = 0; t < ticks; t += 1) {
    const i = Math.round((t * (n - 1)) / (ticks - 1));
    const x = marginLeft + (plotWidth * i) / Math.max(n - 1, 1);
    ctx.textAlign = t === 0 ? 'left' : t === ticks - 1 ? 'right' : 'center';
    const when = new Date(timeline.bucket_start_utc[i]);
    ctx.fillText(when.toLocaleDateString(undefined, { month: 'short', day: 'numeric' }),
      x, height - axisHeight + 14);
  }

  canvas.onmousemove = (event) => {
    const rect = canvas.getBoundingClientRect();
    const x = event.clientX - rect.left;
    if (x < marginLeft || x > marginLeft + plotWidth) { tip.classList.add('hidden'); return; }
    const i = Math.round(((x - marginLeft) / plotWidth) * (n - 1));

    clear(tip);
    tip.appendChild(el('div', { class: 't-time',
      text: new Date(timeline.bucket_start_utc[i]).toLocaleString(undefined,
        { dateStyle: 'medium', timeStyle: 'short' }) }));
    if (timeline.outage[i]) {
      tip.appendChild(el('div', { class: 't-row' }, [
        el('span', { style: { color: css('--c-unmet'), fontWeight: '700' }, text: 'GRID DOWN' }),
      ]));
    }
    for (const { panel } of geometry) {
      for (const s of panel.series) {
        const data = timeline.series[s.key];
        if (!data) continue;
        tip.appendChild(el('div', { class: 't-row' }, [
          el('span', {}, [
            el('span', { class: 'swatch', style: { background: paint(s.role) } }),
            document.createTextNode(s.label),
          ]),
          el('span', { text: `${fmt.n(data.mean[i], 1)} ${panel.unit}` }),
        ]));
      }
    }
    tip.classList.remove('hidden');
    const left = Math.min(x + 14, rect.width - 190);
    tip.style.left = `${Math.max(left, 6)}px`;
    tip.style.top = `${Math.min(event.clientY - rect.top + 12, rect.height - 140)}px`;

    // Redraw plus a crosshair, so the tooltip is anchored to something visible.
    drawCrosshair(canvas, ctx, x, marginTop, height - axisHeight, axis);
  };
  canvas.onmouseleave = () => {
    tip.classList.add('hidden');
    drawTimeline(canvas, tip, timeline, panels);
  };
}

let crosshairFrame = null;
function drawCrosshair(canvas, ctx, x, top, bottom, colour) {
  if (crosshairFrame) cancelAnimationFrame(crosshairFrame);
  crosshairFrame = requestAnimationFrame(() => {
    ctx.save();
    ctx.strokeStyle = colour;
    ctx.lineWidth = 1;
    ctx.setLineDash([3, 3]);
    ctx.beginPath();
    ctx.moveTo(Math.round(x) + 0.5, top);
    ctx.lineTo(Math.round(x) + 0.5, bottom);
    ctx.stroke();
    ctx.restore();
  });
}

function niceTicks(low, high, count) {
  if (!Number.isFinite(low) || !Number.isFinite(high) || high - low < 1e-12) return [low];
  const raw = (high - low) / count;
  const magnitude = 10 ** Math.floor(Math.log10(Math.abs(raw)));
  const normalized = raw / magnitude;
  const step = (normalized <= 1 ? 1 : normalized <= 2 ? 2 : normalized <= 5 ? 5 : 10) * magnitude;
  const out = [];
  for (let t = Math.floor(low / step) * step; t <= high + step * 0.5 && out.length < count + 3; t += step) {
    if (t >= low - step * 0.5) out.push(t);
  }
  return out.length ? out : [low];
}

// ---------------------------------------------------------------------------
// 4 — Sensitivity
// ---------------------------------------------------------------------------

const PARAMS = [
  ['battery_energy_kwh', 'Battery energy (kWh)'],
  ['battery_power_kw', 'Battery power (kW)'],
  ['reserve_soc_pct', 'Reserve floor (%)'],
  ['initial_capacity_pct', 'Initial capacity (%)'],
  ['import_price_multiplier', 'Import price (x)'],
  ['demand_price_per_kw_month_usd', 'Demand price (USD/kW-month)'],
  ['calendar_fade_pct_per_year', 'Calendar fade (%/year)'],
  ['capex_usd', 'CAPEX (USD)'],
  ['discount_rate_pct_per_year', 'Discount rate (%/year)'],
];

function renderSensitivity() {
  const root = document.getElementById('view-sensitivity');
  clear(root);

  const card = el('div', { class: 'card' });
  card.appendChild(el('h3', { text: 'Sensitivity' }));
  card.appendChild(el('p', { class: 'muted', text:
    `Every cell is a full simulation of the whole data set, not an interpolation — which is why the ` +
    `grid is bounded. Your tier runs up to ${state.session.entitlements.max_sensitivity_cells} cells.` }));

  const scenarioPick = select('sens-scenario',
    state.project.scenarios.map((s) => [s.id, s.name]),
    state.project.scenarios.find((s) => s.battery)?.id ?? state.project.scenarios[0].id);

  const axis1 = select('sens-p1', PARAMS, 'battery_energy_kwh');
  const values1 = el('input', { type: 'text', id: 'sens-v1', value: '200, 400, 600, 800',
    placeholder: 'comma-separated values' });
  const axis2 = select('sens-p2', [['', 'None — one axis only'], ...PARAMS], 'battery_power_kw');
  const values2 = el('input', { type: 'text', id: 'sens-v2', value: '50, 100, 150' });

  card.appendChild(el('div', { class: 'grid' }, [
    field('Scenario to vary', scenarioPick),
    field('First axis', axis1),
    field('First axis values', values1),
    field('Second axis', axis2),
    field('Second axis values', values2),
  ]));

  const go = el('button', { class: 'primary', type: 'button', text: 'Run sensitivity' });
  go.addEventListener('click', async () => {
    const parse = (input) => input.value.split(',').map((v) => Number(v.trim())).filter((v) => !Number.isNaN(v));
    const axes = [{ param: axis1.value, values: parse(values1) }];
    if (axis2.value) axes.push({ param: axis2.value, values: parse(values2) });

    go.disabled = true;
    go.textContent = 'Running…';
    const request = { scenario_id: scenarioPick.value, axes };
    const study = await guard(() => api(`/api/projects/${encodeURIComponent(state.project.name)}/sensitivity`, {
      method: 'POST',
      body: request,
    }));
    go.disabled = false;
    go.textContent = 'Run sensitivity';
    if (!study) return;
    state.study = study;
    state.studyRequest = request;
    renderSensitivity();
  });
  card.appendChild(el('div', { class: 'row', style: { marginTop: '10px' } }, go));
  root.appendChild(card);

  if (!state.study) return;
  const study = state.study;

  root.appendChild(heatCard(study, 'Net present value', (m) => m.npv_usd, 'USD', 'diverging'));
  root.appendChild(heatCard(study, 'Peak grid import', (m) => m.peak_import_kw, 'kW', 'sequential'));
  root.appendChild(heatCard(study, 'Unmet critical load', (m) => m.unmet_critical_kwh, 'kWh', 'sequential'));
  if (study.warnings?.length) {
    root.appendChild(el('div', { class: 'card' }, [el('h3', { text: 'Warnings' }), ledger(study.warnings)]));
  }
}

/**
 * Net present value has a sign, and the sign is the whole question, so it gets a
 * diverging ramp with a neutral zero. Peak import and unmet load are pure
 * magnitudes and get a single-hue ramp. A rainbow would invent boundaries the data
 * does not have.
 */
function heatCard(study, title, extract, unit, scale) {
  const card = el('div', { class: 'card' });
  card.appendChild(el('h3', { text: title }));

  const rows = study.axes[0];
  const cols = study.axes[1];
  const values = study.cells.map((c) => extract(c.metrics));
  const low = Math.min(...values);
  const high = Math.max(...values);
  const extent = Math.max(Math.abs(low), Math.abs(high), 1e-9);

  const SEQ = ['#cde2fb', '#9ec5f4', '#6da7ec', '#3987e5', '#2a78d6', '#256abf', '#184f95', '#0d366b'];
  const POS = ['#9ec5f4', '#5598e7', '#2a78d6', '#184f95'];
  const NEG = ['#f2b8b8', '#e87b7b', '#e34948', '#d03b3b'];
  const colourFor = (v) => {
    if (scale === 'diverging') {
      if (Math.abs(v) < extent * 0.02) return css('--gridline');
      const ramp = v >= 0 ? POS : NEG;
      return ramp[Math.min(ramp.length - 1, Math.round((Math.abs(v) / extent) * (ramp.length - 1)))];
    }
    const t = high - low < 1e-9 ? 0 : (v - low) / (high - low);
    return SEQ[Math.min(SEQ.length - 1, Math.round(t * (SEQ.length - 1)))];
  };

  const table = el('table', { class: 'heat' });
  const head = el('tr', {}, el('th', { text: cols ? '' : rows.param }));
  const colValues = cols ? cols.values : [null];
  for (const c of colValues) head.appendChild(el('th', { text: c === null ? title : String(c) }));
  table.appendChild(el('thead', {}, head));

  const body = el('tbody');
  rows.values.forEach((rowValue, r) => {
    const tr = el('tr', {}, el('th', { text: String(rowValue) }));
    colValues.forEach((_, c) => {
      const index = r * colValues.length + c;
      const cell = study.cells[index];
      if (!cell) { tr.appendChild(el('td', { text: '' })); return; }
      const value = extract(cell.metrics);
      tr.appendChild(el('td', {
        style: { background: colourFor(value), color: css('--ink') },
        // Values are printed in every cell, so the matrix reads without decoding the
        // colour at all — which is the relief channel the palette requires.
        text: fmt.n(value, 0),
        title: `${rows.param} ${rowValue}${cols ? ` · ${cols.param} ${cols.values[c]}` : ''}: ${fmt.n(value, 1)} ${unit}`,
      }));
    });
    body.appendChild(tr);
  });
  table.appendChild(body);
  card.appendChild(el('div', { class: 'scroll' }, table));
  card.appendChild(el('p', { class: 'chart-note', text:
    `Rows: ${rows.param}. ${cols ? `Columns: ${cols.param}. ` : ''}Values in ${unit}. ` +
    'Every cell was simulated in full.' }));
  return card;
}

// ---------------------------------------------------------------------------
// 5 — Export
// ---------------------------------------------------------------------------

function renderExport() {
  const root = document.getElementById('view-export');
  clear(root);

  const card = el('div', { class: 'card' });
  card.appendChild(el('h3', { text: 'Export' }));
  card.appendChild(el('p', { class: 'muted', text:
    'A self-contained HTML report with no script and no network access, the full results as JSON, ' +
    'every interval as CSV, a provenance manifest with the input hashes, and a SHA-256 checksum of ' +
    'every file. Written into your workspace folder.' }));

  if (!state.session.entitlements.client_report_rights) {
    card.appendChild(el('div', { class: 'banner', text:
      `The ${state.session.tier} tier does not carry client-facing report rights. Exported reports ` +
      'say so on their first page. See TERMS.md.' }));
  }

  const go = el('button', { class: 'primary', type: 'button', text: 'Export bundle' });
  go.addEventListener('click', async () => {
    go.disabled = true;
    go.textContent = 'Writing…';
    const bundle = await guard(() => api(`/api/projects/${encodeURIComponent(state.project.name)}/export`, {
      method: 'POST',
      body: { sensitivity: state.studyRequest },
    }));
    go.disabled = false;
    go.textContent = 'Export bundle';
    if (!bundle) return;
    state.bundle = bundle;
    renderExport();
    toast(`Wrote ${bundle.artifacts.length} files.`, null, true);
  });
  card.appendChild(el('div', { class: 'row' }, [
    go,
    state.study ? el('span', { class: 'tiny muted', text: 'The current sensitivity study will be included.' })
                : el('span', { class: 'tiny muted', text: 'Run a sensitivity study first to include one.' }),
  ]));
  root.appendChild(card);

  if (!state.bundle) return;
  const bundle = state.bundle;

  const files = el('div', { class: 'card' });
  files.appendChild(el('h3', { text: 'Bundle' }));
  files.appendChild(el('p', { class: 'mono tiny muted', text: bundle.directory }));
  for (const artifact of bundle.artifacts) {
    const href = `/api/projects/${encodeURIComponent(state.project.name)}/exports/` +
                 `${encodeURIComponent(bundle.key)}/${encodeURIComponent(artifact.filename)}`;
    const download = el('button', { type: 'button', text: 'Download' });
    download.addEventListener('click', async () => {
      const response = await fetch(href, { headers: { Authorization: `Bearer ${state.token}` } });
      if (!response.ok) { toast('Could not read that artifact.', 'export.download_failed'); return; }
      const blob = await response.blob();
      const url = URL.createObjectURL(blob);
      const link = el('a', { href: url, download: artifact.filename });
      document.body.appendChild(link);
      link.click();
      link.remove();
      URL.revokeObjectURL(url);
    });
    files.appendChild(el('div', { class: 'artifact' }, [
      el('div', {}, [
        el('div', { class: 'a-name', text: artifact.filename }),
        el('div', { class: 'a-desc', text: artifact.description }),
        el('div', { class: 'a-hash', text: `${artifact.sha256} · ${artifact.size_bytes.toLocaleString()} bytes` }),
      ]),
      download,
    ]));
  }
  root.appendChild(files);
}

// ---------------------------------------------------------------------------
// Small builders
// ---------------------------------------------------------------------------

function field(label, control) {
  return el('div', { class: 'field' }, [el('label', { text: label }), control]);
}

function select(id, options, value) {
  const node = el('select', { id });
  for (const [optionValue, label] of options) {
    node.appendChild(el('option', { value: optionValue, text: label }));
  }
  node.value = value ?? '';
  return node;
}

boot();
