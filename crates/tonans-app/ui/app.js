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

import {
  TARIFF_PRESETS,
  annualNote,
  chooseStudyAxis,
  chooseStudyScenario,
  encodeFileNameHeader,
  fileTooLarge,
  formatAxis,
  hasPriceAxis,
  parseValues,
  readOptionalNumber,
  resultsStatus,
  sendTagged,
  studySettings,
} from '/logic.mjs';

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

const state = {
  token: null,
  desktop: false,
  session: null,
  projects: [],
  project: null,
  /** The digest of the project file this window last read or wrote. */
  revision: null,
  /** True once this window has seen a different revision on disk. */
  changedElsewhere: false,
  /** Bumped by each open, so the slower of two opens cannot replace the newer. */
  openTicket: 0,
  saving: null,
  profile: null,
  savedJson: '',
  reportedUnsaved: false,
  undo: [],
  redo: [],
  editKey: null,
  selected: null,
  openSections: new Set(['battery', 'dispatch']),
  diffOpen: false,
  run: null,
  runStamp: null,
  resultsHash: null,
  timelines: {},
  detail: null,
  study: null,
  studyRequest: null,
  studyMetric: 'npv',
  sens: null,
  bundle: null,
  view: 'data',
  upload: null,
  mapping: null,
  importSettings: null,
  mappingNotes: [],
  mappingReviewed: false,
  importError: null,
  justImported: null,
  /** The request in flight, as `{ what, project, revision }`, or null. */
  busy: null,
};

/** Take the launch token and keep it in memory only. */
function claimToken() {
  // The desktop shell hands the token over directly, so it never appears in a URL.
  const boot = window.__TONANS_BOOT__;
  if (boot) {
    state.token = boot.token;
    state.desktop = Boolean(boot.desktop);
    delete window.__TONANS_BOOT__;
    return;
  }
  const params = new URLSearchParams(window.location.search);
  const token = params.get('token');
  if (token) {
    state.token = token;
    // Strip it from the address bar immediately: the token should not sit in
    // history, in a bookmark, or on screen over someone's shoulder.
    window.history.replaceState({}, '', window.location.pathname);
  }
}

/** Send a request. Resolves with the parsed body and the project revision it was about. */
async function request(path, { method = 'GET', body, raw, filename, revision } = {}) {
  const headers = { Authorization: `Bearer ${state.token}` };
  let payload;
  if (raw) {
    headers['Content-Type'] = 'text/csv';
    headers['X-Tonans-Filename'] = encodeFileNameHeader(filename || 'upload.csv');
    payload = raw;
  } else if (body !== undefined) {
    headers['Content-Type'] = 'application/json';
    payload = JSON.stringify(body);
  }
  if (revision) headers['X-Tonans-Revision'] = revision;

  let response;
  let text;
  try {
    response = await fetch(path, { method, headers, body: payload });
    text = await response.text();
  } catch {
    // All a page gets from a dropped connection is a TypeError saying "Load failed".
    const error = new Error('Tonans could not get an answer from its own local server. Nothing was changed by this request. Try again, or restart Tonans.');
    error.code = 'ui.no_answer';
    throw error;
  }
  let parsed = null;
  try { parsed = text ? JSON.parse(text) : null; } catch { parsed = null; }

  if (!response.ok) {
    const error = new Error(parsed?.message || `Request failed (${response.status})`);
    error.code = parsed?.code || `http.${response.status}`;
    throw error;
  }
  return { body: parsed, revision: response.headers.get('x-tonans-revision') };
}

async function api(path, options) {
  return (await request(path, options)).body;
}

/** What this window believes it is showing, to tag a request with and to check its answer. */
const currentStamp = () => ({ project: state.project?.name ?? null, revision: state.revision });

function markBusy(what, stamp) {
  state.busy = { what, project: stamp.project, revision: stamp.revision };
  return state.busy;
}

/** Only the request that set the mark can clear it: a save or an open may have moved on. */
function releaseBusy(mark) {
  if (state.busy === mark) state.busy = null;
}

const BUSY_LABEL = { run: 'Running…', study: 'Running…', export: 'Writing…' };

/** Say that an answer arrived too late to show. */
function dropLate(what, stamp) {
  toast(stamp.project === state.project?.name
    ? `The ${what} finished after the project changed, so it is not shown. Run it again for the current assumptions.`
    : `The ${what} of "${stamp.project}" finished after you left it, so it is not shown.`, 'ui.late_result');
}

const projectPath = (suffix = '') =>
  `/api/projects/${encodeURIComponent(state.project.name)}${suffix}`;

/** Ask the desktop shell for something only it can do. Rejects with `{ code, message }`. */
function shell(command, args) {
  return window.__TAURI__.core.invoke(command, args);
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
  baseline: '--ink-muted',
};
const paint = (role) => css(COLOUR[role] || '--ink');

/** The ink for text set inside a filled mark: white or near-black, by the fill's luminance. */
function inkOn(fill) {
  const match = /^#([0-9a-f]{2})([0-9a-f]{2})([0-9a-f]{2})$/i.exec(fill);
  if (!match) return css('--ink');
  const [r, g, b] = match.slice(1).map((part) => {
    const c = parseInt(part, 16) / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  });
  const luminance = 0.2126 * r + 0.7152 * g + 0.0722 * b;
  return css(luminance > 0.3 ? '--on-fill-dark' : '--on-fill-light');
}

// ---------------------------------------------------------------------------
// Formatting. Every displayed quantity carries its unit.
// ---------------------------------------------------------------------------

const grouped = (value, digits = 0) => value.toLocaleString(undefined, {
  minimumFractionDigits: digits, maximumFractionDigits: digits,
});

function siteFormat(options) {
  const timeZone = state.project?.site?.timezone;
  try {
    return new Intl.DateTimeFormat(undefined, { ...options, timeZone });
  } catch {
    return new Intl.DateTimeFormat(undefined, options);
  }
}

const fmt = {
  n(value, digits = 1) {
    if (value === null || value === undefined || Number.isNaN(value)) return '—';
    if (Math.abs(value) < 1e-9) return '0';
    return grouped(value, digits);
  },
  kwh: (v) => `${fmt.n(v, Math.abs(v) < 100 ? 1 : 0)} kWh`,
  kw: (v) => `${fmt.n(v, 1)} kW`,
  pct: (v) => `${fmt.n(v, 1)} %`,
  usd(v) {
    if (v === null || v === undefined) return '—';
    return `${grouped(v, Math.abs(v) < 1000 ? 2 : 0)} USD`;
  },
  years: (v) => `${v.toFixed(1)} years`,
  /** A time in the site's own timezone: the clock on the site's meter, not this PC's. */
  when: (iso) => siteFormat({ dateStyle: 'medium', timeStyle: 'short' }).format(new Date(iso)),
  day: (iso) => siteFormat({ month: 'short', day: 'numeric' }).format(new Date(iso)),
  dayHour: (iso) => siteFormat({ month: 'short', day: 'numeric', hour: 'numeric' }).format(new Date(iso)),
  signed(value, format) {
    if (Math.abs(value) < 1e-9) return 'same as baseline';
    return `${value > 0 ? '+' : ''}${format(value)} vs baseline`;
  },
};

function toast(message, code, ok = false, action = null) {
  const node = document.getElementById('toast');
  clear(node);
  const text = el('span');
  if (code) text.appendChild(el('span', { class: 't-code', text: code }));
  text.appendChild(document.createTextNode(message));
  node.appendChild(text);
  if (action) {
    node.appendChild(el('button', { type: 'button', text: action.label, onclick: () => {
      node.classList.add('hidden');
      action.run();
    } }));
  }
  node.className = ok ? 'toast ok' : 'toast';
  clearTimeout(toast.timer);
  toast.timer = setTimeout(() => node.classList.add('hidden'), ok ? 5000 : 12000);
}

/** Tell the user about a failure, in the one way each kind needs. */
function reportFailure(error) {
  if (error.code === 'conflict.project_changed') {
    state.changedElsewhere = true;
    redrawForChange();
    offerReopen();
    return;
  }
  toast(error.message, error.code);
}

async function guard(work) {
  try {
    return await work();
  } catch (error) {
    reportFailure(error);
    return null;
  }
}

// ---------------------------------------------------------------------------
// Dialogs
// ---------------------------------------------------------------------------

/** Show the one modal dialog. Resolves with what `close` is called with, or null on Esc. */
function openDialog(build) {
  const dialog = document.getElementById('dialog');
  clear(dialog);
  return new Promise((resolve) => {
    const close = (value) => {
      dialog.returnValue = '';
      dialog.close();
      resolve(value);
    };
    dialog.oncancel = (event) => { event.preventDefault(); close(null); };
    build(dialog, close);
    dialog.showModal();
  });
}

/** Ask what to do about unsaved edits. Resolves true if it is safe to carry on. */
async function settleUnsaved(what) {
  if (!isUnsaved()) return true;
  const choice = await openDialog((dialog, close) => {
    dialog.appendChild(el('h3', { text: 'This project has unsaved changes' }));
    dialog.appendChild(el('p', { text: `Save them before you ${what}?` }));
    dialog.appendChild(el('div', { class: 'dialog-actions' }, [
      el('button', { type: 'button', text: 'Keep working', onclick: () => close('cancel') }),
      el('button', { type: 'button', text: 'Discard changes', class: 'danger', onclick: () => close('discard') }),
      el('button', { type: 'button', text: 'Save', class: 'primary', onclick: () => close('save') }),
    ]));
  });
  // A save that fails says why, and leaves the user where they were.
  if (choice === 'save') return ensureSaved();
  if (choice === 'discard') {
    restoreEditable(state.savedJson);
    syncUnsaved();
    return true;
  }
  return false;
}

/**
 * Another window got there first. Nothing has been overwritten on either side, and the
 * safe answer is the one Enter gives: stay, with the edits still on screen to copy.
 */
async function offerReopen() {
  if (document.getElementById('dialog').open) {
    toast('This project was changed somewhere else. Nothing was saved.', 'conflict.project_changed');
    return;
  }
  const unsaved = isUnsaved();
  const choice = await openDialog((dialog, close) => {
    dialog.appendChild(el('h3', { text: 'This project was changed somewhere else' }));
    dialog.appendChild(el('p', { text:
      'Probably in another Tonans window. Nothing was saved or run from here, and the other change is intact.' }));
    dialog.appendChild(el('p', { text: unsaved
      ? 'Reopening loads the latest version and drops the unsaved changes on this screen. Keep working if you want to copy them first.'
      : 'Reopening loads the latest version.' }));
    dialog.appendChild(el('div', { class: 'dialog-actions' }, [
      el('button', { type: 'button', text: 'Keep working', onclick: () => close('keep') }),
      el('button', { type: 'button', text: 'Reopen the project', class: 'primary', onclick: () => close('reopen') }),
    ]));
  });
  if (choice !== 'reopen') return;
  restoreEditable(state.savedJson);
  syncUnsaved();
  await openProject(state.project.name);
}

/** Has anyone else written this project? Asked on focus and on each change of screen. */
async function checkForOutsideChange() {
  const stamp = currentStamp();
  if (!stamp.project) return;
  const found = await api(`/api/projects/${encodeURIComponent(stamp.project)}/revision`).catch(() => null);
  if (!found || state.project?.name !== stamp.project || state.revision !== stamp.revision) return;
  const changed = found.revision !== stamp.revision;
  if (changed === state.changedElsewhere) return;
  state.changedElsewhere = changed;
  redrawForChange();
}

/** Bring back into line every screen that says how current its contents are. */
function redrawForChange() {
  if (!state.project) return;
  renderBus();
  const redraw = { results: renderResults, sensitivity: renderSensitivity, export: renderExport }[state.view];
  if (redraw) redraw();
}

/** A sentence about how far to trust what a screen shows, or nothing if it is all current. */
function staleNotice(unsavedText) {
  if (state.changedElsewhere) {
    return el('div', { class: 'banner', text:
      'This project was changed somewhere else after this window read it. What is shown here is from the version ' +
      'this window opened, and saving from here would be refused. Reopen the project to see the latest.' });
  }
  return isUnsaved() ? el('div', { class: 'banner', text: unsavedText }) : null;
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

  document.getElementById('version-badge').textContent =
    `Version ${session.app_version}, model ${session.model_version}, schema v${session.schema_version}`;
  const path = document.getElementById('workspace-path');
  path.textContent = `Workspace: ${session.workspace}`;
  path.title = session.workspace;

  const quit = document.getElementById('quit');
  if (state.desktop) {
    quit.textContent = 'Quit';
    // A reload starts from what is saved, so the shell has nothing left to ask about.
    shell('set_unsaved', { unsaved: false }).catch(() => {});
    // The webview's own menu offers Back and Reload, which mean nothing in an application
    // and would drop unsaved edits. Text fields and selected text keep theirs: copy, paste.
    document.addEventListener('contextmenu', (event) => {
      const target = event.target;
      const editable = target instanceof HTMLElement && (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA');
      if (!editable && !window.getSelection()?.toString()) event.preventDefault();
    });
  }
  quit.addEventListener('click', quitApp);
  document.getElementById('save').addEventListener('click', () => guard(saveProject));
  document.getElementById('undo').addEventListener('click', undo);
  document.getElementById('redo').addEventListener('click', redo);
  document.getElementById('help').addEventListener('click', showHelp);
  document.getElementById('new-project').addEventListener('click', newProject);
  document.addEventListener('keydown', onKey);
  document.addEventListener('focusout', () => { state.editKey = null; });
  window.addEventListener('focus', checkForOutsideChange);

  await refreshProjects();
  if (state.projects.length) await openProject(state.projects[0].name);
}

async function quitApp() {
  if (state.desktop) {
    // The shell's close handler asks about unsaved changes itself, so the button and
    // the window's own close box behave the same way.
    await guard(() => shell('quit'));
    return;
  }
  if (!(await settleUnsaved('stop the server'))) return;
  await guard(() => api('/api/shutdown', { method: 'POST' }));
  document.body.textContent = 'Tonans has stopped. You can close this tab.';
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
        text: `${entry.scenario_count} scenario${entry.scenario_count === 1 ? '' : 's'}, ` +
              `${entry.has_dataset ? 'data imported' : 'no data yet'}`,
      }),
    ]);
    list.appendChild(el('li', {}, button));
  }
}

async function newProject() {
  // Before the dialog, so a project is never created while a question hangs over this one.
  if (!(await settleUnsaved('create a new project'))) return;
  const created = await openDialog((dialog, close) => {
    const name = el('input', { type: 'text', maxlength: '64', placeholder: 'Warehouse 3', autocomplete: 'off' });
    const timezone = timezoneInput(state.project?.site?.timezone || 'America/New_York');
    const failure = el('div', { class: 'banner critical hidden' });
    const create = async () => {
      failure.classList.add('hidden');
      try {
        const project = await api('/api/projects', {
          method: 'POST',
          body: { name: name.value.trim(), timezone: timezone.value.trim() },
        });
        close(project.name);
      } catch (error) {
        failure.textContent = error.message;
        failure.classList.remove('hidden');
      }
    };
    dialog.appendChild(el('h3', { text: 'New project' }));
    dialog.appendChild(el('p', { class: 'lede', text:
      'A project holds one site\'s interval data and the scenarios you compare on it.' }));
    dialog.appendChild(el('div', { class: 'stack' }, [
      field('Name', name),
      field('Site timezone (IANA name)', timezone),
      failure,
    ]));
    dialog.appendChild(el('div', { class: 'dialog-actions' }, [
      el('button', { type: 'button', text: 'Cancel', onclick: () => close(null) }),
      el('button', { type: 'button', text: 'Create project', class: 'primary', onclick: create }),
    ]));
    for (const input of [name, timezone]) input.addEventListener('keydown', (event) => { if (event.key === 'Enter') create(); });
    setTimeout(() => name.focus(), 0);
  });
  if (!created) return;
  await refreshProjects();
  await openProject(created);
  toast(`Created "${created}".`, null, true);
}

/** A free-text timezone box, with the IANA names offered where the webview knows them. */
function timezoneInput(value) {
  const input = el('input', { type: 'text', maxlength: '64', value, autocomplete: 'off', list: 'tz-names' });
  if (!document.getElementById('tz-names') && Intl.supportedValuesOf) {
    const list = el('datalist', { id: 'tz-names' });
    for (const name of Intl.supportedValuesOf('timeZone')) list.appendChild(el('option', { value: name }));
    document.body.appendChild(list);
  }
  return input;
}

async function openProject(name) {
  if (!(await settleUnsaved(`open ${name}`))) return;
  const ticket = ++state.openTicket;
  const reply = await guard(() => request(`/api/projects/${encodeURIComponent(name)}`));
  if (!reply || ticket !== state.openTicket) return;
  const project = reply.body;
  forgetStagedUpload();
  state.project = project;
  state.revision = reply.revision;
  state.changedElsewhere = false;
  state.profile = null;
  dropDerivedResults();
  state.timelines = {};
  state.sens = null;
  state.upload = null;
  state.mapping = null;
  state.importError = null;
  state.justImported = null;
  state.undo = [];
  state.redo = [];
  state.editKey = null;
  state.selected = project.scenarios[0]?.id ?? null;
  state.detail = null;
  markSaved();

  if (project.dataset) {
    const profile = await guard(() => api(projectPath('/profile')));
    if (ticket !== state.openTicket) return;
    state.profile = profile;
  }

  document.getElementById('empty-state').classList.add('hidden');
  document.getElementById('bus').classList.remove('hidden');
  document.getElementById('next').classList.remove('hidden');
  document.getElementById('project-name').textContent = project.name;

  renderProjectSummary(project);
  await refreshProjects();
  show(project.dataset ? 'scenarios' : 'data');
}

function isSyntheticProject(project) {
  return project.name === state.session.demo_project ||
    Boolean(project.dataset?.import_warnings?.some((w) => w.code === 'data.synthetic_sample'));
}

function renderProjectSummary(project) {
  const node = document.getElementById('project-summary');
  clear(node);
  const dataset = project.dataset;
  // The most damaging thing this application could do is present invented data as a
  // real building's, so synthetic data says so at the top of every screen.
  if (dataset && isSyntheticProject(project)) {
    node.appendChild(el('span', { class: 'synthetic', text: 'Synthetic data, not a real site. ' }));
  }
  node.appendChild(document.createTextNode(dataset
    ? `${dataset.interval_count.toLocaleString()} intervals of ${dataset.interval_minutes}-minute data, ` +
      `${project.site.timezone}, from ${dataset.source.filename}`
    : `${project.site.timezone}, no data imported yet`));
}

const VIEWS = ['data', 'scenarios', 'results', 'sensitivity', 'export'];

function show(view, { keepScroll = false } = {}) {
  state.view = view;
  for (const name of VIEWS) {
    document.getElementById(`view-${name}`).classList.toggle('hidden', name !== view);
  }
  const render = { data: renderData, scenarios: renderScenarios, results: renderResults,
                   sensitivity: renderSensitivity, export: renderExport }[view];
  if (render) render();
  renderBus();
  if (!keepScroll) document.getElementById('views').scrollTop = 0;
  checkForOutsideChange();
}

// ---------------------------------------------------------------------------
// The bus: where you are, what is done, and what comes next
// ---------------------------------------------------------------------------

function stepStates() {
  const project = state.project;
  const dataset = project.dataset;
  const count = project.scenarios.length;
  const unsaved = isUnsaved();
  const old = state.changedElsewhere ? ', out of date' : (unsaved ? ', from the last save' : '');
  return [
    { view: 'data', name: '1 Data', done: Boolean(dataset),
      note: dataset ? `${dataset.interval_count.toLocaleString()} intervals` : 'Nothing imported' },
    { view: 'scenarios', name: '2 Scenarios', done: count > 1 && !unsaved,
      note: `${count} scenario${count === 1 ? '' : 's'}${unsaved ? ', unsaved' : ''}` },
    { view: 'results', name: '3 Results', done: Boolean(state.run),
      note: resultsStatus(state.runStamp, currentStamp(), { unsaved, changedElsewhere: state.changedElsewhere }) },
    { view: 'sensitivity', name: '4 Sensitivity', done: Boolean(state.study),
      note: state.study ? `${state.study.cells.length} cells${old}` : 'Optional' },
    { view: 'export', name: '5 Export', done: Boolean(state.bundle),
      note: state.bundle ? `${state.bundle.artifacts.length} files written${old}` : 'Nothing written' },
  ];
}

function renderBus() {
  if (!state.project) return;
  const bus = document.getElementById('bus');
  clear(bus);
  const steps = stepStates();
  steps.forEach((step, index) => {
    // A segment is live once the step feeding it is done. Sensitivity is optional, so
    // results alone carry the conductor through to export.
    const live = step.done || (step.view === 'sensitivity' && steps[2].done);
    bus.appendChild(el('button', {
      type: 'button',
      role: 'tab',
      class: `step${step.done ? ' done' : ''}${live && index < steps.length - 1 ? ' live' : ''}`,
      'aria-selected': state.view === step.view ? 'true' : 'false',
      title: `Ctrl+${index + 1}`,
      onclick: () => show(step.view),
    }, [
      el('span', { class: 'step-name', text: step.name }),
      el('span', { class: 'step-note', text: step.note }),
    ]));
  });
  renderNext();
}

/** The next useful thing, in one line, with the button that does it. */
function nextStep() {
  const project = state.project;
  if (state.changedElsewhere) {
    return { text: 'This project was changed somewhere else, probably in another window.',
             label: 'Reopen the project', run: offerReopen };
  }
  if (!project.dataset) {
    return state.upload
      ? { text: 'Map every column below, then import.' }
      : { text: 'Import a year of interval data to begin. Or try the sample data.',
          label: state.view === 'data' ? null : 'Go to data', run: () => show('data') };
  }
  if (isUnsaved()) return { text: 'You have unsaved changes.', label: 'Save', run: () => guard(saveProject) };
  if (project.scenarios.length < 2) {
    return { text: 'Add a candidate to compare with the baseline.', label: 'Add a scenario', run: addScenario };
  }
  if (!state.run) return { text: 'Run every scenario over every interval.', label: 'Run all scenarios', run: runAll };
  if (state.view === 'sensitivity' && !state.study && !state.bundle) {
    return { text: 'Choose what to vary and run the study, or go straight to export.',
             label: 'Go to export', run: () => show('export') };
  }
  if (!state.study && !state.bundle && state.view === 'results') {
    return { text: 'Test how the results move with size, price, or reserve, or go straight to export.',
             label: 'Open sensitivity', run: () => show('sensitivity') };
  }
  if (!state.study && !state.bundle && state.view !== 'export') {
    return { text: 'Results are current.', label: 'Compare results', run: () => show('results') };
  }
  if (!state.bundle) {
    return state.view === 'export'
      ? { text: 'Write the report, the data, and the provenance manifest.', label: 'Export bundle', run: runExport }
      : { text: 'Write the report, the data, and the provenance manifest.', label: 'Go to export', run: () => show('export') };
  }
  return { text: 'Everything is current. The bundle matches these results.' };
}

function renderNext() {
  const node = document.getElementById('next');
  clear(node);
  const next = nextStep();
  node.appendChild(el('span', { class: 'next-label', text: 'Next' }));
  node.appendChild(el('span', { class: 'muted', text: next.text }));
  if (next.label) node.appendChild(el('button', { type: 'button', text: next.label, onclick: next.run }));
}

// ---------------------------------------------------------------------------
// Saving, unsaved changes, and undo
// ---------------------------------------------------------------------------

/** The parts of a project this screen edits, as text. Comparing it is what "unsaved" means. */
function editableJson() {
  const { scenarios, baseline_scenario_id: baseline, notes } = state.project;
  return JSON.stringify({ scenarios, baseline, notes: notes ?? '' });
}

function restoreEditable(json, selected = state.selected) {
  const { scenarios, baseline, notes } = JSON.parse(json);
  state.project.scenarios = scenarios;
  state.project.baseline_scenario_id = baseline;
  state.project.notes = notes;
  state.selected = scenarios.some((s) => s.id === selected) ? selected : (scenarios[0]?.id ?? null);
}

/** Everything that was worked out from a saved project. All of it goes when that changes. */
function dropDerivedResults() {
  state.run = null;
  state.runStamp = null;
  state.resultsHash = null;
  state.study = null;
  state.studyRequest = null;
  state.bundle = null;
  state.busy = null;
}

function isUnsaved() {
  return Boolean(state.project) && editableJson() !== state.savedJson;
}

function markSaved() {
  state.savedJson = editableJson();
  syncUnsaved();
}

/** Bring every "unsaved" signal into line, including the one the shell asks before closing. */
function syncUnsaved() {
  const unsaved = isUnsaved();
  document.getElementById('save').disabled = !unsaved;
  document.getElementById('undo').disabled = state.undo.length === 0;
  document.getElementById('redo').disabled = state.redo.length === 0;
  document.getElementById('undo').title = state.undo.length
    ? `Undo: ${state.undo[state.undo.length - 1].label} (Ctrl+Z)` : 'Nothing to undo';
  document.getElementById('redo').title = state.redo.length
    ? `Redo: ${state.redo[state.redo.length - 1].label} (Ctrl+Shift+Z)` : 'Nothing to redo';
  const label = document.getElementById('save-state');
  label.textContent = unsaved ? 'Unsaved changes' : (state.project ? 'All changes saved' : '');
  label.className = `save-state${unsaved ? ' unsaved' : ''}`;
  if (state.desktop && unsaved !== state.reportedUnsaved) {
    state.reportedUnsaved = unsaved;
    shell('set_unsaved', { unsaved }).catch(() => {});
  }
  renderBus();
}

const UNDO_LIMIT = 100;

function checkpoint(label) {
  state.undo.push({ label, json: editableJson(), selected: state.selected });
  if (state.undo.length > UNDO_LIMIT) state.undo.shift();
  state.redo = [];
}

/** Record one user edit. Repeated edits to one field, while it has focus, are one undo step. */
function recordEdit(key, label, mutate) {
  if (state.editKey !== key) {
    checkpoint(label);
    state.editKey = key;
  }
  mutate();
  syncUnsaved();
}

/** A change to the shape of the project: undoable, and it redraws the editor. */
function change(label, mutate) {
  state.editKey = null;
  checkpoint(label);
  mutate();
  syncUnsaved();
  if (state.view === 'scenarios') renderScenarios();
}

function undo() {
  const entry = state.undo.pop();
  if (!entry) return;
  state.redo.push({ label: entry.label, json: editableJson(), selected: state.selected });
  restoreEditable(entry.json, entry.selected);
  state.editKey = null;
  show(state.view, { keepScroll: true });
  syncUnsaved();
  toast(`Undid: ${entry.label}.`, null, true);
}

function redo() {
  const entry = state.redo.pop();
  if (!entry) return;
  state.undo.push({ label: entry.label, json: editableJson(), selected: state.selected });
  restoreEditable(entry.json, entry.selected);
  state.editKey = null;
  show(state.view, { keepScroll: true });
  syncUnsaved();
  toast(`Redid: ${entry.label}.`, null, true);
}

/** Refuse to save a number field the user left blank or unreadable, rather than save 0. */
function firstInvalidField() {
  return document.querySelector('.view:not(.hidden) input[aria-invalid="true"]');
}

/** One save at a time: a second would carry a revision the first is about to replace. */
function saveProject() {
  if (!state.saving) state.saving = writeProject().finally(() => { state.saving = null; });
  return state.saving;
}

async function writeProject() {
  const invalid = firstInvalidField();
  if (invalid) {
    invalid.closest('details')?.setAttribute('open', '');
    invalid.focus();
    const failure = new Error('This field needs a number. Enter one, or undo the change.');
    failure.code = 'ui.blank_number';
    throw failure;
  }
  const stamp = currentStamp();
  const sent = editableJson();
  let reply;
  try {
    reply = await request(projectPath(), { method: 'PUT', body: state.project, revision: stamp.revision });
  } catch (error) {
    // Point at the scenario the error names, rather than leaving the reader to hunt.
    const named = /scenario\[([^\]]+)\]/.exec(error.message);
    if (named && state.project.scenarios.some((s) => s.id === named[1])) {
      state.selected = named[1];
      if (state.view === 'scenarios') renderScenarios();
    }
    throw error;
  }
  if (state.project?.name !== stamp.project) return reply.body;
  // Anything typed while the save was on its way is not in it, and is still unsaved.
  const typedSince = editableJson() === sent ? null : editableJson();
  state.project = reply.body;
  state.revision = reply.revision;
  state.changedElsewhere = false;
  // Results were produced from the old assumptions and are now stale. Throwing them
  // away is the only honest thing to do: a chart next to assumptions it was not
  // produced from is worse than no chart.
  dropDerivedResults();
  markSaved();
  if (typedSince) {
    restoreEditable(typedSince);
    syncUnsaved();
  }
  renderProjectSummary(reply.body);
  // The inputs on screen are bound to the object just replaced. Redraw them, or the
  // next edit changes that old object and the next save silently drops it.
  show(state.view, { keepScroll: true });
  toast('Saved. Run again to refresh results.', null, true);
  await refreshProjects();
  return reply.body;
}

/**
 * The server runs what is saved, not what is on screen. Anything that reads the project
 * saves first, so a result can never quietly ignore the edits above it.
 */
async function ensureSaved() {
  if (!isUnsaved()) return true;
  try {
    await saveProject();
    return true;
  } catch (error) {
    reportFailure(error);
    return false;
  }
}

function onKey(event) {
  const mod = event.ctrlKey || event.metaKey;
  const target = event.target;
  const typing = target instanceof HTMLElement &&
    (target.tagName === 'INPUT' || target.tagName === 'TEXTAREA' || target.tagName === 'SELECT');
  const modal = document.getElementById('dialog').open;

  if (mod && event.key.toLowerCase() === 'q' && state.desktop) {
    event.preventDefault();
    quitApp();
    return;
  }
  if (modal) return;

  if (mod && event.key.toLowerCase() === 's') {
    event.preventDefault();
    if (isUnsaved()) guard(saveProject);
  } else if (mod && event.key === 'Enter') {
    event.preventDefault();
    if (state.project?.dataset) runAll();
  } else if (mod && /^[1-5]$/.test(event.key)) {
    event.preventDefault();
    if (state.project) show(VIEWS[Number(event.key) - 1]);
  } else if (mod && event.key.toLowerCase() === 'd') {
    event.preventDefault();
    if (state.project && state.view === 'scenarios') duplicateSelected();
  } else if (mod && event.key.toLowerCase() === 'z' && !typing) {
    // In a text field, Ctrl+Z belongs to that field.
    event.preventDefault();
    if (event.shiftKey) redo(); else undo();
  } else if (event.key === '?' && !typing && !mod) {
    event.preventDefault();
    showHelp();
  }
}

function showHelp() {
  openDialog((dialog, close) => {
    dialog.appendChild(el('h3', { text: 'Help' }));
    const keys = [
      ['Ctrl+S', 'Save the project'],
      ['Ctrl+Enter', 'Save if needed, then run all scenarios'],
      ['Ctrl+1 to Ctrl+5', 'Go to data, scenarios, results, sensitivity, export'],
      ['Ctrl+D', 'Duplicate the selected scenario'],
      ['Ctrl+Z, Ctrl+Shift+Z', 'Undo and redo scenario changes (outside a text field)'],
      ['?', 'Show this help'],
    ];
    if (state.desktop) keys.push(['Ctrl+Q', 'Quit']);
    dialog.appendChild(el('h4', { text: 'Keyboard' }));
    dialog.appendChild(el('div', { class: 'keys' }, keys.flatMap(([key, what]) => [
      el('kbd', { text: key }), el('span', { text: what }),
    ])));

    dialog.appendChild(el('h4', { text: 'What the model does not do' }));
    dialog.appendChild(el('ul', {}, state.session.model_exclusions.map((line) => el('li', { text: line }))));

    dialog.appendChild(el('h4', { text: 'Where things are' }));
    dialog.appendChild(el('p', { class: 'mono', text: state.session.workspace }));
    dialog.appendChild(el('p', {}, [
      outsideLink('source', 'Source code', state.session.source_url),
      document.createTextNode(', '),
      outsideLink('support', state.session.support_email, `mailto:${state.session.support_email}`),
    ]));
    dialog.appendChild(el('div', { class: 'dialog-actions' }, [
      el('button', { type: 'button', text: 'Close', onclick: () => close(null) }),
    ]));
  });
}

/** A link that leaves Tonans. The shell opens it with the OS, by name, never by URL. */
function outsideLink(name, text, href) {
  const link = el('a', { href, text, target: '_blank', rel: 'noreferrer' });
  link.addEventListener('click', (event) => {
    if (!state.desktop) return;
    event.preventDefault();
    guard(() => shell('open_link', { link: name }));
  });
  return link;
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

  if (state.justImported) root.appendChild(importedBanner(state.justImported));
  if (state.upload) root.appendChild(mappingCard(state.upload));
  if (project.dataset) root.appendChild(dataSummary(project.dataset));
  if (!state.upload) root.appendChild(sourceChooser(project));
  root.appendChild(notesCard(project));
}

/** What just happened, and the one thing to do about it. */
function importedBanner(imported) {
  const next = state.project.scenarios.length > 1
    ? el('button', { type: 'button', class: 'primary', text: 'Review the scenarios', onclick: () => show('scenarios') })
    : el('button', { type: 'button', class: 'primary', text: 'Add a scenario to compare', onclick: addScenario });
  const series = state.project.dataset?.series ?? {};
  const hasPrices = Boolean(series.import_price && series.export_price);
  const flat = state.project.scenarios.filter((s) => s.tariff.import.source === 'flat');
  const usePrices = hasPrices && flat.length
    ? el('button', { type: 'button', text: 'Use the imported prices in every scenario', onclick: () => {
      change('use the imported prices', () => {
        for (const scenario of state.project.scenarios) {
          scenario.tariff.import = { source: 'series' };
          scenario.tariff.export = { source: 'series' };
        }
      });
      renderData();
      toast('Every scenario now prices energy from your file.', null, true, { label: 'Undo', run: undo });
    } })
    : null;
  return el('div', { class: 'banner ok' }, [
    el('p', { text: `Imported ${imported.interval_count.toLocaleString()} intervals. ` +
      'Check the figures below against what you know about this site, then set up the scenarios to compare.' }),
    flat.length
      ? el('p', { text: hasPrices
        ? 'Your file has price columns, but the tariff here is still Tonans\'s round example flat rate.'
        : 'The tariff here is still Tonans\'s round example flat rate. Set your own on each scenario.' })
      : null,
    imported.warnings?.length ? ledger(imported.warnings) : null,
    el('div', { class: 'row' }, [next, usePrices]),
  ]);
}

function sourceChooser(project) {
  const box = el('section', { class: 'stack' });
  box.appendChild(el('div', {}, [
    el('h3', { text: project.dataset ? 'Replace the interval data' : 'Import interval data' }),
    el('p', { class: 'lede', text:
      'Tonans reads the file you choose and nothing else. You state the timezone, the interval, and the ' +
      'unit of every column, and every column must be mapped or explicitly ignored. Tonans never guesses ' +
      'silently: it can suggest a mapping from your headers, and you review it before anything is read.' }),
  ]));

  const picker = el('input', { type: 'file', accept: '.csv,text/csv', id: 'csv-file', class: 'hidden' });
  picker.addEventListener('change', async () => {
    const file = picker.files[0];
    picker.value = '';
    if (!file) return;
    // From the file's metadata, before any of it is read: a page that reads first runs
    // out of memory on the file it was going to refuse.
    const refusal = fileTooLarge(file.name, file.size, state.session.limits.max_csv_bytes);
    if (refusal) {
      toast(refusal, 'limit.csv_bytes');
      return;
    }
    const into = state.project.name;
    const receipt = await guard(async () => {
      const bytes = await readFile(file);
      return api('/api/upload', { method: 'POST', raw: bytes, filename: file.name });
    });
    if (receipt) receiveUpload(receipt, into);
  });
  const sample = el('button', { type: 'button', text: 'Use the sample data', onclick: async () => {
    const into = state.project.name;
    const receipt = await guard(() => api('/api/upload/sample', { method: 'POST' }));
    if (receipt) receiveUpload(receipt, into);
  } });
  const template = el('button', { type: 'button', text: 'Save a CSV template', onclick: saveTemplate });
  box.appendChild(el('div', { class: 'row' }, [
    picker,
    el('button', { type: 'button', class: project.dataset ? '' : 'primary', text: 'Choose a CSV file',
                   onclick: () => picker.click() }),
    sample,
    template,
  ]));
  box.appendChild(el('p', { class: 'tiny muted', text:
    'The sample is one synthetic year of hourly data for an invented light-industrial site. It goes through ' +
    'the same mapping and import as your own file, and the project records that it is synthetic. The template ' +
    'has every column Tonans can read and three rows showing the format of each.' }));
  return box;
}

/** The page's own error for a file that has gone is a number and a riddle. */
async function readFile(file) {
  try {
    return new Uint8Array(await file.arrayBuffer());
  } catch {
    const error = new Error(`Tonans could not read "${file.name}". It may have been moved or deleted, or you may not have permission to read it.`);
    error.code = 'ui.file_unreadable';
    throw error;
  }
}

function receiveUpload(receipt, into) {
  // The user has moved on to another project, and this file was not meant for it.
  if (state.project?.name !== into) {
    api('/api/upload/cancel', { method: 'POST', body: { upload_id: receipt.upload_id } }).catch(() => {});
    return;
  }
  state.upload = receipt;
  state.mapping = receipt.headers.map(() => ({ role: '__unmapped', unit: '', why: null, check: null }));
  state.importSettings = {
    timezone: state.project.site.timezone, interval: '60', intervalNote: null, gap: 'reject', gapMax: '2',
  };
  state.mappingNotes = [];
  state.mappingReviewed = false;
  state.importError = null;
  state.justImported = null;
  renderData();
  renderNext();
}

/** Tell the server to drop the file it is holding, now rather than at the next upload. */
function forgetStagedUpload() {
  if (!state.upload) return;
  api('/api/upload/cancel', { method: 'POST', body: { upload_id: state.upload.upload_id } }).catch(() => {});
}

async function saveTemplate() {
  if (state.desktop) {
    const saved = await guard(() => shell('save_csv_template'));
    if (saved) toast(`Saved ${saved}.`, null, true);
    return;
  }
  const response = await fetch('/api/templates/interval-csv', { headers: { Authorization: `Bearer ${state.token}` } });
  if (response.ok) downloadBlob(await response.blob(), 'tonans-interval-template.csv');
}

function downloadBlob(blob, filename) {
  const url = URL.createObjectURL(blob);
  const link = el('a', { href: url, download: filename });
  document.body.appendChild(link);
  link.click();
  link.remove();
  URL.revokeObjectURL(url);
}

/** The imported data, described. No model is involved in any of these figures. */
function dataSummary(dataset) {
  const box = el('section', { class: 'stack' });
  box.appendChild(el('h3', { text: 'What was imported' }));

  const profile = state.profile;
  if (profile) {
    const tiles = [
      tile('Peak load', fmt.kw(profile.load.peak_kw), fmt.when(profile.load.peak_utc)),
      tile('Mean load', fmt.kw(profile.load.mean_kw), `${fmt.kwh(profile.load.energy_kwh)} in ${grouped(profile.covered_hours)} hours`),
    ];
    if (profile.generation) {
      tiles.push(tile('Generation', fmt.kwh(profile.generation.energy_kwh), `peak ${fmt.kw(profile.generation.peak_kw)}`));
      tiles.push(tile('Peak load net of generation', fmt.kw(profile.peak_net_load_kw), 'highest load minus generation in one interval'));
    }
    if (profile.critical_load) {
      tiles.push(tile('Critical load', fmt.kw(profile.critical_load.mean_kw), `mean, peak ${fmt.kw(profile.critical_load.peak_kw)}`));
    }
    if (profile.import_price) {
      tiles.push(tile('Import price', `${profile.import_price.mean.toFixed(3)} USD/kWh`,
        `mean, from ${profile.import_price.min.toFixed(3)} to ${profile.import_price.max.toFixed(3)}`));
    }
    box.appendChild(el('div', { class: 'tiles' }, tiles));
    box.appendChild(monthCharts(profile));
  }

  if (dataset.import_warnings?.length) box.appendChild(ledger(dataset.import_warnings));

  const rows = [
    ['Source file', dataset.source.filename],
    ['Digest', dataset.source.sha256],
    ['Size', `${dataset.source.size_bytes.toLocaleString()} bytes, ${dataset.source.row_count.toLocaleString()} data rows`],
    ['Interval', `${dataset.interval_minutes} minutes, ${dataset.interval_count.toLocaleString()} intervals`],
    ['Starts', fmt.when(dataset.start_utc)],
    ['Gap policy', dataset.gap_policy.mode.replace(/_/g, ' ')],
    ['Synthesized intervals', dataset.synthesized_interval_count === 0
      ? 'none: every interval is a measurement'
      : `${dataset.synthesized_interval_count} of ${dataset.interval_count} were NOT measured`],
    ['Imported', fmt.when(dataset.source.imported_utc)],
  ];
  const table = el('table');
  const body = el('tbody');
  for (const [key, value] of rows) {
    body.appendChild(el('tr', {}, [
      el('th', { scope: 'row', text: key }),
      el('td', { class: key === 'Digest' ? 'mono' : '', text: value }),
    ]));
  }
  for (const [role, column] of Object.entries(dataset.source.column_mapping)) {
    body.appendChild(el('tr', {}, [
      el('th', { scope: 'row', text: `Column for ${role.replace(/_/g, ' ')}` }),
      el('td', { text: column }),
    ]));
  }
  table.appendChild(body);
  box.appendChild(el('details', {}, [
    el('summary', { text: 'File, digest, and column mapping' }),
    el('div', { class: 'scroll' }, table),
  ]));
  return box;
}

function monthCharts(profile) {
  const label = (m) => new Date(Date.UTC(m.year, m.month - 1, 1))
    .toLocaleDateString(undefined, { month: 'short', timeZone: 'UTC' });
  const charts = [
    { title: 'Peak load by month, kW', role: 'load', read: (m) => m.peak_load_kw, format: fmt.kw },
    { title: 'Load energy by month, kWh', role: 'load', read: (m) => m.load_kwh, format: fmt.kwh },
  ];
  if (profile.generation) {
    charts.push({ title: 'Generation by month, kWh', role: 'generation', read: (m) => m.generation_kwh, format: fmt.kwh });
  }

  const grid = el('div', { class: 'months' });
  for (const chart of charts) {
    const values = profile.months.map(chart.read);
    const high = Math.max(...values, 1e-9);
    const peak = values.indexOf(Math.max(...values));
    const columns = el('div', { class: 'columns' });
    profile.months.forEach((month, index) => {
      columns.appendChild(el('div', {
        class: 'column',
        title: `${label(month)} ${month.year}: ${chart.format(values[index])}`,
        style: { height: `${(values[index] / high) * 100}%`, background: paint(chart.role) },
      }, index === peak ? el('span', { class: 'column-label', text: grouped(values[index]) }) : null));
    });
    grid.appendChild(el('div', {}, [
      el('h4', { text: chart.title, style: { marginTop: '0' } }),
      columns,
      el('div', { class: 'column-axis' }, profile.months.map((m) => el('span', { text: label(m).slice(0, 1) }))),
    ]));
  }

  const rows = profile.months.map((m) => [
    `${label(m)} ${m.year}`, fmt.kw(m.peak_load_kw), fmt.kwh(m.load_kwh),
    ...(profile.generation ? [fmt.kwh(m.generation_kwh)] : []),
  ]);
  const head = ['Month', 'Peak load', 'Load energy', ...(profile.generation ? ['Generation'] : [])];
  const table = el('table', {}, [
    el('thead', {}, el('tr', {}, head.map((h, i) => el('th', { class: i ? 'num' : '', text: h })))),
    el('tbody', {}, rows.map((row) => el('tr', {}, row.map((cell, i) => el('td', { class: i ? 'num' : '', text: cell }))))),
  ]);
  return el('div', {}, [
    grid,
    el('details', {}, [el('summary', { text: 'These figures as a table' }), el('div', { class: 'scroll' }, table)]),
  ]);
}

function notesCard(project) {
  const notes = el('textarea', { rows: '3', maxlength: String(state.session.limits.max_notes_chars ?? 4000) });
  notes.value = project.notes ?? '';
  notes.addEventListener('input', () => {
    recordEdit('project-notes', 'edit notes', () => { project.notes = notes.value; });
  });
  return el('section', { class: 'stack' }, [
    el('div', {}, [
      el('h3', { text: 'Notes' }),
      el('p', { class: 'lede', text: 'Reproduced in the exported report.' }),
    ]),
    notes,
  ]);
}

function mappingCard(receipt) {
  const card = el('section', { class: 'panel stack' });
  const settings = state.importSettings;
  card.appendChild(el('div', {}, [
    el('h3', { text: `Map the columns of ${receipt.filename}` }),
    el('p', { class: 'lede', text:
      `${receipt.row_count.toLocaleString()} data rows, ${receipt.headers.length} columns. Nothing has been interpreted yet.` }),
    el('p', { class: 'mono tiny muted', text: receipt.sha256 }),
  ]));

  const timezone = timezoneInput(settings.timezone);
  timezone.id = 'imp-tz';
  timezone.addEventListener('input', () => { settings.timezone = timezone.value; });
  const interval = el('input', { type: 'number', id: 'imp-interval', value: settings.interval, min: '1', max: '60' });
  interval.addEventListener('input', () => { settings.interval = interval.value; settings.intervalNote = null; });
  const gap = select('imp-gap', [
    ['reject', 'Reject the file (the safe default)'],
    ['fill_zero', 'Fill with zero: the filled intervals are NOT measurements'],
    ['hold_last_value', 'Hold the previous value: the filled intervals are NOT measurements'],
  ], settings.gap);
  gap.addEventListener('change', () => { settings.gap = gap.value; });
  const gapMax = el('input', { type: 'number', id: 'imp-gap-max', value: settings.gapMax, min: '1', max: '24' });
  gapMax.addEventListener('input', () => { settings.gapMax = gapMax.value; });

  card.appendChild(el('div', { class: 'grid' }, [
    field('Timezone (IANA name)', timezone),
    el('div', { class: 'field' }, [
      el('label', { text: 'Interval (minutes)' }),
      interval,
      settings.intervalNote ? el('span', { class: 'field-note', text: settings.intervalNote }) : null,
    ]),
    field('Missing intervals', gap),
    field('Longest run to fill', gapMax),
  ]));

  const suggest = el('button', { type: 'button', text: 'Suggest mapping from headers', onclick: () => suggestFromHeaders(receipt) });
  card.appendChild(el('div', { class: 'row' }, [
    suggest,
    el('span', { class: 'tiny muted', text:
      'Fills the choices below from your column names, with the reason for each. Nothing is read until you confirm.' }),
  ]));
  // A column left unsuggested is a question for the user, so it is not set in small print.
  if (state.mappingNotes.length) {
    card.appendChild(el('div', { class: 'banner' }, state.mappingNotes.map((note) => el('p', { text: note }))));
  }

  const table = el('table', { class: 'map-table' });
  table.appendChild(el('thead', {}, el('tr', {}, [
    el('th', { text: 'Column in your file' }),
    el('th', { text: 'First values' }),
    el('th', { text: 'Means' }),
    el('th', { text: 'Unit' }),
    el('th', { text: 'Why this was suggested' }),
  ])));
  const body = el('tbody');

  receipt.headers.forEach((header, index) => {
    const entry = state.mapping[index];
    const roleSelect = select(`role-${index}`, [
      ['__unmapped', 'Choose meaning…'],
      ['__timestamp', 'Timestamp'],
      ...ROLES.map(([id, label]) => [id, label]),
      ['demand_period', 'Demand-charge period label'],
      ['__ignore', 'Ignore this column'],
    ], entry.role);

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
      unitSelect.value = spec[2].includes(entry.unit) ? entry.unit : '';
    };
    syncUnits();

    const why = el('td');
    const row = el('tr', {}, [
      el('td', { class: 'mono', text: header }),
      el('td', {}, receipt.sample_rows.slice(0, 2).map((sampleRow) =>
        el('div', { class: 'sample', text: sampleRow[index] ?? '' }))),
      el('td', {}, roleSelect),
      el('td', {}, unitSelect),
      why,
    ]);
    const paintWhy = () => {
      clear(why);
      row.className = entry.why ? (entry.check ? 'suggested-check' : 'suggested') : '';
      if (entry.why) why.appendChild(el('span', { class: 'why', text: entry.why }));
      if (entry.check) why.appendChild(el('span', { class: 'why check', text: entry.check }));
    };
    paintWhy();

    // A row the user has touched is the user's: its reason no longer applies.
    roleSelect.addEventListener('change', () => {
      entry.role = roleSelect.value;
      entry.unit = '';
      entry.why = null;
      entry.check = null;
      syncUnits();
      paintWhy();
      syncImportButton();
    });
    unitSelect.addEventListener('change', () => {
      entry.unit = unitSelect.value;
      entry.check = null;
      paintWhy();
    });
    body.appendChild(row);
  });
  table.appendChild(body);
  card.appendChild(el('div', { class: 'scroll' }, table));

  if (state.importError) {
    card.appendChild(el('div', { class: 'banner critical' }, [
      el('span', { class: 't-code', text: state.importError.code }),
      el('p', { text: state.importError.message }),
      el('p', { class: 'tiny muted', text: 'The file is still loaded. Fix the mapping above and import again.' }),
    ]));
  }

  const go = el('button', { class: 'primary', type: 'button', text: 'Import', id: 'run-import' });
  go.addEventListener('click', () => runImport(receipt));
  const review = el('label', { class: 'field-row', id: 'mapping-review' });
  const tick = el('input', { type: 'checkbox', id: 'mapping-reviewed' });
  tick.checked = state.mappingReviewed;
  tick.addEventListener('change', () => { state.mappingReviewed = tick.checked; syncImportButton(); });
  review.appendChild(tick);
  review.appendChild(el('span', { text: 'I have checked the suggested rows against my file.' }));
  card.appendChild(el('div', { class: 'row' }, [
    review,
    go,
    el('button', { type: 'button', text: 'Cancel', onclick: () => { forgetStagedUpload(); state.upload = null; state.mapping = null; renderData(); renderNext(); } }),
  ]));
  setTimeout(syncImportButton, 0);
  return card;
}

/** While any suggestion stands unreviewed, the import waits for the tick. */
function syncImportButton() {
  const go = document.getElementById('run-import');
  const review = document.getElementById('mapping-review');
  if (!go || !review) return;
  const standing = state.mapping.some((entry) => entry.why);
  review.classList.toggle('hidden', !standing);
  go.disabled = standing && !state.mappingReviewed;
}

async function suggestFromHeaders(receipt) {
  const found = await guard(() => api('/api/upload/suggest', {
    method: 'POST',
    body: { upload_id: receipt.upload_id },
  }));
  if (!found) return;
  const byHeader = new Map(found.columns.map((column) => [column.header, column]));
  receipt.headers.forEach((header, index) => {
    const column = byHeader.get(header);
    const entry = state.mapping[index];
    if (!column) return;
    entry.role = column.role === 'timestamp' ? '__timestamp' : column.role;
    entry.unit = column.unit ?? '';
    entry.why = column.reason;
    entry.check = column.check;
  });
  state.mappingReviewed = false;
  state.mappingNotes = found.notes;
  if (found.interval_minutes) {
    state.importSettings.interval = String(found.interval_minutes);
    state.importSettings.intervalNote = `The first two timestamps are ${found.interval_minutes} minutes apart.`;
  }
  renderData();
  const applied = found.columns.length;
  toast(`Suggested ${applied} of ${receipt.headers.length} columns. Review each one, then confirm.`, null, true);
}

async function runImport(receipt) {
  const series = {};
  const ignore = [];
  const unresolved = [];
  const missingUnits = [];
  const repeated = [];
  let timestamp = null;
  let demandPeriod = null;

  receipt.headers.forEach((header, index) => {
    const role = document.getElementById(`role-${index}`).value;
    const unit = document.getElementById(`unit-${index}`).value;
    if (role === '__unmapped') unresolved.push(header);
    else if (role === '__timestamp') {
      if (timestamp) repeated.push(`${timestamp} and ${header} are both the timestamp`);
      timestamp = header;
    } else if (role === '__ignore') ignore.push(header);
    else if (role === 'demand_period') {
      if (demandPeriod) repeated.push(`${demandPeriod} and ${header} are both the period label`);
      demandPeriod = header;
    } else {
      if (!unit) missingUnits.push(header);
      if (series[role]) repeated.push(`${series[role].header} and ${header} both mean ${role.replace(/_/g, ' ')}`);
      series[role] = { header, unit };
    }
  });

  if (unresolved.length) {
    toast(`Choose a meaning or Ignore for: ${unresolved.join(', ')}`, 'ui.unmapped_columns');
    return;
  }
  if (repeated.length) {
    toast(`One column per meaning: ${repeated.join('; ')}.`, 'ui.repeated_meaning');
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

  if (!(await settleUnsaved('import this file'))) return;

  const into = state.project.name;
  let reply;
  try {
    reply = await request(projectPath('/import'), {
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
    });
  } catch (error) {
    if (state.project?.name !== into) {
      toast(`The import into "${into}" failed: ${error.message}`, error.code);
      return;
    }
    // An import error names the row and column to fix, so it stays on screen.
    state.importError = { code: error.code, message: error.message };
    renderData();
    return;
  }
  // The file went into the project it was meant for, whichever one is on screen now.
  if (state.project?.name !== into) {
    toast(`Finished importing into "${into}".`, null, true);
    await refreshProjects();
    return;
  }

  const result = reply.body;
  state.project = result.project;
  state.revision = reply.revision;
  state.changedElsewhere = false;
  state.upload = null;
  state.mapping = null;
  state.importError = null;
  dropDerivedResults();
  markSaved();
  const profile = await guard(() => api(projectPath('/profile')));
  if (state.project?.name !== into) return;
  state.profile = profile;
  state.justImported = result.import;
  renderProjectSummary(state.project);
  await refreshProjects();
  show('data');
}

// ---------------------------------------------------------------------------
// 2 — Scenarios
// ---------------------------------------------------------------------------

const POLICY_NAMES = {
  self_consumption: 'Self-consumption',
  peak_limit: 'Peak limit',
  reserve_first: 'Reserve first',
  manual_schedule: 'Manual schedule',
};

function selectedScenario() {
  const project = state.project;
  return project.scenarios.find((s) => s.id === state.selected) ?? project.scenarios[0];
}

function renderScenarios() {
  const root = document.getElementById('view-scenarios');
  clear(root);
  const project = state.project;
  const max = state.session.limits.max_scenarios;
  const scenario = selectedScenario();
  state.selected = scenario.id;

  root.appendChild(el('div', {}, [
    el('h3', { text: 'Scenarios' }),
    el('p', { class: 'lede', text:
      'Each scenario is complete in itself: what you read here is exactly what the engine uses. Dispatch ' +
      'policies are named rules evaluated interval by interval with no view of the future. Tonans does not ' +
      'optimise and never claims an optimal result.' }),
  ]));

  if (project.scenarios.length > 1) root.appendChild(assumptionDiff(project));

  const feeders = el('nav', { class: 'feeders', 'aria-label': 'Scenarios' });
  const list = el('ul', { class: 'feeder-list' });
  for (const entry of project.scenarios) {
    list.appendChild(el('li', {}, el('button', {
      type: 'button',
      class: 'feeder',
      'aria-current': entry.id === scenario.id ? 'true' : 'false',
      onclick: () => { state.selected = entry.id; renderScenarios(); },
    }, [
      el('span', { class: 'feeder-name', dataset: { scenario: entry.id }, text: entry.name }),
      el('span', { class: 'feeder-meta', text: feederNote(entry, project) }),
    ])));
  }
  feeders.appendChild(list);
  const add = el('button', { type: 'button', class: project.scenarios.length < 2 ? 'primary' : '',
                             text: 'Add a scenario', onclick: addScenario });
  add.disabled = project.scenarios.length >= max;
  feeders.appendChild(el('div', { class: 'row' }, add));
  if (project.scenarios.length >= max) {
    feeders.appendChild(el('p', { class: 'tiny muted', text: `A project holds up to ${max} scenarios.` }));
  }

  root.appendChild(el('div', { class: 'scenario-layout' }, [feeders, scenarioEditor(scenario)]));
}

function feederNote(scenario, project) {
  const parts = [];
  if (scenario.id === project.baseline_scenario_id) parts.push('Baseline');
  parts.push(scenario.battery
    ? `${formatAxis(scenario.battery.nameplate_energy_kwh)} kWh, ${formatAxis(scenario.battery.max_discharge_kw)} kW`
    : 'No battery');
  parts.push(POLICY_NAMES[scenario.dispatch.policy]);
  return parts.join(', ');
}

/** Every assumption, side by side, with what differs from the baseline marked. */
function assumptionDiff(project) {
  const dash = '—';
  const price = (source, series) => (source.source === 'flat' ? `${source.price_per_kwh_usd} USD/kWh` : series);
  const rows = [
    ['Battery energy', (s) => (s.battery ? `${s.battery.nameplate_energy_kwh} kWh` : dash)],
    ['Battery power', (s) => (s.battery ? `${s.battery.max_discharge_kw} kW` : dash)],
    ['Reserve floor', (s) => (s.battery ? `${s.battery.reserve_soc_pct} %` : dash)],
    ['Round trip', (s) => (s.battery
      ? `${(s.battery.charge_efficiency_pct * s.battery.discharge_efficiency_pct / 100).toFixed(1)} %` : dash)],
    ['Temperature derating', (s) => (s.battery?.temperature_derate ? `${s.battery.temperature_derate.points.length} points` : dash)],
    ['Dispatch policy', (s) => POLICY_NAMES[s.dispatch.policy]],
    ['Import ceiling', (s) => (s.dispatch.policy === 'peak_limit' ? `${s.dispatch.import_limit_kw} kW` : dash)],
    ['Reserve target', (s) => (s.dispatch.policy === 'reserve_first' ? `${s.dispatch.reserve_target_soc_pct} %` : dash)],
    ['Grid charging', (s) => (s.dispatch.allow_grid_charge === undefined ? dash : (s.dispatch.allow_grid_charge ? 'Allowed' : 'Not allowed'))],
    ['Import price', (s) => price(s.tariff.import, 'From data')],
    ['Export credit', (s) => price(s.tariff.export, 'From data')],
    ['Import price multiplier', (s) => `${s.tariff.import_price_multiplier} x`],
    ['Demand charge', (s) => {
      const entries = Object.entries(s.tariff.demand_prices_per_kw_month_usd ?? {});
      return entries.length ? entries.map(([k, v]) => `${k} ${v}`).join(', ') + ' USD/kW-month' : 'None';
    }],
    ['Fixed charge', (s) => `${s.tariff.fixed_charge_usd_per_month} USD/month`],
    ['Site export limit', (s) => (s.inverter.site_export_limit_kw ?? null) === null ? 'Unlimited' : `${s.inverter.site_export_limit_kw} kW`],
    ['Installed cost', (s) => fmt.usd(s.economics.capex_usd)],
    ['Operating cost', (s) => `${fmt.usd(s.economics.annual_opex_usd)}/year`],
    ['Replacement', (s) => (s.economics.replacement
      ? `${fmt.usd(s.economics.replacement.cost_usd)} at ${s.economics.replacement.trigger_capacity_pct} %` : 'None')],
    ['Horizon and discount rate', (s) => `${s.economics.horizon_years} years, ${s.economics.discount_rate_pct_per_year} %`],
    ['Energy escalation', (s) => `${s.economics.energy_price_escalation_pct_per_year} %/year`],
    ['Calendar fade', (s) => `${s.degradation.calendar_fade_pct_per_year} %/year`],
    ['Cycle fade', (s) => `${s.degradation.cycle_fade_pct_per_cycle} %/cycle`],
    ['Outage windows', (s) => String(s.outage.windows?.length ?? 0)],
  ];
  const baseline = project.scenarios.find((s) => s.id === project.baseline_scenario_id) ?? project.scenarios[0];

  const head = el('tr', {}, [el('th', { text: 'Assumption' }), ...project.scenarios.map((s) =>
    el('th', { class: 'num' }, el('button', { type: 'button', text: s.name, onclick: () => {
      state.selected = s.id;
      renderScenarios();
    } })))]);
  const body = el('tbody');
  for (const [label, read] of rows) {
    const reference = read(baseline);
    body.appendChild(el('tr', {}, [
      el('th', { scope: 'row', text: label }),
      ...project.scenarios.map((s) => {
        const value = read(s);
        return el('td', { class: `num${value !== reference ? ' differs' : ''}`, text: value });
      }),
    ]));
  }
  const details = el('details', { class: 'section', open: state.diffOpen ? 'open' : null }, [
    el('summary', {}, [
      el('span', { class: 'section-name', text: 'Compare assumptions' }),
      el('span', { class: 'section-summary', text: 'Marked values differ from the baseline.' }),
    ]),
    el('div', { class: 'scroll' }, el('table', { class: 'diff' }, [el('thead', {}, head), body])),
  ]);
  details.addEventListener('toggle', () => { state.diffOpen = details.open; });
  return details;
}

// -- Editing -----------------------------------------------------------------

/** A number box that refuses to turn a blank or unreadable entry into a number. */
function numberField(obj, key, editKey, step = 'any') {
  const input = el('input', { type: 'number', step, value: String(obj[key] ?? 0) });
  input.addEventListener('input', () => {
    const raw = input.value.trim();
    if (raw === '' || !Number.isFinite(Number(raw))) {
      input.setAttribute('aria-invalid', 'true');
      input.title = 'Enter a number. A blank is not saved as 0.';
      return;
    }
    input.removeAttribute('aria-invalid');
    input.removeAttribute('title');
    recordEdit(editKey, 'edit a value', () => { obj[key] = Number(raw); });
  });
  return input;
}

function textField(obj, key, editKey, afterEdit) {
  const input = el('input', { type: 'text', value: String(obj[key] ?? '') });
  input.addEventListener('input', () => {
    recordEdit(editKey, 'edit a value', () => { obj[key] = input.value; if (afterEdit) afterEdit(input.value); });
  });
  return input;
}

function section(id, name, summary, body) {
  const details = el('details', { class: 'section', open: state.openSections.has(id) ? 'open' : null }, [
    el('summary', {}, [
      el('span', { class: 'section-name', text: name }),
      el('span', { class: 'section-summary', text: summary }),
    ]),
    el('div', { class: 'section-body' }, body),
  ]);
  details.addEventListener('toggle', () => {
    if (details.open) state.openSections.add(id); else state.openSections.delete(id);
  });
  return details;
}

function scenarioEditor(scenario) {
  const project = state.project;
  const isBaseline = scenario.id === project.baseline_scenario_id;
  const box = el('article', { class: 'stack' });
  const prefix = `${scenario.id}.`;

  const title = el('h3', { text: scenario.name });
  const actions = el('div', { class: 'row' }, [
    isBaseline ? el('span', { class: 'tag', text: 'Baseline' })
      : el('button', { type: 'button', text: 'Make this the baseline', onclick: () => {
        change('change the baseline', () => { project.baseline_scenario_id = scenario.id; });
      } }),
    el('button', { type: 'button', text: 'Duplicate', title: 'Ctrl+D', onclick: duplicateSelected }),
    project.scenarios.length > 1 ? el('button', { type: 'button', class: 'danger', text: 'Remove', onclick: () => removeScenario(scenario) }) : null,
  ]);
  box.appendChild(el('div', { class: 'editor-head' }, [
    el('div', {}, [title, isBaseline
      ? el('p', { class: 'tiny muted', text: 'Every saving, delta, and NPV in this project is measured against this scenario.' })
      : null]),
    actions,
  ]));

  box.appendChild(el('div', { class: 'grid' }, [
    field('Name', textField(scenario, 'name', `${prefix}name`, (value) => {
      title.textContent = value;
      const label = document.querySelector(`.feeder-name[data-scenario="${CSS.escape(scenario.id)}"]`);
      if (label) label.textContent = value;
    })),
    field('Id, used in export file names', textField(scenario, 'id', `${prefix}id`, (value) => {
      // The baseline is named by id. Keep the reference with it.
      if (project.baseline_scenario_id === state.selected) project.baseline_scenario_id = value;
      state.selected = value;
    })),
  ]));

  box.appendChild(batterySection(scenario, prefix));
  box.appendChild(dispatchSection(scenario, prefix));
  box.appendChild(tariffSection(scenario, prefix));
  box.appendChild(economicsSection(scenario, prefix));
  box.appendChild(degradationSection(scenario, prefix));
  box.appendChild(outageSection(scenario, prefix));
  return box;
}

function batterySection(scenario, prefix) {
  const b = scenario.battery;
  const body = [];
  const toggle = el('input', { type: 'checkbox' });
  toggle.checked = Boolean(b);
  toggle.addEventListener('change', () => {
    change(toggle.checked ? 'add a battery' : 'remove the battery', () => {
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
        // The other policies need a battery, and the engine refuses them without one.
        scenario.dispatch = { policy: 'self_consumption' };
      }
    });
  });
  body.push(el('label', { class: 'field-row' }, [toggle, el('span', { text: 'This scenario has a battery' })]));

  if (b) {
    const num = (obj, key) => numberField(obj, key, `${prefix}battery.${key}`);
    body.push(el('div', { class: 'grid' }, [
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
    body.push(el('p', { class: 'tiny muted', text:
      `Round trip: ${(b.charge_efficiency_pct * b.discharge_efficiency_pct / 100).toFixed(1)} %. All conversion ` +
      'loss lives in these two efficiencies. The inverter contributes a power ceiling and no loss of its own, ' +
      'so the two are never double-counted.' }));
    body.push(deratingEditor(scenario, b, prefix));
  }
  const summary = b
    ? `${formatAxis(b.nameplate_energy_kwh)} kWh, ${formatAxis(b.max_discharge_kw)} kW, reserve ${b.reserve_soc_pct} %`
    : 'None';
  return section('battery', 'Battery', summary, body);
}

/** The derating table the model uses. It was invisible before, and it changes results. */
function deratingEditor(scenario, battery, prefix) {
  const box = el('div', {});
  const derate = battery.temperature_derate;
  box.appendChild(el('h4', { text: 'Temperature derating' }));
  if (!derate) {
    box.appendChild(el('p', { class: 'tiny muted', text: 'None. The battery performs the same at every temperature.' }));
    const hasTemperature = Boolean(state.project.dataset?.series?.temperature);
    const add = el('button', { type: 'button', text: 'Add a derating table', onclick: () => {
      change('add a derating table', () => {
        battery.temperature_derate = { points: [
          { temperature_c: 0, power_limit_pct: 100, usable_capacity_pct: 100 },
          { temperature_c: 25, power_limit_pct: 100, usable_capacity_pct: 100 },
        ] };
      });
    } });
    add.disabled = !hasTemperature;
    if (!hasTemperature) add.title = 'Import a temperature column first.';
    box.appendChild(add);
    return box;
  }
  const body = el('tbody');
  derate.points.forEach((point, index) => {
    const num = (key) => numberField(point, key, `${prefix}derate.${index}.${key}`);
    body.appendChild(el('tr', { class: 'schedule' }, [
      el('td', {}, num('temperature_c')),
      el('td', {}, num('power_limit_pct')),
      el('td', {}, num('usable_capacity_pct')),
      el('td', {}, derate.points.length > 1 ? el('button', { type: 'button', class: 'quiet', text: 'Remove', onclick: () => {
        change('remove a derating point', () => { derate.points.splice(index, 1); });
      } }) : null),
    ]));
  });
  box.appendChild(el('div', { class: 'scroll' }, el('table', {}, [
    el('thead', {}, el('tr', {}, [
      el('th', { text: 'Ambient (°C)' }), el('th', { text: 'Power limit (%)' }),
      el('th', { text: 'Usable capacity (%)' }), el('th', {}),
    ])),
    body,
  ])));
  box.appendChild(el('div', { class: 'row' }, [
    el('button', { type: 'button', text: 'Add a point', onclick: () => {
      change('add a derating point', () => {
        const last = derate.points[derate.points.length - 1];
        derate.points.push({ ...last, temperature_c: last.temperature_c + 10 });
      });
    } }),
    el('button', { type: 'button', text: 'Remove derating', onclick: () => {
      change('remove derating', () => { delete battery.temperature_derate; });
    } }),
  ]));
  box.appendChild(el('p', { class: 'tiny muted', text: 'Points must rise in temperature. Between points Tonans interpolates linearly.' }));
  return box;
}

function suggestedCeiling() {
  const peak = state.profile?.peak_net_load_kw;
  return peak ? niceNumber(peak * 0.85) : 90;
}

function dispatchSection(scenario, prefix) {
  const body = [];
  const options = scenario.battery
    ? Object.entries(POLICY_NAMES)
    : [['self_consumption', POLICY_NAMES.self_consumption]];
  const policy = select(`policy-${scenario.id}`, options, scenario.dispatch.policy);
  policy.addEventListener('change', () => {
    change('change the dispatch policy', () => {
      const chosen = policy.value;
      if (chosen === 'self_consumption') scenario.dispatch = { policy: chosen };
      else if (chosen === 'peak_limit') {
        scenario.dispatch = { policy: chosen, import_limit_kw: suggestedCeiling(), allow_grid_charge: false,
                              grid_charge_target_soc_pct: scenario.battery?.soc_max_pct ?? 100 };
      } else if (chosen === 'reserve_first') {
        scenario.dispatch = { policy: chosen,
                              reserve_target_soc_pct: scenario.battery?.reserve_soc_pct ?? 25,
                              allow_grid_charge: false };
      } else scenario.dispatch = { policy: chosen, entries: [] };
    });
  });
  body.push(el('div', { class: 'row' }, policy));
  if (!scenario.battery) {
    body.push(el('p', { class: 'tiny muted', text: 'The other policies need a battery.' }));
  }

  const d = scenario.dispatch;
  const explain = {
    self_consumption: 'Stores surplus generation and discharges to cover net load. Never charges from ' +
      'the grid; never exports from the battery.',
    peak_limit: 'Discharges only as much as it takes to hold grid import at or below the ceiling, and ' +
      'charges from surplus generation. It does not otherwise self-consume: holding the peak down is ' +
      'the whole job.',
    reserve_first: 'Below the target the battery only ever charges: refilling the outage reserve outranks ' +
      'every saving. At or above it, it behaves as self-consumption and will not fall back below the target ' +
      'while the grid is up.',
    manual_schedule: 'Follows your setpoints exactly, and is idle outside them. Setpoints are clipped to ' +
      'what the hardware can do, and any clipping is reported as a warning: the schedule you wrote is not ' +
      'necessarily the schedule that ran.',
  }[d.policy];
  body.push(el('p', { class: 'tiny muted', text: explain }));

  const num = (key) => numberField(d, key, `${prefix}dispatch.${key}`);
  const allow = () => {
    const input = el('input', { type: 'checkbox' });
    input.checked = Boolean(d.allow_grid_charge);
    input.addEventListener('change', () => {
      recordEdit(`${prefix}dispatch.grid`, 'edit a value', () => { d.allow_grid_charge = input.checked; });
    });
    return el('label', { class: 'field-row' }, [input, el('span', { text: 'May charge from the grid' })]);
  };
  if (d.policy === 'peak_limit') {
    body.push(el('div', { class: 'grid' }, [
      field('Import ceiling (kW)', num('import_limit_kw')),
      field('Grid charge target (% state of charge)', num('grid_charge_target_soc_pct')),
      el('div', { class: 'field' }, allow()),
    ]));
  } else if (d.policy === 'reserve_first') {
    body.push(el('div', { class: 'grid' }, [
      field('Reserve target (% state of charge)', num('reserve_target_soc_pct')),
      el('div', { class: 'field' }, allow()),
    ]));
  } else if (d.policy === 'manual_schedule') {
    body.push(scheduleEditor(d, prefix));
  }

  const summary = POLICY_NAMES[d.policy] +
    (d.policy === 'peak_limit' ? `, ceiling ${d.import_limit_kw} kW` : '') +
    (d.policy === 'reserve_first' ? `, target ${d.reserve_target_soc_pct} %` : '');
  return section('dispatch', 'Dispatch policy', summary, body);
}

/** Copy one part of this scenario onto every other, so a comparison stays like for like. */
function applyToAll(label, copy) {
  return el('button', { type: 'button', class: 'quiet', text: `Use this ${label} in every scenario`, onclick: () => {
    change(`use this ${label} in every scenario`, () => {
      const source = selectedScenario();
      for (const other of state.project.scenarios) {
        if (other !== source) copy(source, other);
      }
    });
    toast(`Every scenario now has this ${label}.`, null, true, { label: 'Undo', run: undo });
  } });
}

function tariffSection(scenario, prefix) {
  const t = scenario.tariff;
  const body = [];
  const dataset = state.project.dataset;
  const hasPrices = Boolean(dataset?.series?.import_price && dataset?.series?.export_price);
  const periods = dataset?.demand_periods?.labels ?? ['all'];

  body.push(el('div', { class: 'row' }, [
    ...TARIFF_PRESETS.map((preset) => {
      const button = el('button', { type: 'button', text: preset.name, onclick: () => {
        change(`apply the "${preset.name}" tariff`, () => preset.apply(t, periods));
      } });
      if (preset.needsSeries && !hasPrices) {
        button.disabled = true;
        button.title = 'Import price columns first.';
      }
      return button;
    }),
  ]));
  body.push(el('p', { class: 'tiny muted', text:
    'Presets fill in a tariff\'s shape with round example rates. They are not any utility\'s tariff: replace the rates with yours.' }));

  const importSource = select(`imp-src-${scenario.id}`, [
    ['series', 'Per-interval price from the imported data'],
    ['flat', 'One flat price'],
  ], t.import.source);
  importSource.addEventListener('change', () => {
    change('change the import price source', () => {
      t.import = importSource.value === 'flat' ? { source: 'flat', price_per_kwh_usd: 0.15 } : { source: 'series' };
    });
  });
  const exportSource = select(`exp-src-${scenario.id}`, [
    ['series', 'Per-interval credit from the imported data'],
    ['flat', 'One flat credit'],
  ], t.export.source);
  exportSource.addEventListener('change', () => {
    change('change the export credit source', () => {
      t.export = exportSource.value === 'flat' ? { source: 'flat', price_per_kwh_usd: 0.05 } : { source: 'series' };
    });
  });
  const num = (obj, key) => numberField(obj, key, `${prefix}tariff.${key}`);

  body.push(el('div', { class: 'grid' }, [
    field('Import price source', importSource),
    t.import.source === 'flat' ? field('Import price (USD/kWh)', num(t.import, 'price_per_kwh_usd')) : null,
    field('Export credit source', exportSource),
    t.export.source === 'flat' ? field('Export credit (USD/kWh)', numberField(t.export, 'price_per_kwh_usd', `${prefix}tariff.export`)) : null,
    field('Import price multiplier (x)', num(t, 'import_price_multiplier')),
    field('Export credit multiplier (x)', num(t, 'export_price_multiplier')),
    field('Fixed charge (USD per month)', num(t, 'fixed_charge_usd_per_month')),
    field('Site export limit (kW, blank = unlimited)', exportLimit(scenario, prefix)),
  ].filter(Boolean)));

  const demand = t.demand_prices_per_kw_month_usd || {};
  body.push(el('h4', { text: 'Demand charge (USD per kW of monthly peak)' }));
  body.push(el('div', { class: 'grid' }, periods.map((period) => {
    const input = el('input', { type: 'number', step: 'any', value: String(demand[period] ?? 0) });
    input.addEventListener('input', () => {
      if (input.value.trim() === '') { input.setAttribute('aria-invalid', 'true'); return; }
      input.removeAttribute('aria-invalid');
      recordEdit(`${prefix}demand.${period}`, 'edit a value', () => {
        const prices = t.demand_prices_per_kw_month_usd || {};
        // A price shown as 0 must be a price of 0: the engine refuses a tariff that
        // prices one period and leaves another out.
        for (const label of periods) prices[label] = prices[label] ?? 0;
        prices[period] = Number(input.value);
        t.demand_prices_per_kw_month_usd = prices;
      });
    });
    return field(`${period} (USD/kW-month)`, input);
  })));
  body.push(el('div', { class: 'row' }, applyToAll('tariff', (from, to) => {
    to.tariff = structuredClone(from.tariff);
  })));

  const summary = [
    t.import.source === 'flat' ? `${t.import.price_per_kwh_usd} USD/kWh flat` : 'Prices from data',
    Object.keys(demand).length ? 'demand charge' : 'no demand charge',
  ].join(', ');
  return section('tariff', 'Tariff', summary, body);
}

function exportLimit(scenario, prefix) {
  const input = el('input', {
    type: 'number', step: 'any',
    value: scenario.inverter.site_export_limit_kw === null || scenario.inverter.site_export_limit_kw === undefined
      ? '' : String(scenario.inverter.site_export_limit_kw),
  });
  input.addEventListener('input', () => {
    // `1e` reaches this as a blank value, and only badInput tells it from a real blank.
    const reading = readOptionalNumber(input.value, input.validity.badInput);
    if (!reading.ok) {
      input.setAttribute('aria-invalid', 'true');
      input.title = 'Enter a number, or leave it blank for no limit.';
      return;
    }
    input.removeAttribute('aria-invalid');
    input.removeAttribute('title');
    recordEdit(`${prefix}export-limit`, 'edit a value', () => {
      scenario.inverter.site_export_limit_kw = reading.value;
    });
  });
  return input;
}

function economicsSection(scenario, prefix) {
  const k = scenario.economics;
  const num = (obj, key, step) => numberField(obj, key, `${prefix}economics.${key}`, step);
  const body = [
    el('div', { class: 'grid' }, [
      field('Installed cost (USD)', num(k, 'capex_usd')),
      field('Operating cost (USD per year)', num(k, 'annual_opex_usd')),
      field('Horizon (years)', num(k, 'horizon_years', '1')),
      field('Discount rate (% per year)', num(k, 'discount_rate_pct_per_year')),
      field('Energy escalation (% per year)', num(k, 'energy_price_escalation_pct_per_year')),
    ]),
  ];

  const replacing = el('input', { type: 'checkbox' });
  replacing.checked = Boolean(k.replacement);
  replacing.addEventListener('change', () => {
    change(replacing.checked ? 'add a replacement' : 'remove the replacement', () => {
      k.replacement = replacing.checked ? { cost_usd: Math.round(k.capex_usd * 0.5), trigger_capacity_pct: 70 } : null;
    });
  });
  body.push(el('label', { class: 'field-row' }, [replacing, el('span', { text: 'Replace the battery when it fades' })]));
  if (k.replacement) {
    body.push(el('div', { class: 'grid' }, [
      field('Replacement cost (USD)', numberField(k.replacement, 'cost_usd', `${prefix}replacement.cost`)),
      field('Replace at capacity (% of nameplate)', numberField(k.replacement, 'trigger_capacity_pct', `${prefix}replacement.trigger`)),
    ]));
  }
  body.push(el('div', { class: 'row' }, applyToAll('horizon, discount rate, and escalation', (from, to) => {
    to.economics.horizon_years = from.economics.horizon_years;
    to.economics.discount_rate_pct_per_year = from.economics.discount_rate_pct_per_year;
    to.economics.energy_price_escalation_pct_per_year = from.economics.energy_price_escalation_pct_per_year;
  })));
  const summary = `${fmt.usd(k.capex_usd)} installed, ${k.horizon_years} years at ${k.discount_rate_pct_per_year} %`;
  return section('economics', 'Economics', summary, body);
}

function degradationSection(scenario, prefix) {
  const g = scenario.degradation;
  const num = (key) => numberField(g, key, `${prefix}degradation.${key}`);
  const scaling = select(`scaling-${scenario.id}`, [
    ['proportional_to_usable_capacity', 'In proportion to usable capacity'],
    ['constant', 'Constant: year one\'s benefit every year'],
  ], g.benefit_scaling);
  scaling.addEventListener('change', () => {
    recordEdit(`${prefix}degradation.scaling`, 'edit a value', () => { g.benefit_scaling = scaling.value; });
  });
  return section('degradation', 'Degradation',
    `${g.calendar_fade_pct_per_year} %/year calendar, ${g.cycle_fade_pct_per_cycle} %/cycle`, [
      el('p', { class: 'tiny muted', text:
        'A transparent approximation, not a lifetime model: a linear fade in two terms that knows nothing about ' +
        'depth of discharge, C-rate, or thermal history. Where a vendor gives a warranted capacity curve, use that instead.' }),
      el('div', { class: 'grid' }, [
        field('Calendar fade (% per year)', num('calendar_fade_pct_per_year')),
        field('Cycle fade (% per equivalent full cycle)', num('cycle_fade_pct_per_cycle')),
        field('Throughput cost (USD per kWh discharged)', num('throughput_cost_per_kwh_throughput_usd')),
        field('Later years\' benefit', scaling),
      ]),
    ]);
}

/** An instant, edited in UTC, with the site's own clock beside it. */
function utcField(entry, key, editKey) {
  const readout = el('div', { class: 'tiny muted', text: entry[key] ? `${fmt.when(entry[key])} at the site` : '' });
  const input = el('input', { type: 'datetime-local', value: (entry[key] || '').slice(0, 16) });
  input.addEventListener('input', () => {
    if (!input.value) return;
    recordEdit(editKey, 'edit a time', () => {
      entry[key] = new Date(`${input.value}Z`).toISOString();
      readout.textContent = `${fmt.when(entry[key])} at the site`;
    });
  });
  return el('div', {}, [input, readout]);
}

function scheduleEditor(dispatch, prefix) {
  const box = el('div', {});
  const body = el('tbody');
  (dispatch.entries ?? []).forEach((entry, i) => {
    body.appendChild(el('tr', { class: 'schedule' }, [
      el('td', {}, utcField(entry, 'start_utc', `${prefix}schedule.${i}.start`)),
      el('td', {}, utcField(entry, 'end_utc', `${prefix}schedule.${i}.end`)),
      el('td', {}, numberField(entry, 'battery_power_kw', `${prefix}schedule.${i}.kw`)),
      el('td', {}, el('button', { type: 'button', class: 'quiet', text: 'Remove', onclick: () => {
        change('remove a setpoint window', () => { dispatch.entries.splice(i, 1); });
      } })),
    ]));
  });
  box.appendChild(el('div', { class: 'scroll' }, el('table', {}, [
    el('thead', {}, el('tr', {}, [
      el('th', { text: 'From (UTC)' }), el('th', { text: 'To (UTC)' }),
      el('th', { text: 'Battery power (kW, + discharges)' }), el('th', {}),
    ])),
    body,
  ])));
  box.appendChild(el('div', { class: 'row' }, el('button', { type: 'button', text: 'Add a setpoint window', onclick: () => {
    change('add a setpoint window', () => {
      const start = state.project.dataset?.start_utc || new Date().toISOString();
      dispatch.entries = [...(dispatch.entries ?? []), { start_utc: start, end_utc: start, battery_power_kw: 0 }];
    });
  } })));
  box.appendChild(el('p', { class: 'tiny muted', text:
    'Windows must start and end exactly on an interval boundary inside the imported data. Tonans will ' +
    'not round a window to fit: that would be a silent repair.' }));
  return box;
}

function outageSection(scenario, prefix) {
  // Drawing a form must not change what it draws: a list the server left out stays out,
  // or a project just saved would read as unsaved as soon as the page redraws.
  const windows = scenario.outage.windows ?? [];
  const body = el('tbody');
  windows.forEach((window, i) => {
    const label = textField(window, 'label', `${prefix}outage.${i}.label`);
    body.appendChild(el('tr', { class: 'schedule' }, [
      el('td', {}, utcField(window, 'start_utc', `${prefix}outage.${i}.start`)),
      el('td', {}, utcField(window, 'end_utc', `${prefix}outage.${i}.end`)),
      el('td', {}, label),
      el('td', {}, el('button', { type: 'button', class: 'quiet', text: 'Remove', onclick: () => {
        change('remove an outage window', () => { windows.splice(i, 1); });
      } })),
    ]));
  });
  return section('outage', 'Outages to replay',
    windows.length ? `${windows.length} window${windows.length === 1 ? '' : 's'}` : 'None', [
      el('div', { class: 'scroll' }, el('table', {}, [
        el('thead', {}, el('tr', {}, [
          el('th', { text: 'From (UTC)' }), el('th', { text: 'To (UTC)' }), el('th', { text: 'Label' }), el('th', {}),
        ])),
        body,
      ])),
      el('div', { class: 'row' }, [
        el('button', { type: 'button', text: 'Add an outage window', onclick: () => {
          change('add an outage window', () => {
            const start = state.project.dataset?.start_utc || new Date().toISOString();
            scenario.outage.windows = [...windows, { start_utc: start, end_utc: start, label: '' }];
          });
        } }),
        applyToAll('outage list', (from, to) => { to.outage = structuredClone(from.outage); }),
      ]),
      el('p', { class: 'tiny muted', text:
        'During an outage Tonans assumes non-critical load is shed by a transfer switch, and lets the battery ' +
        'discharge below its reserve floor down to its hard minimum: the reserve exists for exactly this moment.' }),
    ]);
}

// -- Adding ------------------------------------------------------------------

function uniqueId(base) {
  const taken = new Set(state.project.scenarios.map((s) => s.id));
  const slug = (base.toLowerCase().replace(/[^a-z0-9]+/g, '-').slice(0, 44).replace(/^-+|-+$/g, '')) || 'scenario';
  if (!taken.has(slug)) return slug;
  for (let n = 2; ; n += 1) {
    const candidate = `${slug}-${n}`;
    if (!taken.has(candidate)) return candidate;
  }
}

function uniqueName(base) {
  const taken = new Set(state.project.scenarios.map((s) => s.name));
  const trimmed = base.slice(0, 56);
  if (!taken.has(trimmed)) return trimmed;
  for (let n = 2; ; n += 1) {
    const candidate = `${trimmed} (${n})`;
    if (!taken.has(candidate)) return candidate;
  }
}

function addCopy(source, name) {
  const copy = structuredClone(source);
  copy.name = uniqueName(name);
  copy.id = uniqueId(copy.name);
  return copy;
}

function duplicateSelected() {
  const project = state.project;
  if (project.scenarios.length >= state.session.limits.max_scenarios) {
    toast(`A project holds up to ${state.session.limits.max_scenarios} scenarios.`, 'limit.scenarios');
    return;
  }
  const source = selectedScenario();
  change('duplicate a scenario', () => {
    const copy = addCopy(source, `${source.name} copy`);
    project.scenarios.splice(project.scenarios.indexOf(source) + 1, 0, copy);
    state.selected = copy.id;
  });
}

function removeScenario(scenario) {
  const project = state.project;
  change(`remove ${scenario.name}`, () => {
    const index = project.scenarios.indexOf(scenario);
    project.scenarios.splice(index, 1);
    if (project.baseline_scenario_id === scenario.id) project.baseline_scenario_id = project.scenarios[0].id;
    state.selected = project.scenarios[Math.max(index - 1, 0)].id;
  });
  toast(`Removed ${scenario.name}.`, null, true, { label: 'Undo', run: undo });
}

/** Round a starting value to something a person would say out loud. */
function niceNumber(value) {
  if (!(value > 0)) return 0;
  const unit = 10 ** (Math.floor(Math.log10(value)) - 1);
  return Number((Math.round(value / unit / 5) * 5 * unit).toPrecision(6));
}

/** Installed cost per kWh from a scenario in this project, if one states it. */
function costFromProject() {
  for (const scenario of state.project.scenarios) {
    if (scenario.battery && scenario.economics.capex_usd > 0) {
      return {
        rate: Math.round(scenario.economics.capex_usd / scenario.battery.nameplate_energy_kwh),
        from: scenario.name,
      };
    }
  }
  return null;
}

/**
 * Starting points, each built from this site's own data where it can be. None of them
 * is a recommendation: every value lands in the form, and the dialog says where it came
 * from. Tonans has no price data, so installed cost is always the user's to state.
 */
function scenarioTemplates() {
  const profile = state.profile;
  const peak = profile?.peak_net_load_kw ?? 100;
  const criticalMean = profile?.critical_load?.mean_kw ?? (profile ? profile.load.mean_kw * 0.25 : 25);
  const criticalPeak = profile?.critical_load?.peak_kw ?? criticalMean * 2;
  const basis = profile ? `your data's net peak of ${fmt.kw(peak)}` : 'a placeholder, since there is no data yet';

  const battery = (energy, power) => ({
    nameplate_energy_kwh: energy, max_charge_kw: power, max_discharge_kw: power,
    soc_min_pct: 10, soc_max_pct: 100, reserve_soc_pct: 25,
    charge_efficiency_pct: 95, discharge_efficiency_pct: 95, self_discharge_pct_per_month: 1.5,
  });

  return [
    {
      id: 'ladder', name: 'Battery sizes',
      blurb: 'Several sizes under one rule, to see where more storage stops helping.',
      params: [
        { key: 'power', label: 'Battery power (kW)', value: niceNumber(peak * 0.25), note: `A quarter of ${basis}.` },
        { key: 'hours', label: 'Hours of storage, comma-separated', value: '1, 2, 4', text: true },
      ],
      build(values, base, costPerKwh) {
        const hours = parseValues(values.hours);
        if (!hours.ok || !hours.values.length) throw new Error('Enter the hours of storage as numbers, separated by commas.');
        return hours.values.map((h) => {
          const energy = values.power * h;
          return { name: `${fmt.n(energy, 0)} kWh / ${fmt.n(values.power, 0)} kW, self-consumption`,
                   battery: battery(energy, values.power), capex: energy * costPerKwh,
                   dispatch: { policy: 'self_consumption' } };
        });
      },
    },
    {
      id: 'peak', name: 'Peak shaving',
      blurb: 'Holds grid import at a ceiling, to cut the demand charge.',
      params: [
        { key: 'ceiling', label: 'Import ceiling (kW)', value: niceNumber(peak * 0.85), note: `85 % of ${basis}.` },
        { key: 'power', label: 'Battery power (kW)', value: niceNumber(peak * 0.25), note: `A quarter of ${basis}.` },
        { key: 'hours', label: 'Hours of storage', value: 2 },
      ],
      build(values, base, costPerKwh) {
        const energy = values.power * values.hours;
        return [{ name: `${fmt.n(energy, 0)} kWh / ${fmt.n(values.power, 0)} kW, peak limit ${fmt.n(values.ceiling, 0)} kW`,
                  battery: battery(energy, values.power), capex: energy * costPerKwh,
                  dispatch: { policy: 'peak_limit', import_limit_kw: values.ceiling, allow_grid_charge: true, grid_charge_target_soc_pct: 90 } }];
      },
    },
    {
      id: 'backup', name: 'Backup first',
      blurb: 'Keeps the battery full for outages before it saves any money.',
      params: [
        { key: 'hours', label: 'Hours of critical load to carry', value: 8 },
        { key: 'critical', label: 'Critical load to carry (kW)', value: niceNumber(criticalMean),
          note: profile?.critical_load ? 'Your data\'s mean critical load.' : 'A quarter of your mean load: no critical-load column was imported.' },
        { key: 'power', label: 'Battery power (kW)', value: niceNumber(criticalPeak), note: 'Enough for the critical peak.' },
        { key: 'target', label: 'Reserve target (% state of charge)', value: 80 },
      ],
      build(values, base, costPerKwh) {
        // Usable window 10 to 100 % at 95 % discharge efficiency.
        const energy = niceNumber((values.critical * values.hours) / (0.9 * 0.95));
        return [{ name: `${fmt.n(energy, 0)} kWh / ${fmt.n(values.power, 0)} kW, backup first`,
                  battery: { ...battery(energy, values.power), reserve_soc_pct: Math.min(values.target, 80) },
                  capex: energy * costPerKwh,
                  dispatch: { policy: 'reserve_first', reserve_target_soc_pct: values.target, allow_grid_charge: true } }];
      },
    },
    {
      id: 'solar', name: 'Store surplus generation',
      blurb: 'Self-consumption at one size, to keep more of what the site generates.',
      params: [
        { key: 'power', label: 'Battery power (kW)', value: niceNumber((profile?.generation?.peak_kw ?? peak) * 0.25),
          note: profile?.generation ? 'A quarter of your generation peak.' : 'No generation was imported, so this policy has little to store.' },
        { key: 'hours', label: 'Hours of storage', value: 4 },
      ],
      build(values, base, costPerKwh) {
        const energy = values.power * values.hours;
        return [{ name: `${fmt.n(energy, 0)} kWh / ${fmt.n(values.power, 0)} kW, self-consumption`,
                  battery: battery(energy, values.power), capex: energy * costPerKwh,
                  dispatch: { policy: 'self_consumption' } }];
      },
    },
    {
      id: 'copy', name: 'Blank copy of the baseline',
      blurb: 'The baseline\'s assumptions, to change one thing by hand.',
      params: [],
      noCost: true,
      build() { return [{ name: 'New scenario', copyOnly: true }]; },
    },
  ];
}

function addScenario() {
  const project = state.project;
  if (project.scenarios.length >= state.session.limits.max_scenarios) {
    toast(`A project holds up to ${state.session.limits.max_scenarios} scenarios.`, 'limit.scenarios');
    return;
  }
  const templates = scenarioTemplates();
  const known = costFromProject();
  let current = templates[0];

  openDialog((dialog, close) => {
    const pane = el('div', { class: 'stack' });
    const picks = el('div', { class: 'template-list' });
    const failure = el('div', { class: 'banner critical hidden' });
    const cost = el('input', { type: 'number', step: 'any', min: '0', value: known ? String(known.rate) : '', id: 'template-cost' });

    let inputs = {};
    const drawPane = () => {
      clear(pane);
      inputs = {};
      pane.appendChild(el('h4', { text: current.name, style: { marginTop: '0' } }));
      pane.appendChild(el('p', { class: 'lede', text: current.blurb }));
      const fields = current.params.map((param) => {
        const input = param.text
          ? el('input', { type: 'text', value: param.value })
          : el('input', { type: 'number', step: 'any', value: String(param.value) });
        inputs[param.key] = { input, text: Boolean(param.text) };
        return el('div', { class: 'field' }, [
          el('label', { text: param.label }),
          input,
          param.note ? el('span', { class: 'field-note', text: param.note }) : null,
        ]);
      });
      if (!current.noCost) {
        fields.push(el('div', { class: 'field' }, [
          el('label', { text: 'Installed cost (USD per kWh of nameplate)' }),
          cost,
          el('span', { class: 'field-note', text: known
            ? `From ${known.from} in this project. Replace it with your quote.`
            : 'Tonans has no price data. Enter your own quote; 0 is allowed and will make NPV look better than it is.' }),
        ]));
      }
      if (fields.length) pane.appendChild(el('div', { class: 'grid' }, fields));
      pane.appendChild(el('p', { class: 'tiny muted', text: current.noCost
        ? 'Every assumption is copied from the baseline. It is a starting point, not a recommendation.'
        : 'Tariff, economics, degradation, and outages are copied from the baseline, so the comparison stays like ' +
          'for like. That includes any replacement cost, which does not move with the size here. Battery defaults ' +
          'are 95 % efficiency each way, a 10 to 100 % window, 1.5 % self-discharge a month, and a 50 % starting ' +
          'charge. All of it is a starting point, not a recommendation: review it on the scenario.' }));
      pane.appendChild(failure);
    };

    const drawPicks = () => {
      clear(picks);
      for (const template of templates) {
        picks.appendChild(el('button', {
          type: 'button',
          class: 'template-pick',
          'aria-current': template === current ? 'true' : 'false',
          onclick: () => { current = template; failure.classList.add('hidden'); drawPicks(); drawPane(); },
        }, [template.name, el('small', { text: template.blurb })]));
      }
    };

    const add = () => {
      const values = {};
      try {
        for (const [key, { input, text }] of Object.entries(inputs)) {
          if (text) { values[key] = input.value; continue; }
          const number = Number(input.value);
          if (input.value.trim() === '' || !Number.isFinite(number) || number <= 0) {
            throw new Error('Every value here must be a number above zero.');
          }
          values[key] = number;
        }
        let costPerKwh = 0;
        if (!current.noCost) {
          const raw = cost.value.trim();
          if (raw === '' || !Number.isFinite(Number(raw)) || Number(raw) < 0) {
            throw new Error('Enter an installed cost per kWh. Tonans has no price data to fill this in.');
          }
          costPerKwh = Number(raw);
        }
        const base = project.scenarios.find((s) => s.id === project.baseline_scenario_id) ?? project.scenarios[0];
        const builds = current.build(values, base, costPerKwh);
        if (project.scenarios.length + builds.length > state.session.limits.max_scenarios) {
          throw new Error(`That would take the project past ${state.session.limits.max_scenarios} scenarios.`);
        }
        change(`add ${current.name.toLowerCase()}`, () => {
          let last = null;
          for (const build of builds) {
            const scenario = addCopy(base, build.name);
            if (!build.copyOnly) {
              scenario.battery = build.battery;
              scenario.inverter = { ...scenario.inverter, battery_power_kw: build.battery.max_discharge_kw };
              scenario.dispatch = build.dispatch;
              scenario.initial_soc_pct = 50;
              scenario.initial_capacity_pct = 100;
              scenario.economics = { ...scenario.economics, capex_usd: build.capex };
            }
            project.scenarios.push(scenario);
            last = scenario;
          }
          state.selected = last.id;
        });
        close(builds.length);
      } catch (error) {
        failure.textContent = error.message;
        failure.classList.remove('hidden');
      }
    };

    dialog.appendChild(el('h3', { text: 'Add a scenario' }));
    dialog.appendChild(el('div', { class: 'template-layout' }, [picks, pane]));
    dialog.appendChild(el('div', { class: 'dialog-actions' }, [
      el('button', { type: 'button', text: 'Cancel', onclick: () => close(null) }),
      el('button', { type: 'button', class: 'primary', text: 'Add to project', onclick: add }),
    ]));
    drawPicks();
    drawPane();
  }).then((added) => {
    if (!added) return;
    show('scenarios');
    toast(`Added ${added} scenario${added === 1 ? '' : 's'}. Review the assumptions, then save.`, null, true,
      { label: 'Undo', run: undo });
  });
}

// ---------------------------------------------------------------------------
// 3 — Results
// ---------------------------------------------------------------------------

async function runAll() {
  if (state.busy || !state.project?.dataset) return;
  if (!(await ensureSaved())) return;
  const { stamp, reply, late } = await sendTagged(currentStamp, (stamp) => {
    const mark = markBusy('run', stamp);
    show('results');
    return guard(() => request(projectPath('/run'), { method: 'POST', revision: stamp.revision }))
      .finally(() => releaseBusy(mark));
  });
  if (late) {
    dropLate('run', stamp);
  } else if (reply) {
    const result = reply.body;
    state.run = result.run;
    state.runStamp = stamp;
    state.resultsHash = result.results_hash;
    state.timelines = Object.fromEntries(result.timelines.map((t) => [t.scenario_id, t]));
    if (!result.run.scenarios.some((s) => s.scenario_id === state.detail)) {
      state.detail = result.run.scenarios.find((s) => !s.comparison.is_baseline)?.scenario_id
        ?? result.run.scenarios[0].scenario_id;
    }
  }
  if (state.view === 'results') renderResults();
  renderBus();
}

function renderResults() {
  const root = document.getElementById('view-results');
  clear(root);

  if (!state.project.dataset) {
    root.appendChild(el('div', { class: 'banner', text: 'Import interval data first.' }));
    return;
  }

  const go = el('button', { class: state.run ? '' : 'primary', type: 'button',
                            text: state.busy ? BUSY_LABEL[state.busy.what] : (state.run ? 'Run again' : 'Run all scenarios'),
                            onclick: runAll });
  go.disabled = Boolean(state.busy);
  root.appendChild(el('div', {}, [
    el('h3', { text: 'Results' }),
    el('div', { class: 'row' }, [
      go,
      el('span', { class: 'tiny muted', text:
        'Every scenario is simulated over every interval. Identical inputs always produce identical results, ' +
        'and the hashes below are how you can check that.' }),
    ]),
  ]));

  if (!state.run) return;
  const run = state.run;

  const notice = staleNotice('You have unsaved changes. These results are from the last saved assumptions. Running again saves first.');
  if (notice) root.appendChild(notice);

  root.appendChild(warningSection(run));
  root.appendChild(comparisonSection(run));
  root.appendChild(billSection(run));
  root.appendChild(detailSection(run));
  root.appendChild(provenanceSection(run));
}

function provenanceSection(run) {
  return el('section', { class: 'stack' }, [
    el('h3', { text: 'Provenance' }),
    el('p', { class: 'lede', text:
      'The same model input hash on the same build gives the same results hash. Compare these against a report or a later run.' }),
    kv([
      ['Results hash', state.resultsHash],
      ['Model input hash', run.model_input_hash],
      ['Project hash', run.project_hash],
      ['Generated', fmt.when(run.generated_utc)],
      ['Model version', run.model_version],
    ]),
  ]);
}

/** Every distinct warning, most serious first, naming the scenarios each came from. */
function collectWarnings(run) {
  const found = new Map();
  const add = (warning, who) => {
    const key = `${warning.code}\u0000${warning.message}`;
    if (!found.has(key)) found.set(key, { ...warning, who: [] });
    if (who) found.get(key).who.push(who);
  };
  for (const warning of run.warnings || []) add(warning, null);
  for (const scenario of run.scenarios) {
    for (const warning of scenario.warnings || []) add(warning, scenario.scenario_name);
  }
  const rank = { critical: 0, caution: 1, info: 2 };
  return [...found.values()].sort((a, b) =>
    rank[a.severity] - rank[b.severity] || a.code.localeCompare(b.code) || a.message.localeCompare(b.message));
}

function warningSection(run) {
  const warnings = collectWarnings(run);
  const box = el('section', { class: 'stack' });
  if (!warnings.length) {
    box.appendChild(el('h3', { text: 'Warning ledger' }));
    box.appendChild(el('p', { class: 'lede', text: 'No warnings were raised. Every assumption is still an assumption.' }));
    return box;
  }
  const serious = warnings.filter((w) => w.severity !== 'info');
  const notes = warnings.filter((w) => w.severity === 'info');
  box.appendChild(el('div', {}, [
    el('h3', { text: `Warning ledger (${warnings.length})` }),
    el('p', { class: 'lede', text: 'Most serious first. Nothing is filtered out for being inconvenient.' }),
  ]));
  if (serious.length) box.appendChild(ledger(serious));
  if (notes.length) {
    box.appendChild(el('details', {}, [
      el('summary', { text: `${notes.length} informational note${notes.length === 1 ? '' : 's'}` }),
      ledger(notes),
    ]));
  }
  return box;
}

function ledger(warnings) {
  const list = el('ul', { class: 'ledger' });
  const icon = { info: 'i', caution: '!', critical: 'x' };
  for (const w of warnings) {
    list.appendChild(el('li', { class: w.severity }, [
      el('span', { class: 'sev', text: `${icon[w.severity]} ${w.severity}` }),
      el('span', { class: 'code', text: w.code }),
      document.createTextNode(w.message),
      w.who?.length ? el('span', { class: 'who', text: w.who.join(', ') }) : null,
    ]));
  }
  return list;
}

/**
 * The comparison, as bars inside a table: each measure is its own column on its own
 * scale, with a tick at the baseline's value. Length is compared down a column only,
 * never across, so no measure is drawn against another's axis. Values are printed, so
 * nothing depends on reading a bar.
 */
function comparisonSection(run) {
  const measure = (title, unit, read, format, role, delta) => ({ title, unit, read, format, role, delta });
  const columns = [
    measure('Grid cost', 'USD per year', (s) => s.economics.scenario_annual_grid_cost_usd, fmt.usd, 'import',
      (s) => s.comparison.grid_cost_delta_usd * s.economics.annualization_multiplier),
    measure('Peak import', 'kW', (s) => s.energy.peak_import_kw, fmt.kw, 'import', (s) => s.comparison.peak_import_delta_kw),
    measure('Self-sufficiency', '% of load', (s) => s.energy.self_sufficiency_pct, fmt.pct, 'generation',
      (s) => s.comparison.self_sufficiency_delta_pct),
    measure('Unmet critical load', 'kWh in outages', (s) => s.energy.unmet_critical_kwh, fmt.kwh, 'unmet',
      (s) => s.comparison.unmet_critical_delta_kwh),
  ];

  const baseline = run.scenarios.find((s) => s.comparison.is_baseline) ?? run.scenarios[0];
  const head = el('tr', {}, [
    el('th', { text: 'Scenario' }),
    ...columns.map((c) => el('th', { class: 'num' }, [c.title, el('span', { class: 'lens-policy', text: c.unit })])),
    el('th', { class: 'num' }, ['Net present value', el('span', { class: 'lens-policy', text: 'USD, against the baseline' })]),
    el('th', { class: 'num' }, ['Payback', el('span', { class: 'lens-policy', text: 'simple' })]),
    el('th', { class: 'num' }, ['Outages', el('span', { class: 'lens-policy', text: 'critical load carried' })]),
  ]);

  const npvValues = run.scenarios.filter(hasInvestment).map((s) => s.economics.npv_usd);
  const npvExtent = Math.max(...npvValues.map(Math.abs), 1e-9);

  const body = el('tbody');
  for (const s of run.scenarios) {
    const isBaseline = s.comparison.is_baseline;
    const row = el('tr', {}, el('td', { class: 'lens-name' }, [
      s.scenario_name,
      isBaseline ? el('span', { class: 'tag', text: 'Baseline', style: { marginLeft: '6px' } }) : null,
      el('span', { class: 'lens-policy', text: s.dispatch_policy }),
    ]));
    for (const column of columns) {
      const values = run.scenarios.map(column.read);
      const high = Math.max(...values, 1e-9);
      const value = column.read(s);
      const track = el('div', { class: 'track' }, value > 1e-9 ? el('div', {
        class: 'bar from-left',
        style: { left: '0', width: `${(value / high) * 100}%`, background: paint(column.role) },
      }) : null);
      if (!isBaseline) {
        track.appendChild(el('div', { class: 'tick', style: { left: `${(column.read(baseline) / high) * 100}%` } }));
      }
      const delta = isBaseline ? null : column.delta(s);
      row.appendChild(el('td', { class: 'measure', title: `${column.title}: ${column.format(value)}` }, [
        el('span', { class: 'lens-value', text: column.format(value) }),
        track,
        delta === null ? null : el('span', { class: 'lens-delta', text: fmt.signed(delta, column.format) }),
      ]));
    }

    // NPV has a sign, and the sign is the whole question: a bar each way from zero.
    const k = s.economics;
    if (hasInvestment(s)) {
      const share = (Math.abs(k.npv_usd) / npvExtent) * 50;
      row.appendChild(el('td', { class: 'measure', title: `Net present value: ${fmt.usd(k.npv_usd)}` }, [
        el('span', { class: 'lens-value', text: fmt.usd(k.npv_usd) }),
        el('div', { class: 'track' }, [
          el('div', { class: `bar ${k.npv_usd >= 0 ? 'from-left' : 'from-right'}`, style: k.npv_usd >= 0
            ? { left: '50%', width: `${share}%`, background: css('--div-pos-2') }
            : { left: `${50 - share}%`, width: `${share}%`, background: css('--div-neg-0') } }),
          el('div', { class: 'zero', style: { left: '50%' } }),
        ]),
        el('span', { class: 'lens-delta', text: `${k.horizon_years} years at ${k.discount_rate_pct_per_year} %` }),
      ]));
    } else {
      row.appendChild(el('td', { class: 'num', text: '—' }));
    }

    // "Never" is a claim about an investment, so a scenario without one has no payback.
    row.appendChild(el('td', { class: 'num', text: !hasInvestment(s) ? '—'
      : (k.simple_payback_years === null || k.simple_payback_years === undefined
        ? `Not in ${k.horizon_years} years` : fmt.years(k.simple_payback_years)) }));

    const outages = s.outages ?? [];
    const survived = outages.filter((o) => o.survived).length;
    row.appendChild(el('td', { class: 'num', text: !outages.length ? 'None modelled'
      : (survived === outages.length ? `All ${outages.length} carried`
        : `${outages.length - survived} of ${outages.length} lost`) }));
    body.appendChild(row);
  }

  return el('section', { class: 'stack' }, [
    el('div', {}, [
      el('h3', { text: 'Compare scenarios' }),
      el('p', { class: 'lede', text:
        'Bars compare within a column. The tick marks the baseline\'s value. A negative change in cost is a saving. ' +
        'These are the rules you set up, compared; Tonans does not search for an optimum.' }),
    ]),
    el('div', { class: 'scroll' }, el('table', { class: 'lens' }, [el('thead', {}, head), body])),
  ]);
}

function hasInvestment(s) {
  return !s.comparison.is_baseline && (Boolean(s.scenario.battery) || s.economics.capex_usd > 0);
}

function billSection(run) {
  const parts = [
    ['Energy imported', (s) => s.cost.import_energy_cost_usd],
    ['Demand charge', (s) => s.cost.demand_charge_usd],
    ['Fixed charge', (s) => s.cost.fixed_charge_usd],
    ['Export credit', (s) => -s.cost.export_credit_usd],
    ['Total', (s) => s.cost.total_grid_cost_usd],
  ];
  const baseline = run.scenarios.find((s) => s.comparison.is_baseline) ?? run.scenarios[0];
  const months = baseline.cost.billed_month_count;

  const head = el('tr', {}, [el('th', { text: 'Scenario' }), ...parts.map(([title]) => el('th', { class: 'num', text: title }))]);
  const body = el('tbody');
  for (const s of run.scenarios) {
    const row = el('tr', {}, el('td', { class: 'lens-name', text: s.scenario_name }));
    for (const [title, read] of parts) {
      const value = read(s);
      const difference = value - read(baseline);
      row.appendChild(el('td', { class: 'measure', title: `${title}: ${fmt.usd(value)}` }, [
        el('span', { class: 'lens-value', text: fmt.usd(value) }),
        s.comparison.is_baseline ? null : el('span', { class: 'lens-delta', text: fmt.signed(difference, fmt.usd) }),
      ]));
    }
    body.appendChild(row);
  }
  return el('section', { class: 'stack' }, [
    el('div', {}, [
      el('h3', { text: 'Where the bill goes' }),
      el('p', { class: 'lede', text: `Over the ${months} billed month${months === 1 ? '' : 's'} in the data. Credit is shown as a negative cost.` }),
    ]),
    el('div', { class: 'scroll' }, el('table', { class: 'lens' }, [el('thead', {}, head), body])),
  ]);
}

/** One scenario in full, chosen from the rest, so the page does not run to dozens of screens. */
function detailSection(run) {
  const chosen = run.scenarios.find((s) => s.scenario_id === state.detail) ?? run.scenarios[0];
  const pick = (id) => { state.detail = id; renderResults(); };
  const picker = run.scenarios.length <= 5
    ? el('div', { class: 'segments' }, run.scenarios.map((s) => el('button', {
      type: 'button',
      'aria-pressed': s.scenario_id === chosen.scenario_id ? 'true' : 'false',
      text: s.scenario_name,
      onclick: () => pick(s.scenario_id),
    })))
    : (() => {
      const node = select('detail-pick', run.scenarios.map((s) => [s.scenario_id, s.scenario_name]), chosen.scenario_id);
      node.addEventListener('change', () => pick(node.value));
      return node;
    })();
  return el('section', { class: 'stack' }, [
    el('h3', { text: 'One scenario in detail' }),
    picker,
    scenarioResult(run, chosen),
  ]);
}

function scenarioResult(run, s) {
  const card = el('div', { class: 'stack' });
  const e = s.energy;
  const k = s.economics;
  card.appendChild(el('div', { class: 'tiles' }, [
    tile('Grid cost (annual)', fmt.usd(k.scenario_annual_grid_cost_usd), annualNote(k.annualization_multiplier, e.duration_hours)),
    tile('Peak grid import', fmt.kw(e.peak_import_kw), 'sets the demand charge'),
    tile('Self-sufficiency', fmt.pct(e.self_sufficiency_pct), 'of load met by on-site generation, directly or after storage'),
    hasInvestment(s)
      ? tile('Net present value', fmt.usd(k.npv_usd), `${k.horizon_years} years at ${k.discount_rate_pct_per_year} %`)
      : tile('Dispatch', s.dispatch_policy, 'the rule this scenario runs'),
  ]));

  const timeline = state.timelines[s.scenario_id];
  if (timeline?.peak_window) {
    card.appendChild(el('h4', { text: 'What happened at the peak' }));
    card.appendChild(peakChart(run, s, timeline.peak_window));
  }

  card.appendChild(el('h4', { text: 'Energy flow' }));
  card.appendChild(energyFlow(e));

  if (timeline) {
    card.appendChild(el('h4', { text: 'The whole data set' }));
    card.appendChild(timelineCharts(timeline, Boolean(s.scenario.battery)));
  }

  if (s.outages?.length) {
    card.appendChild(el('h4', { text: 'Outages' }));
    card.appendChild(outageTable(s.outages));
  }

  card.appendChild(el('details', {}, [
    el('summary', { text: 'Energy balance' }),
    kv([
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
    ]),
    el('p', { class: 'tiny muted', text:
      'The residual is the model checking itself: generation used, plus import, plus discharge, minus load ' +
      'served, export, and charge, summed over every interval. It should be zero to within floating-point ' +
      'rounding. If it is not, nothing above is trustworthy.' }),
  ]));

  if (s.scenario.battery || k.capex_usd > 0) {
    card.appendChild(el('details', {}, [
      el('summary', { text: 'Cash flow, year by year' }),
      cashFlowTable(k),
      el('p', { class: 'tiny muted', text: k.degradation_method }),
    ]));
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
    // Available, not used: what was curtailed is on the other bar, so counting only what
    // was used would leave the bars apart by exactly that, and the balance would not close.
    ['generation', 'Generation', e.generation_available_kwh],
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
      // Equal totals can differ in the last bit and round apart at a .5, which would read
      // as a kWh going missing. The demo does exactly that.
      el('span', { text: fmt.kwh(Math.abs(totalIn - totalOut) < 1e-6 ? scale : total) }),
    ]));
    const present = segments.filter(([, , value]) => value > 0);
    const bar = el('div', { class: 'flow-bar', style: { width: `${(total / scale) * 100}%` } });
    for (const [role, label, value] of present) {
      const fill = paint(role);
      const seg = el('div', {
        class: 'flow-seg',
        style: { flex: `${value} 1 0`, background: fill, color: inkOn(fill) },
        title: `${label}: ${fmt.kwh(value)} (${((value / total) * 100).toFixed(1)} % of ${title.toLowerCase()})`,
      });
      if (value / scale > 0.09) seg.textContent = label;
      bar.appendChild(seg);
    }
    box.appendChild(bar);
    box.appendChild(el('div', { class: 'legend' }, present.map(([role, label, value]) => el('span', {}, [
      el('span', { class: 'swatch', style: { background: paint(role) } }),
      document.createTextNode(`${label} ${fmt.kwh(value)}`),
    ]))));
  }

  const battery = e.battery_charge_kwh > 0 || e.battery_discharge_kwh > 0
    ? `Battery in and Battery out differ by the ${fmt.kwh(losses)} lost in conversion and standing loss, ` +
      `and the ${fmt.kwh(stored)} change in stored energy. `
    : '';
  box.appendChild(el('p', { class: 'chart-note', text:
    `Both bars are drawn to the same scale, and they match because the model conserves energy at the bus. ${battery}` +
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
        el('div', { class: 'tiny muted', text: `${fmt.when(o.start_utc)} to ${fmt.when(o.end_utc)}` }),
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
    'Year', 'Usable capacity', 'Gross saving', 'Operating', 'Throughput', 'Installed cost', 'Replacement',
    'Net cash flow', 'Discounted', 'Cumulative',
  ].map((h) => el('th', { class: 'num', text: h })))));
  const body = el('tbody');
  body.appendChild(el('tr', {}, [
    el('td', { class: 'num', text: '0' }), el('td', { class: 'num', text: '—' }),
    el('td', { class: 'num', text: '—' }), el('td', { class: 'num', text: '—' }),
    el('td', { class: 'num', text: '—' }), el('td', { class: 'num', text: fmt.usd(-k.capex_usd) }),
    el('td', { class: 'num', text: '—' }),
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
      el('td', { class: 'num', text: '—' }),
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
// Canvas charts
// ---------------------------------------------------------------------------

/** Charts on screen, so dark mode can repaint them and a resize redraws each one once. */
const liveCharts = new Set();

function mountChart(canvas, draw) {
  const chart = { canvas, draw, width: 0 };
  liveCharts.add(chart);
  const observer = new ResizeObserver(() => {
    if (!canvas.isConnected) {
      observer.disconnect();
      liveCharts.delete(chart);
      return;
    }
    if (canvas.clientWidth !== chart.width) {
      chart.width = canvas.clientWidth;
      draw();
    }
  });
  observer.observe(canvas);
}

// Dark mode changes every colour; the canvas has to be repainted, since unlike SVG it
// cannot reference a CSS custom property.
window.matchMedia('(prefers-color-scheme: dark)').addEventListener('change', () => {
  for (const chart of liveCharts) {
    if (chart.canvas.isConnected) chart.draw(); else liveCharts.delete(chart);
  }
});

/**
 * Stacked panels sharing one time axis.
 *
 * State of charge gets its own panel rather than a second y-axis on the power
 * chart. A chart with two y-scales invites a reader to compare two quantities whose
 * relative height is an artefact of the scaling the author chose, and that is not a
 * comparison Tonans is willing to imply.
 */
function chartBlock(timeline, panels, note, tickFormat) {
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
  const crosshair = el('div', { class: 'crosshair hidden' });
  wrap.appendChild(crosshair);
  const tip = el('div', { class: 'tooltip hidden' });
  wrap.appendChild(tip);
  wrap.appendChild(el('p', { class: 'chart-note', text: note }));

  mountChart(canvas, () => drawPanels(canvas, tip, crosshair, timeline, panels, tickFormat));
  return wrap;
}

function timelineCharts(timeline, hasBattery) {
  const panels = [
    { title: 'Load and on-site generation', unit: 'kW', signed: false, series: [
      { key: 'load_kw', role: 'load', label: 'Load' },
      { key: 'generation_kw', role: 'generation', label: 'Generation' },
    ] },
    { title: 'Grid exchange: import above zero, export below', unit: 'kW', signed: true, series: [
      { key: 'grid_import_kw', role: 'import', label: 'Import', sign: 1 },
      { key: 'grid_export_kw', role: 'export', label: 'Export', sign: -1 },
    ] },
  ];
  if (hasBattery) {
    panels.push({ title: 'Battery power: discharge above zero, charge below', unit: 'kW', signed: true, series: [
      { key: 'battery_discharge_kw', role: 'discharge', label: 'Discharge', sign: 1 },
      { key: 'battery_charge_kw', role: 'charge', label: 'Charge', sign: -1 },
    ] });
    panels.push({ title: 'State of charge', unit: '%', signed: false, series: [
      { key: 'soc_pct', role: 'soc', label: 'State of charge' },
    ] });
  }
  const note = timeline.decimated
    ? `${timeline.source_interval_count.toLocaleString()} intervals shown as ` +
      `${timeline.bucket_start_utc.length.toLocaleString()} buckets. The shaded band is each bucket's ` +
      'full range, so no peak is hidden. The exported timeseries CSV holds every interval.'
    : `${timeline.source_interval_count.toLocaleString()} intervals, at full resolution.`;
  return chartBlock(timeline, panels, `${note} Times are at the site.`, fmt.day);
}

/** The days around this scenario's highest import, interval by interval, against the baseline. */
function peakChart(run, s, window) {
  const flat = (values) => ({ min: values, max: values, mean: values });
  const timeline = {
    bucket_start_utc: window.start_utc,
    outage: window.outage,
    series: {
      grid_import_kw: flat(window.grid_import_kw),
      baseline_import_kw: flat(window.baseline_grid_import_kw),
      soc_pct: flat(window.soc_pct),
    },
  };
  const ceiling = s.scenario.dispatch.policy === 'peak_limit' ? s.scenario.dispatch.import_limit_kw : null;
  const series = [{ key: 'grid_import_kw', role: 'import', label: 'This scenario' }];
  if (!s.comparison.is_baseline) series.push({ key: 'baseline_import_kw', role: 'baseline', label: 'Baseline', dashed: true });
  const panels = [{
    title: 'Grid import',
    unit: 'kW',
    signed: false,
    series,
    reference: ceiling === null ? null : { value: ceiling, label: `Ceiling ${fmt.kw(ceiling)}` },
  }];
  if (s.scenario.battery) {
    panels.push({ title: 'State of charge', unit: '%', signed: false, series: [
      { key: 'soc_pct', role: 'soc', label: 'State of charge' },
    ] });
  }
  const over = ceiling !== null && window.peak_kw > ceiling + 1e-6;
  const note = `This scenario's peak is ${fmt.kw(window.peak_kw)}, at ${fmt.when(window.peak_utc)} at the site.` +
    (over ? ' It is above the ceiling, so the battery could not hold it there: it reached its reserve floor or its power limit first.' : '') +
    ' Every interval in this window is shown.';
  return chartBlock(timeline, panels, note, fmt.dayHour);
}

function drawPanels(canvas, tip, crosshair, timeline, panels, tickFormat) {
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
  const muted = css('--ink-secondary');
  const gridline = css('--gridline');
  const axis = css('--axis');
  const font = '12px Barlow, system-ui, "Segoe UI", sans-serif';
  const xs = (i) => marginLeft + (plotWidth * i) / Math.max(n - 1, 1);

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
    if (panel.reference) high = Math.max(high, panel.reference.value);
    if (!Number.isFinite(low) || !Number.isFinite(high)) { low = 0; high = 1; }
    if (panel.signed) {
      const extent = Math.max(Math.abs(low), Math.abs(high), 1e-6);
      low = -extent; high = extent;
    } else {
      low = Math.min(low, 0);
      if (high - low < 1e-9) high = low + 1;
    }
    const y = (value) => bottom - ((value - low) / (high - low)) * panelHeight;

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
    ctx.fillText(panel.unit, marginLeft - 6, top - 9);

    // Outage shading, so a reader can see which part of the year is islanded.
    ctx.fillStyle = css('--c-unmet');
    ctx.globalAlpha = 0.10;
    for (let i = 0; i < n; i += 1) {
      if (!timeline.outage[i]) continue;
      ctx.fillRect(xs(i), top, Math.max(plotWidth / n, 1.5), panelHeight);
    }
    ctx.globalAlpha = 1;

    if (panel.reference) {
      const ry = Math.round(y(panel.reference.value)) + 0.5;
      ctx.strokeStyle = ink;
      ctx.lineWidth = 1;
      ctx.beginPath();
      ctx.moveTo(marginLeft, ry);
      ctx.lineTo(marginLeft + plotWidth, ry);
      ctx.stroke();
      ctx.fillStyle = ink;
      ctx.textAlign = 'right';
      ctx.fillText(panel.reference.label, marginLeft + plotWidth, ry - 4);
    }

    for (const s of panel.series) {
      const data = timeline.series[s.key];
      if (!data) continue;
      const sign = s.sign ?? 1;
      const colour = paint(s.role);

      // The band: each bucket's full range. Collapses to nothing when the data is not
      // decimated, which is exactly right.
      ctx.fillStyle = colour;
      ctx.globalAlpha = 0.18;
      ctx.beginPath();
      for (let i = 0; i < n; i += 1) ctx.lineTo(xs(i), y(Math.max(data.max[i] * sign, data.min[i] * sign)));
      for (let i = n - 1; i >= 0; i -= 1) ctx.lineTo(xs(i), y(Math.min(data.max[i] * sign, data.min[i] * sign)));
      ctx.closePath();
      ctx.fill();
      ctx.globalAlpha = 1;

      ctx.strokeStyle = colour;
      ctx.lineWidth = 2;
      ctx.lineJoin = 'round';
      ctx.setLineDash(s.dashed ? [5, 4] : []);
      ctx.beginPath();
      for (let i = 0; i < n; i += 1) {
        const py = y(data.mean[i] * sign);
        if (i === 0) ctx.moveTo(xs(i), py); else ctx.lineTo(xs(i), py);
      }
      ctx.stroke();
      ctx.setLineDash([]);
    }
  });

  // The shared time axis.
  ctx.font = font;
  ctx.fillStyle = muted;
  const ticks = 6;
  for (let t = 0; t < ticks; t += 1) {
    const i = Math.round((t * (n - 1)) / (ticks - 1));
    ctx.textAlign = t === 0 ? 'left' : t === ticks - 1 ? 'right' : 'center';
    ctx.fillText(tickFormat(timeline.bucket_start_utc[i]), xs(i), height - axisHeight + 14);
  }

  canvas.onmousemove = (event) => {
    const rect = canvas.getBoundingClientRect();
    const x = event.clientX - rect.left;
    if (x < marginLeft || x > marginLeft + plotWidth) {
      tip.classList.add('hidden');
      crosshair.classList.add('hidden');
      return;
    }
    const i = Math.round(((x - marginLeft) / plotWidth) * (n - 1));

    clear(tip);
    tip.appendChild(el('div', { class: 't-time', text: fmt.when(timeline.bucket_start_utc[i]) }));
    if (timeline.outage[i]) tip.appendChild(el('div', { class: 't-row bad', text: 'Grid down' }));
    for (const panel of panels) {
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
    const left = canvas.offsetLeft + Math.min(x + 14, rect.width - 200);
    tip.style.left = `${Math.max(left, 6)}px`;
    tip.style.top = `${canvas.offsetTop + Math.min(event.clientY - rect.top + 12, rect.height - 140)}px`;

    crosshair.classList.remove('hidden');
    crosshair.style.left = `${canvas.offsetLeft + x}px`;
    crosshair.style.top = `${canvas.offsetTop + marginTop}px`;
    crosshair.style.height = `${height - axisHeight - marginTop}px`;
  };
  canvas.onmouseleave = () => {
    tip.classList.add('hidden');
    crosshair.classList.add('hidden');
  };
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
  ['capex_usd', 'Installed cost (USD)'],
  ['discount_rate_pct_per_year', 'Discount rate (%/year)'],
];
const paramLabel = (param) => PARAMS.find(([id]) => id === param)?.[1] ?? param;

/** What a metric is, what it reads like, and which way is better within the tested cells. */
const METRICS = [
  { id: 'npv', title: 'Net present value', unit: 'USD', read: (m) => m.npv_usd, scale: 'diverging', better: 'high', format: fmt.usd },
  { id: 'savings', title: 'Annual saving', unit: 'USD', read: (m) => m.annual_savings_usd, scale: 'diverging', better: 'high', format: fmt.usd },
  { id: 'cost', title: 'Annual grid cost', unit: 'USD', read: (m) => m.annual_grid_cost_usd, scale: 'sequential', better: 'low', format: fmt.usd },
  { id: 'peak', title: 'Peak grid import', unit: 'kW', read: (m) => m.peak_import_kw, scale: 'sequential', better: 'low', format: fmt.kw },
  { id: 'self', title: 'Self-sufficiency', unit: '%', read: (m) => m.self_sufficiency_pct, scale: 'sequential', better: 'high', format: fmt.pct },
  { id: 'unmet', title: 'Unmet critical load', unit: 'kWh', read: (m) => m.unmet_critical_kwh, scale: 'sequential', better: 'low', format: fmt.kwh },
  { id: 'payback', title: 'Simple payback', unit: 'years', read: (m) => m.simple_payback_years ?? null, scale: 'sequential', better: 'low', format: fmt.years },
];

function sensitivitySettings() {
  state.sens = studySettings(state.sens, state.project.scenarios, state.project.baseline_scenario_id);
  return state.sens;
}

function renderSensitivity() {
  const root = document.getElementById('view-sensitivity');
  clear(root);
  if (!state.project.dataset) {
    root.appendChild(el('div', { class: 'banner', text: 'Import interval data first.' }));
    return;
  }
  const settings = sensitivitySettings();
  if (!settings) {
    root.appendChild(el('div', { class: 'banner', text: 'Add a scenario first.' }));
    return;
  }
  const scenarioOf = () => state.project.scenarios.find((s) => s.id === settings.scenario);

  const box = el('section', { class: 'stack' });
  box.appendChild(el('div', {}, [
    el('h3', { text: 'Sensitivity' }),
    el('p', { class: 'lede', text:
      `Every cell is a full simulation of the whole data set, not an interpolation, which is why the grid ` +
      `is bounded at ${state.session.limits.max_sensitivity_cells} cells.` }),
  ]));

  const scenarioPick = select('sens-scenario', state.project.scenarios.map((s) => [s.id, s.name]), settings.scenario);
  const axis1 = select('sens-p1', PARAMS, settings.p1);
  const values1 = el('input', { type: 'text', id: 'sens-v1', value: settings.v1, placeholder: 'comma-separated values' });
  const axis2 = select('sens-p2', [['', 'None: one axis only'], ...PARAMS], settings.p2);
  const values2 = el('input', { type: 'text', id: 'sens-v2', value: settings.v2, placeholder: 'comma-separated values' });
  values2.disabled = !settings.p2;

  scenarioPick.addEventListener('change', () => {
    chooseStudyScenario(settings, scenarioPick.value);
    renderSensitivity();
  });
  axis1.addEventListener('change', () => {
    chooseStudyAxis(settings, 1, axis1.value);
    renderSensitivity();
  });
  axis2.addEventListener('change', () => {
    chooseStudyAxis(settings, 2, axis2.value);
    renderSensitivity();
  });
  values1.addEventListener('input', () => { settings.v1 = values1.value; settings.v1Edited = true; });
  values2.addEventListener('input', () => { settings.v2 = values2.value; settings.v2Edited = true; });

  box.appendChild(el('div', { class: 'grid' }, [
    field('Scenario to vary', scenarioPick),
    field('First axis', axis1),
    field('First axis values', values1),
    field('Second axis', axis2),
    field('Second axis values', values2),
  ]));

  // Battery size and installed cost move together in the real world, and a study has to
  // say what it did about that, or the "best" net present value is just the biggest box.
  const energyAxis = settings.p1 === 'battery_energy_kwh' || settings.p2 === 'battery_energy_kwh';
  const costAxis = settings.p1 === 'capex_usd' || settings.p2 === 'capex_usd';
  const scaling = energyAxis && !costAxis;
  if (scaling) {
    const scale = el('input', { type: 'checkbox', id: 'sens-scale' });
    scale.checked = settings.scale;
    const rate = el('input', { type: 'number', step: 'any', min: '0', id: 'sens-rate', value: settings.rate });
    rate.disabled = !settings.scale;
    scale.addEventListener('change', () => { settings.scaleChoice = scale.checked; renderSensitivity(); });
    // Typing a rate is a choice, and it stands until the cost or size it was about changes.
    // It says so, because at any other rate the scenario's own cell will not match Results.
    const own = settings.exactRate;
    const differs = el('p', { class: 'tiny muted' });
    const syncDiffers = () => {
      const typed = settings.rateOverride !== null && own !== null && Number(settings.rate) !== own;
      differs.classList.toggle('hidden', !typed);
      differs.textContent = typed
        ? `This scenario's own rate is ${own} USD per kWh. At the rate above, its own size will not match Results.`
        : '';
    };
    syncDiffers();
    rate.addEventListener('input', () => {
      settings.rate = rate.value;
      settings.rateOverride = rate.value;
      syncDiffers();
    });
    box.appendChild(el('div', { class: 'stack' }, [
      el('label', { class: 'field-row' }, [scale, el('span', { text: 'Move installed cost with battery energy' })]),
      settings.scale
        ? el('div', {}, [el('div', { class: 'grid' }, field('Installed cost (USD per kWh of nameplate)', rate)), differs])
        : el('p', { class: 'tiny muted', text:
          `Every cell keeps this scenario's installed cost of ${fmt.usd(scenarioOf().economics.capex_usd)}, ` +
          'so a larger battery looks better than it is.' }),
    ]));
  } else if (settings.p1 === 'battery_power_kw' || settings.p2 === 'battery_power_kw') {
    box.appendChild(el('p', { class: 'tiny muted', text:
      'Battery power does not change installed cost in this study. Add installed cost as the other axis to see both move.' }));
  }
  if (hasPriceAxis([{ param: settings.p1 }, { param: settings.p2 }])) {
    box.appendChild(el('p', { class: 'tiny muted', text:
      'A price axis changes the price for the baseline as well, because a price is not something the candidate ' +
      'chooses. Each cell is what you would get by setting both scenarios to that price and comparing them.' }));
  }

  const go = el('button', { class: 'primary', type: 'button', text: state.busy ? BUSY_LABEL[state.busy.what] : 'Run sensitivity', onclick: () => runStudy() });
  go.disabled = Boolean(state.busy);
  const count = el('span', { class: 'tiny muted' });
  const countCells = () => {
    const cells = parseValues(settings.v1).values.length * (settings.p2 ? parseValues(settings.v2).values.length : 1);
    count.textContent = `${cells} cell${cells === 1 ? '' : 's'}.`;
  };
  countCells();
  values1.addEventListener('input', countCells);
  values2.addEventListener('input', countCells);
  box.appendChild(el('div', { class: 'row' }, [go, count]));
  const notice = staleNotice(state.study
    ? 'You have unsaved changes. This study is from the last saved assumptions. Running it again saves first.'
    : 'You have unsaved changes. Running a study saves them first.');
  if (notice) root.appendChild(notice);
  root.appendChild(box);

  if (!state.study) return;
  root.appendChild(bestCells(state.study));
  root.appendChild(heatSection(state.study));
  if (state.study.warnings?.length) {
    root.appendChild(el('section', { class: 'stack' }, [el('h3', { text: 'Warnings on this study' }), ledger(state.study.warnings)]));
  }
}

async function runStudy() {
  if (state.busy) return;
  const settings = sensitivitySettings();
  if (!settings) return;
  const first = parseValues(settings.v1);
  const second = settings.p2 ? parseValues(settings.v2) : { ok: true, values: [] };
  if (!first.ok || !second.ok) {
    toast(`Not a number: ${[...(first.bad ?? []), ...(second.bad ?? [])].join(', ')}. Use comma-separated numbers.`, 'ui.bad_axis_values');
    return;
  }
  const axes = [{ param: settings.p1, values: first.values }];
  if (settings.p2) axes.push({ param: settings.p2, values: second.values });
  const body = { scenario_id: settings.scenario, axes };

  const energyAxis = settings.p1 === 'battery_energy_kwh' || settings.p2 === 'battery_energy_kwh';
  const costAxis = settings.p1 === 'capex_usd' || settings.p2 === 'capex_usd';
  if (energyAxis && !costAxis && settings.scale) {
    const rate = Number(settings.rate);
    if (settings.rate.trim() === '' || !Number.isFinite(rate) || rate < 0) {
      toast('Enter the installed cost per kWh, or turn off moving it with battery energy.', 'ui.bad_cost_rate');
      return;
    }
    body.capex_per_kwh_usd = rate;
  }

  if (!(await ensureSaved())) return;
  const { stamp, reply, late } = await sendTagged(currentStamp, (stamp) => {
    const mark = markBusy('study', stamp);
    renderSensitivity();
    return guard(() => request(projectPath('/sensitivity'), { method: 'POST', body, revision: stamp.revision }))
      .finally(() => releaseBusy(mark));
  });
  if (late) {
    dropLate('study', stamp);
  } else if (reply) {
    state.study = reply.body;
    state.studyRequest = body;
    state.bundle = null;
  }
  if (state.view === 'sensitivity') renderSensitivity();
  renderBus();
}

function describeCell(study, cell) {
  return study.axes.map((axis, index) => `${paramLabel(axis.param)} ${formatAxis(cell.axis_values[index])}`).join(', ');
}

/** The cells at the best tested value of a metric. Empty if the metric never varies. */
function bestOf(study, metric) {
  const scored = study.cells
    .map((cell) => ({ cell, value: metric.read(cell.metrics) }))
    .filter(({ value }) => value !== null && value !== undefined);
  if (!scored.length) return { value: null, cells: [], varies: false };
  const values = scored.map(({ value }) => value);
  const target = metric.better === 'high' ? Math.max(...values) : Math.min(...values);
  const tolerance = Math.max(Math.abs(target) * 1e-9, 1e-9);
  const cells = scored.filter(({ value }) => Math.abs(value - target) <= tolerance).map(({ cell }) => cell);
  return { value: target, cells, varies: Math.max(...values) - Math.min(...values) > tolerance };
}

function bestCells(study) {
  const count = study.cells.length;
  // A cell on a price axis is a price the site might face, not a design to pick, and copied
  // into a scenario it would move that scenario's tariff and leave the baseline's behind.
  const priced = hasPriceAxis(study.axes);
  const items = ['npv', 'peak', 'unmet'].map((id) => {
    const metric = METRICS.find((m) => m.id === id);
    const best = bestOf(study, metric);
    const word = metric.better === 'high' ? 'Highest' : 'Lowest';
    const where = !best.varies
      ? `The same in every cell${best.value === null ? '' : `: ${metric.format(best.value)}`}.`
      : best.cells.length > 1
        ? `${best.cells.length} cells tie, first at ${describeCell(study, best.cells[0])}.`
        : `At ${describeCell(study, best.cells[0])}.`;
    return el('div', { class: 'best-item' }, [
      el('div', { class: 'k', text: `${word} ${metric.title.toLowerCase()}` }),
      el('div', { class: 'v', text: best.value === null ? '—' : metric.format(best.value) }),
      el('div', { class: 'where', text: where }),
      best.varies && best.cells.length && !priced ? el('button', { type: 'button', text: 'Add this cell as a scenario',
        onclick: () => addCellAsScenario(study, best.cells[0]) }) : null,
    ]);
  });
  const npv = bestOf(study, METRICS[0]);
  return el('section', { class: 'stack' }, [
    el('div', {}, [
      el('h3', { text: `Best of the ${count} cell${count === 1 ? '' : 's'} you tested` }),
      el('p', { class: 'lede', text:
        'These are the best values among the cells you chose. They are not an optimum: Tonans tests only what ' +
        'you list, and a better answer can sit between or beyond these values.' +
        (npv.value !== null && npv.value < 0 ? ' No tested cell has a positive net present value.' : '') +
        (priced ? ' A price is not a choice: read this as how the result moves with the price, not as a cell to pick.' : '') }),
    ]),
    el('div', { class: 'best' }, items),
  ]);
}

/** Turn a cell into a scenario, applying the axes the way the study did. */
function addCellAsScenario(study, cell) {
  const project = state.project;
  const source = project.scenarios.find((s) => s.id === study.scenario_id);
  if (!source) {
    toast('The scenario this study varied is gone.', 'ui.unknown_scenario');
    return;
  }
  if (project.scenarios.length >= state.session.limits.max_scenarios) {
    toast(`A project holds up to ${state.session.limits.max_scenarios} scenarios.`, 'limit.scenarios');
    return;
  }
  const scenario = addCopy(source, `${source.name}, ${cell.axis_values.map((v) => formatAxis(v)).join(' x ')}`);
  study.axes.forEach((axis, index) => {
    const value = cell.axis_values[index];
    switch (axis.param) {
      case 'battery_energy_kwh': scenario.battery.nameplate_energy_kwh = value; break;
      case 'battery_power_kw':
        scenario.battery.max_charge_kw = value;
        scenario.battery.max_discharge_kw = value;
        scenario.inverter.battery_power_kw = value;
        break;
      case 'reserve_soc_pct': scenario.battery.reserve_soc_pct = value; break;
      case 'initial_capacity_pct': scenario.initial_capacity_pct = value; break;
      case 'import_price_multiplier': scenario.tariff.import_price_multiplier = value; break;
      case 'demand_price_per_kw_month_usd':
        for (const label of Object.keys(scenario.tariff.demand_prices_per_kw_month_usd)) {
          scenario.tariff.demand_prices_per_kw_month_usd[label] = value;
        }
        break;
      case 'calendar_fade_pct_per_year': scenario.degradation.calendar_fade_pct_per_year = value; break;
      case 'capex_usd': scenario.economics.capex_usd = value; break;
      case 'discount_rate_pct_per_year': scenario.economics.discount_rate_pct_per_year = value; break;
      default: break;
    }
  });
  // The cost the study used for this cell, moved with its size or not.
  scenario.economics.capex_usd = cell.metrics.installed_cost_usd;
  checkpoint('add a sensitivity cell as a scenario');
  project.scenarios.push(scenario);
  state.selected = scenario.id;
  syncUnsaved();
  // This screen now has unsaved changes to say so about.
  if (state.view === 'sensitivity') renderSensitivity();
  toast(`Added "${scenario.name}" to the scenarios. Save and run to compare it.`, null, true, {
    label: 'Open it', run: () => show('scenarios'),
  });
}

function heatSection(study) {
  const metric = METRICS.find((m) => m.id === state.studyMetric) ?? METRICS[0];
  const picker = el('div', { class: 'segments' }, METRICS.map((m) => el('button', {
    type: 'button',
    'aria-pressed': m.id === metric.id ? 'true' : 'false',
    text: m.title,
    onclick: () => { state.studyMetric = m.id; renderSensitivity(); },
  })));
  return el('section', { class: 'stack' }, [
    el('div', {}, [el('h3', { text: 'Every cell' }), picker]),
    heatTable(study, metric),
  ]);
}

/**
 * Net present value has a sign, and the sign is the whole question, so it gets a
 * diverging ramp with a neutral zero. Peak import and unmet load are pure
 * magnitudes and get a single-hue ramp. A rainbow would invent boundaries the data
 * does not have.
 */
function heatTable(study, metric) {
  const rows = study.axes[0];
  const cols = study.axes[1];
  const values = study.cells.map((c) => metric.read(c.metrics)).filter((v) => v !== null && v !== undefined);
  const low = values.length ? Math.min(...values) : 0;
  const high = values.length ? Math.max(...values) : 1;
  const extent = Math.max(Math.abs(low), Math.abs(high), 1e-9);

  const ramp = (prefix, count) => Array.from({ length: count }, (_, i) => css(`--${prefix}-${i}`));
  const seq = ramp('seq', 13);
  const pos = ramp('div-pos', 4);
  const neg = ramp('div-neg', 4).reverse(); // Lightest first, like the positive side.
  const fillFor = (v) => {
    if (metric.scale === 'diverging') {
      if (Math.abs(v) < extent * 0.02) return css('--div-mid');
      const side = v >= 0 ? pos : neg;
      return side[Math.min(side.length - 1, Math.round((Math.abs(v) / extent) * (side.length - 1)))];
    }
    const t = high - low < 1e-9 ? 0 : (v - low) / (high - low);
    return seq[Math.min(seq.length - 1, Math.round(t * (seq.length - 1)))];
  };

  const best = bestOf(study, metric);
  const table = el('table', { class: 'heat' });
  const colValues = cols ? cols.values : [null];
  const head = el('tr', {}, [
    el('th', { class: 'axis-name', text: paramLabel(rows.param) }),
    ...colValues.map((c) => el('th', { text: c === null ? metric.title : formatAxis(c) })),
  ]);
  table.appendChild(el('thead', {}, [
    cols ? el('tr', {}, [el('th', {}), el('th', { colspan: String(colValues.length), text: paramLabel(cols.param) })]) : null,
    head,
  ]));

  const body = el('tbody');
  rows.values.forEach((rowValue, r) => {
    const tr = el('tr', {}, el('th', { scope: 'row', text: formatAxis(rowValue) }));
    colValues.forEach((_, c) => {
      const cell = study.cells[r * colValues.length + c];
      const value = cell ? metric.read(cell.metrics) : null;
      if (value === null || value === undefined) {
        tr.appendChild(el('td', { text: cell && metric.id === 'payback' ? 'Not repaid' : '' }));
        return;
      }
      const fill = fillFor(value);
      const marked = best.varies && best.cells.includes(cell);
      tr.appendChild(el('td', {
        class: marked ? 'marked' : '',
        style: { background: fill, color: inkOn(fill) },
        // Values are printed in every cell, so the matrix reads without decoding the
        // colour at all — which is the relief channel the palette requires.
        title: `${describeCell(study, cell)}: ${metric.format(value)}`,
      }, [
        grouped(value, metric.unit === 'USD' || Math.abs(value) >= 1000 ? 0 : 1),
        marked ? el('span', { class: 'cell-mark', text: metric.better === 'high' ? 'highest' : 'lowest' }) : null,
      ]));
    });
    body.appendChild(tr);
  });
  table.appendChild(body);

  return el('div', {}, [
    el('div', { class: 'scroll' }, table),
    el('p', { class: 'chart-note', text:
      `${metric.title} in ${metric.unit}. Every cell was simulated in full. ` +
      (best.varies
        ? `The outlined cell${best.cells.length === 1 ? ' is' : 's are'} the ${metric.better === 'high' ? 'highest' : 'lowest'} of those tested.`
        : 'No cell stands out on this measure.') +
      (study.capex_per_kwh_usd != null
        ? ` Installed cost moved with battery energy at ${study.capex_per_kwh_usd} USD per kWh.` : '') }),
  ]);
}

// ---------------------------------------------------------------------------
// 5 — Export
// ---------------------------------------------------------------------------

async function runExport() {
  if (state.busy) return;
  if (state.study && isUnsaved()) {
    // Saving ends the study, because it was run on what is about to be replaced. That is
    // the user's decision to make, not something to find out from a missing file.
    const choice = await openDialog((dialog, close) => {
      dialog.appendChild(el('h3', { text: 'Exporting will leave out this study' }));
      dialog.appendChild(el('p', { text:
        'You have unsaved changes, and exporting saves them first. The sensitivity study on screen was run on the ' +
        'last saved assumptions, so saving discards it and the bundle will not include it.' }));
      dialog.appendChild(el('div', { class: 'dialog-actions' }, [
        el('button', { type: 'button', text: 'Keep working', onclick: () => close('keep') }),
        el('button', { type: 'button', text: 'Save and export without the study', class: 'primary', onclick: () => close('save') }),
      ]));
    });
    if (choice !== 'save') return;
  }
  if (!(await ensureSaved())) return;
  const { stamp, reply, late } = await sendTagged(currentStamp, (stamp) => {
    const mark = markBusy('export', stamp);
    renderExport();
    return guard(() => request(projectPath('/export'), {
      method: 'POST',
      body: { sensitivity: state.studyRequest },
      revision: stamp.revision,
    })).finally(() => releaseBusy(mark));
  });
  if (late) {
    toast(stamp.project === state.project?.name
      ? `The export finished after the project changed. It describes the earlier version and is in ${reply.body.directory}. Export again for the current one.`
      : `The export of "${stamp.project}" finished after you left it, and is in ${reply.body.directory}.`, 'ui.late_result');
  } else if (reply) {
    state.bundle = reply.body;
    toast(`Wrote ${reply.body.artifacts.length} files.`, null, true);
  }
  if (state.view === 'export') renderExport();
  renderBus();
}

function renderExport() {
  const root = document.getElementById('view-export');
  clear(root);
  if (!state.project.dataset) {
    root.appendChild(el('div', { class: 'banner', text: 'Import interval data first.' }));
    return;
  }

  const go = el('button', { class: state.bundle ? '' : 'primary', type: 'button',
                            text: state.busy ? BUSY_LABEL[state.busy.what] : (state.bundle ? 'Export again' : 'Export bundle'), onclick: runExport });
  go.disabled = Boolean(state.busy);
  const notice = staleNotice('You have unsaved changes. Exporting saves them first.');
  if (notice) root.appendChild(notice);
  root.appendChild(el('section', { class: 'stack' }, [
    el('div', {}, [
      el('h3', { text: 'Export' }),
      el('p', { class: 'lede', text:
        'A self-contained HTML report with no script and no network access, the full results as JSON, every ' +
        'interval as CSV, a provenance manifest with the input hashes, and a SHA-256 checksum of every file. ' +
        'Written into your workspace folder.' }),
    ]),
    el('div', { class: 'row' }, [
      go,
      el('span', { class: 'tiny muted', text: state.study
        ? (isUnsaved()
          ? 'Saving discards the sensitivity study on screen, so this export would leave it out. You will be asked first.'
          : 'The current sensitivity study will be included.')
        : 'Run a sensitivity study first to include one.' }),
    ]),
  ]));

  if (!state.bundle) return;
  const bundle = state.bundle;
  const project = state.project.name;

  const actions = [];
  if (state.desktop) {
    actions.push(el('button', { type: 'button', class: 'primary', text: 'Save the whole bundle to a folder', onclick: async () => {
      const saved = await guard(() => shell('save_bundle', {
        project, key: bundle.key, artifacts: bundle.artifacts.map((a) => a.filename),
      }));
      if (saved) toast(`Saved to ${saved}.`, null, true);
    } }));
    actions.push(el('button', { type: 'button', text: 'Show in file manager', onclick: () =>
      guard(() => shell('reveal_bundle', { project, key: bundle.key })) }));
  }

  const files = el('section', { class: 'stack' }, [
    el('div', {}, [el('h3', { text: 'Bundle' }), el('p', { class: 'mono tiny muted', text: bundle.directory })]),
    actions.length ? el('div', { class: 'row' }, actions) : null,
  ]);
  const list = el('div');
  for (const artifact of bundle.artifacts) {
    const save = el('button', { type: 'button', text: state.desktop ? 'Save as' : 'Download', onclick: () => saveArtifact(bundle, artifact) });
    list.appendChild(el('div', { class: 'artifact' }, [
      el('div', {}, [
        el('div', { class: 'a-name', text: artifact.filename }),
        el('div', { class: 'a-desc', text: artifact.description }),
        el('div', { class: 'a-hash', text: `${artifact.sha256}, ${artifact.size_bytes.toLocaleString()} bytes` }),
      ]),
      save,
    ]));
  }
  files.appendChild(list);
  root.appendChild(files);
}

async function saveArtifact(bundle, artifact) {
  if (state.desktop) {
    const saved = await guard(() => shell('save_artifact', {
      project: state.project.name, key: bundle.key, artifact: artifact.filename,
    }));
    if (saved) toast(`Saved ${saved}.`, null, true);
    return;
  }
  const href = `${projectPath('/exports/')}${encodeURIComponent(bundle.key)}/${encodeURIComponent(artifact.filename)}`;
  const response = await fetch(href, { headers: { Authorization: `Bearer ${state.token}` } });
  if (!response.ok) { toast('Could not read that artifact.', 'export.download_failed'); return; }
  downloadBlob(await response.blob(), artifact.filename);
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
