// The page's rules, under Node: node --test crates/tonans-app/ui-tests/logic.test.mjs
import assert from 'node:assert/strict';
import { test } from 'node:test';

import {
  TARIFF_PRESETS,
  annualNote,
  chooseStudyAxis,
  chooseStudyScenario,
  encodeFileNameHeader,
  exactCostRate,
  fileTooLarge,
  formatAxis,
  hasPriceAxis,
  parseValues,
  readOptionalNumber,
  resultsStatus,
  sendTagged,
  stampMatches,
  studySettings,
} from '../ui/logic.mjs';

const MIB = 1024 * 1024;

test('a file name becomes the ASCII the server decodes', () => {
  // The same pairs as `a_file_name_survives_percent_encoding_in_any_script` in api.rs.
  for (const [name, encoded] of [
    ['load – site.csv', 'load%20%E2%80%93%20site.csv'],
    ['電力.csv', '%E9%9B%BB%E5%8A%9B.csv'],
    ['Müller.csv', 'M%C3%BCller.csv'],
    ['plain.csv', 'plain.csv'],
    ['100% load.csv', '100%25%20load.csv'],
  ]) {
    assert.equal(encodeFileNameHeader(name), encoded);
    assert.match(encoded, /^[\x20-\x7e]*$/, 'fetch accepts only this in a header');
  }
  // A path separator is sent as itself encoded, so that the server sees and refuses it.
  assert.equal(encodeFileNameHeader('../evil.csv'), '..%2Fevil.csv');
});

test('a file is refused from its size alone, with the real limit', () => {
  const limit = 64 * MIB;
  assert.equal(fileTooLarge('a.csv', limit, limit), null, 'the limit itself is allowed');
  const large = fileTooLarge('big70.csv', 70_000_000, limit);
  assert.match(large, /66\.8 MiB/);
  assert.match(large, /up to 64 MiB/);
  assert.match(large, /big70\.csv/);
  assert.match(fileTooLarge('big3g.csv', 3 * 1024 * MIB, limit), /3 GiB.*up to 64 MiB/);
});

/** A request whose answer arrives when the test says so. */
function deferred() {
  let resolve;
  const promise = new Promise((done) => { resolve = done; });
  return { promise, resolve };
}

test('a late run is dropped: opening another project while it runs (review finding 3, A)', async () => {
  let now = { project: 'Big', revision: 'r1' };
  const answer = deferred();
  const running = sendTagged(() => now, () => answer.promise);
  now = { project: 'Demo', revision: 'r9' };
  answer.resolve({ body: 'Big results', revision: 'r1' });
  const { late, stamp } = await running;
  assert.equal(late, true);
  assert.equal(stamp.project, 'Big', 'the message can name the project it belonged to');
});

test('a late run is dropped: saving a change while it runs (review finding 3, B)', async () => {
  let now = { project: 'Big', revision: 'r1' };
  const answer = deferred();
  const running = sendTagged(() => now, () => answer.promise);
  now = { project: 'Big', revision: 'r2' };
  answer.resolve({ body: 'old results', revision: 'r1' });
  assert.equal((await running).late, true);
});

test('an answer on time is kept, and a failure is neither late nor kept', async () => {
  const now = { project: 'Big', revision: 'r1' };
  const kept = await sendTagged(() => now, async () => ({ body: 'results', revision: 'r1' }));
  assert.equal(kept.late, false);
  assert.equal(kept.reply.body, 'results');
  assert.ok(stampMatches(kept.stamp, now));

  const failed = await sendTagged(() => now, async () => null);
  assert.equal(failed.late, false);
  assert.equal(failed.reply, null);

  // The server's own word for which revision it answered about outranks what was sent.
  const moved = await sendTagged(() => now, async () => ({ body: 'x', revision: 'r0' }));
  assert.equal(moved.late, true);
});

test('results are called current only for the state on screen', () => {
  const now = { project: 'Big', revision: 'r2' };
  assert.equal(resultsStatus(null, now), 'Not run');
  assert.equal(resultsStatus(now, now), 'Current');
  assert.equal(resultsStatus(now, now, { unsaved: true }), 'From the last save');
  assert.equal(resultsStatus({ project: 'Big', revision: 'r1' }, now), 'Out of date');
  assert.equal(resultsStatus({ project: 'Demo', revision: 'r2' }, now), 'Out of date');
  assert.equal(resultsStatus(now, now, { changedElsewhere: true }), 'Out of date');
  assert.equal(resultsStatus(now, now, { changedElsewhere: true, unsaved: true }), 'Out of date');
});

test('a blank export limit means none, and unreadable input is not a blank', () => {
  assert.deepEqual(readOptionalNumber('', false), { ok: true, value: null });
  assert.deepEqual(readOptionalNumber('  ', false), { ok: true, value: null });
  assert.deepEqual(readOptionalNumber('120', false), { ok: true, value: 120 });
  // A number input reports `1e` as an empty value with badInput set.
  assert.equal(readOptionalNumber('', true).ok, false);
  assert.equal(readOptionalNumber('1e', false).ok, false);
  assert.equal(readOptionalNumber('Infinity', false).ok, false);
});

test('the flat energy preset leaves no demand charge behind', () => {
  const tariff = { demand_prices_per_kw_month_usd: { all: 15 } };
  TARIFF_PRESETS.find((p) => p.name === 'Flat energy price').apply(tariff, ['all']);
  assert.deepEqual(tariff.demand_prices_per_kw_month_usd, {});
  assert.equal(tariff.import.price_per_kwh_usd, 0.15);

  TARIFF_PRESETS.find((p) => p.name === 'Flat energy plus a demand charge').apply(tariff, ['on', 'off']);
  assert.deepEqual(tariff.demand_prices_per_kw_month_usd, { on: 15, off: 15 });
});

test('the annual tile says what it covers', () => {
  assert.equal(annualNote(1, 8760), 'over the simulated year');
  assert.equal(annualNote(1.005, 8716), 'over the simulated year');
  assert.equal(annualNote(2920, 3), 'scaled 2,920 times from 3 hours of data');
});

test('axis values are shown as they are', () => {
  assert.equal(formatAxis(0.8, 'en-US'), '0.8');
  assert.equal(formatAxis(1.25, 'en-US'), '1.25');
  assert.equal(formatAxis(0.125, 'en-US'), '0.125');
  assert.equal(formatAxis(1000, 'en-US'), '1,000');
  assert.equal(formatAxis(333.3, 'en-US'), '333.3');
});

const battery = (nameplate = 400, power = 150) => ({
  nameplate_energy_kwh: nameplate, max_discharge_kw: power,
});
const scenario = (id, capex, b = battery()) => ({ id, name: id, battery: b, economics: { capex_usd: capex } });

test('the cost rate is exact', () => {
  assert.equal(exactCostRate(scenario('a', 220000)), 550);
  const hard = scenario('a', 123457, battery(333.3));
  const rate = exactCostRate(hard);
  assert.ok(Math.abs(rate * 333.3 - 123457) < 0.005, 'the 1x cell costs the scenario to the cent');
  assert.equal(Number(String(rate)), rate, 'the number survives the text field');
  assert.equal(exactCostRate(scenario('a', 0)), null);
  assert.equal(exactCostRate({ id: 'a', battery: null, economics: { capex_usd: 5 } }), null);
});

test('the cost rate follows an edit with the screen already open (review finding 2)', () => {
  const project = [scenario('base', 0, null), scenario('b1', 220000)];
  const opened = studySettings(null, project, 'base');
  assert.equal(opened.scenario, 'b1');
  assert.equal(opened.rate, '550');
  assert.equal(opened.scale, true);

  // The form stays open while the user edits the installed cost and saves.
  project[1].economics.capex_usd = 440000;
  const after = studySettings(opened, project, 'base');
  assert.equal(after.rate, '1100');
  assert.equal(Number(after.rate) * 400, 440000);
  assert.equal(after.scale, true);

  // A size edit moves it too, and the default axis follows the size.
  project[1].battery.nameplate_energy_kwh = 800;
  const resized = studySettings(after, project, 'base');
  assert.equal(resized.rate, '550');
  assert.equal(resized.v1, '400, 800, 1200, 1600');
});

test('a typed rate stands until the cost or size it was about changes', () => {
  const project = [scenario('b1', 220000)];
  const settings = studySettings(null, project, null);
  settings.rate = '500';
  settings.rateOverride = '500';
  assert.equal(studySettings(settings, project, null).rate, '500');
  project[0].economics.capex_usd = 440000;
  const after = studySettings(settings, project, null);
  assert.equal(after.rateOverride, null);
  assert.equal(after.rate, '1100');
});

test('switching to a scenario with no cost leaves no rate behind', () => {
  const project = [scenario('b1', 220000), scenario('free', 0)];
  const settings = studySettings(null, project, null);
  assert.equal(settings.rate, '550');
  chooseStudyScenario(settings, 'free');
  const after = studySettings(settings, project, null);
  assert.equal(after.rate, '');
  assert.equal(after.scale, false);
});

test('a choice the user made is kept, and a scenario switch clears it', () => {
  const project = [scenario('b1', 220000), scenario('b2', 300000, battery(600, 200))];
  const settings = studySettings(null, project, null);
  settings.scaleChoice = false;
  settings.v1 = '100, 200';
  settings.v1Edited = true;
  project[0].economics.capex_usd = 250000;
  let after = studySettings(settings, project, null);
  assert.equal(after.scale, false);
  assert.equal(after.v1, '100, 200');

  chooseStudyScenario(settings, 'b2');
  after = studySettings(settings, project, null);
  assert.equal(after.scale, true);
  assert.equal(after.v1, '300, 600, 900, 1200');

  chooseStudyAxis(settings, 2, 'reserve_soc_pct');
  assert.equal(studySettings(settings, project, null).v2, '10, 20, 30, 50');
  chooseStudyAxis(settings, 2, '');
  assert.equal(studySettings(settings, project, null).v2, '');
});

test('the scenario keeps its exact size on the axis', () => {
  const project = [scenario('b1', 100000, battery(333.3, 111.1))];
  const settings = studySettings(null, project, null);
  assert.equal(settings.v1.split(', ')[1], '333.3');
  assert.equal(settings.v2.split(', ')[1], '111.1');
  assert.deepEqual(parseValues('1, x').bad, ['x']);
  assert.deepEqual(parseValues('0.8, 1').values, [0.8, 1]);
});

test('a study whose scenario is gone starts again rather than going blank (review finding 6)', () => {
  const project = [scenario('b1', 220000), scenario('b2', 300000)];
  const settings = studySettings(null, project, null);
  settings.scaleChoice = false;
  settings.v1 = '1';
  settings.v1Edited = true;
  project.shift();
  const after = studySettings(settings, project, null);
  assert.equal(after.scenario, 'b2');
  assert.equal(after.scale, true);
  assert.notEqual(after.v1, '1');
  assert.equal(studySettings(after, [], null), null);
});

test('price axes are told apart from the ones a design can choose', () => {
  assert.ok(hasPriceAxis([{ param: 'import_price_multiplier' }]));
  assert.ok(hasPriceAxis([{ param: 'battery_energy_kwh' }, { param: 'demand_price_per_kw_month_usd' }]));
  assert.ok(!hasPriceAxis([{ param: 'battery_energy_kwh' }, { param: '' }]));
});
