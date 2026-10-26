#!/usr/bin/env node
'use strict';

// Run: node scripts/browser-tests.cjs (Node >= 18).
// Prerequisites: build target/debug/debarred-server and crates/web/dist first;
// install Playwright in .tools/browser. No builds, Docker, DB, or real auth here.
// Overrides: BROWSER_TEST_BINARY, PLAYWRIGHT_CHROMIUM_EXECUTABLE.
// Auth scenarios below are BROWSER CONTRACT MOCKS, not actual authentication tests.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const net = require('node:net');
const { spawn } = require('node:child_process');
const { once } = require('node:events');
const root = path.resolve(__dirname, '..');
const sample = JSON.parse(fs.readFileSync(path.join(root, 'crates/web/data/sample-plan.json'), 'utf8'));
const clone = value => JSON.parse(JSON.stringify(value));
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const user = { user_id: 'browser-contract-user-a', email: 'user-a@example.invalid', display_name: 'Fake User A', csrf_token: 'TEST' };
const draftKey = `vitattendance:v1:draft:${user.user_id}:winter-2026`;
const sandboxKey = 'vitattendance:v1:sandbox:winter-2026';
const contexts = new Set();
let browser, child, origin, stage = 'prerequisites', currentFaults;
const pageDiagnostics = new WeakMap();
let rejectInterrupted;
const interrupted = new Promise((_, reject) => { rejectInterrupted = reject; });
const onSignal = () => rejectInterrupted(new Error('Interrupted'));
process.on('SIGINT', onSignal);
process.on('SIGTERM', onSignal);

function check(condition, message) { assert.ok(condition, message); }
async function until(predicate, message) {
  const end = Date.now() + 8000;
  while (Date.now() < end) {
    if (await predicate()) return;
    await sleep(40);
  }
  throw new Error(message);
}
async function bounded(promise, ms, message) {
  let timer;
  try {
    return await Promise.race([promise, interrupted, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(message)), ms);
    })]);
  } finally { clearTimeout(timer); }
}
function safeEnv() {
  // Allowlist, rather than copying the developer's credentials into either child.
  const env = {};
  for (const key of ['PATH', 'HOME', 'TMPDIR', 'TMP', 'TEMP', 'LANG', 'LC_ALL', 'SystemRoot', 'WINDIR', 'DISPLAY', 'XDG_RUNTIME_DIR']) {
    if (process.env[key]) env[key] = process.env[key];
  }
  return env;
}
async function unusedPort() {
  const server = net.createServer();
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');
  const port = server.address().port;
  await new Promise((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
  return port;
}
async function start() {
  const binary = path.resolve(root, process.env.BROWSER_TEST_BINARY || 'target/debug/debarred-server');
  check(fs.existsSync(binary), 'Native server binary missing; build it before running UI tests');
  for (const file of ['app.js', 'app_bg.wasm', 'boot.js', 'app.css']) {
    check(fs.existsSync(path.join(root, 'crates/web/dist/assets', file)), `Frontend asset missing: ${file}; build the web distribution first`);
  }
  let chromium;
  try { ({ chromium } = require(path.join(root, '.tools/browser/node_modules/playwright'))); }
  catch { throw new Error('Playwright missing: install it in .tools/browser/node_modules/playwright'); }
  const port = await unusedPort();
  origin = `http://127.0.0.1:${port}`;
  child = spawn(binary, ['--preview'], {
    cwd: root,
    env: { ...safeEnv(), PREVIEW_PORT: String(port), FRONTEND_DIST: path.join(root, 'crates/web/dist') },
    stdio: ['ignore', 'ignore', 'ignore'], // Never print inherited/server credential-bearing diagnostics.
  });
  let spawnFailed = false;
  child.on('error', () => { spawnFailed = true; });
  await until(async () => {
    check(!spawnFailed && child.exitCode === null && child.signalCode === null, 'Preview server could not start');
    try {
      const response = await fetch(origin, { signal: AbortSignal.timeout(500) });
      await response.arrayBuffer();
      return response.ok;
    } catch { return false; }
  }, 'Preview server did not become ready');
  const chrome = '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
  const executablePath = process.env.PLAYWRIGHT_CHROMIUM_EXECUTABLE || (fs.existsSync(chrome) ? chrome : undefined);
  // With no override/system Chrome, Playwright uses bundled Chromium (CI).
  browser = await chromium.launch({ headless: true, executablePath, env: safeEnv(), timeout: 15000 });
}
function sanitizeDiagnostic(value, limit = 3000) {
  return String(value || '')
    .replace(/^.*(?:authorization|cookie|x-csrf-token|x-session-user)\s*:.*$/gim, '[redacted header]')
    .replace(/\bBearer\s+[^\s"']+/gi, 'Bearer [redacted]')
    .replace(/\b(password|access_token|refresh_token|csrf_token|api[_-]?key|secret)\b["']?\s*[:=]\s*["']?[^\s,"'\r\n}]+/gi, '$1=[redacted]')
    .replace(/https?:\/\/[^\s"'<>]+/g, value => {
      try { const url = new URL(value); return `${url.origin}${url.pathname}`; }
      catch { return '[redacted URL]'; }
    })
    .replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/g, '')
    .slice(0, limit);
}
function markPage(page, label, step) {
  const meta = pageDiagnostics.get(page);
  if (meta) { meta.label = label; meta.step = step; }
}
function recordDiagnostic(faults, page, type, message, stack = '') {
  const meta = pageDiagnostics.get(page);
  let pathname = '[unavailable]';
  try { pathname = new URL(page.url()).pathname; } catch { /* about:blank/closed */ }
  const entry = {
    type, page: meta?.label, step: meta?.step || stage, path: pathname,
    message: sanitizeDiagnostic(message, 2000), stack: sanitizeDiagnostic(stack, 3500),
  };
  const records = type === 'pageerror' ? faults.errors : faults.consoleErrors;
  if (type === 'pageerror') { if (records.length < 8) records.push(entry); }
  else { if (records.length === 16) records.shift(); records.push(entry); }
}
function printDiagnostics(faults) {
  if (!faults) return;
  // Bounded snapshots only: never inspect console JS handles, headers, bodies or env.
  let budget = 18000;
  for (const entry of [...faults.errors, ...faults.consoleErrors]) {
    if (budget <= 0) break;
    const output = sanitizeDiagnostic(`[${entry.type}] page=${entry.page} step=${entry.step} path=${entry.path}\n${entry.message}\n${entry.stack}`, Math.min(5600, budget));
    console.error(output);
    budget -= output.length;
  }
}
async function newContext(options = {}) {
  const context = await browser.newContext({ viewport: { width: 1440, height: 1000 }, acceptDownloads: true, ...options });
  contexts.add(context);
  const faults = { pageErrors: 0, csp: 0, assetErrors: 0, external: 0, routeErrors: 0, privateRequests: 0, assets: new Set(), errors: [], consoleErrors: [], pages: 0 };
    currentFaults = faults;
  await context.exposeBinding('__browserTestCsp', () => { faults.csp++; });
  await context.addInitScript(() => {
    document.addEventListener('securitypolicyviolation', () => { window.__browserTestCsp(); });
  });
  // No scenario can send a request to a real identity provider, API, or image service.
  await context.route('**/*', async route => {
    const url = new URL(route.request().url());
    if (url.origin !== origin && ['http:', 'https:'].includes(url.protocol)) {
      faults.external++;
      return route.abort();
    }
    // Private APIs must be handled by a contract mock, never by the native server.
    if ((url.pathname.startsWith('/api/') && url.pathname !== '/api/runtime') || url.pathname.startsWith('/auth/')) {
      faults.privateRequests++;
      return route.abort();
    }
    return route.continue();
  });
  context.on('page', page => {
    pageDiagnostics.set(page, { label: `page-${++faults.pages}`, step: '' });
    page.on('pageerror', error => {
      faults.pageErrors++;
      recordDiagnostic(faults, page, 'pageerror', error.message, error.stack);
    });
    page.on('console', message => {
      if (message.type() === 'error') recordDiagnostic(faults, page, 'console.error', message.text());
    });
    page.on('response', response => {
      const url = new URL(response.url());
      if (url.origin === origin && url.pathname.startsWith('/assets/')) {
        faults.assets.add(url.pathname);
        if (response.status() >= 400) faults.assetErrors++;
      }
    });
    page.on('requestfailed', request => {
      if (new URL(request.url()).pathname.startsWith('/assets/')) faults.assetErrors++;
    });
  });
  return { context, faults, page: await context.newPage() };
}
function clean(faults, { brokenWasm = false } = {}) {
  check(faults.pageErrors === 0, `Uncaught browser page error(s): ${faults.pageErrors}; ${faults.errors[0]?.page || ''} ${faults.errors[0]?.step || ''}: ${faults.errors[0]?.message || ''}`);
  check(faults.csp === 0, 'CSP violation(s)');
  check(faults.external === 0, 'Unexpected external request(s)');
  check(faults.routeErrors === 0, 'Browser contract route handler failed');
  check(faults.privateRequests === 0, 'Unmocked private API/auth request blocked (no actual DB/provider calls permitted)');
  check(faults.assetErrors === (brokenWasm ? 1 : 0), 'Unexpected missing/failed self-hosted assets');
}
async function close(context) { contexts.delete(context); await context.close(); }
async function hydrated(page, route) {
  if (route) await page.goto(origin + route, { waitUntil: 'load' });
  await page.waitForFunction(() => document.documentElement.dataset.hydrated === 'true');
  check(await page.getByTestId('boot-error').count() === 0, 'Boot error on healthy page');
}
async function text(locator, expected) {
  await until(async () => (await locator.textContent()).includes(expected), `Expected visible content: ${expected}`);
  check(await locator.isVisible(), `Content was not visible: ${expected}`);
}
const status = page => page.getByTestId('save-status');
const cell = page => page.getByRole('button', { name: /Saturday 21 November 2026,/ });
const cse = page => page.locator('.projection-course').filter({ has: page.locator('strong', { hasText: /^CSE1001$/ }) }).first();
async function change(locator, value) { await locator.fill(value); await locator.press('Tab'); }
async function noOverflow(page) {
  check(await page.evaluate(() => document.body.scrollWidth <= innerWidth + 1 && document.documentElement.scrollWidth <= innerWidth + 1), 'Body overflows viewport');
}
async function tabId(page) {
  return page.evaluate(() => sessionStorage.getItem('vitattendance:tab-id:v1'));
}
async function draft(page, key = draftKey) {
  return page.evaluate(key => {
    const pointer = sessionStorage.getItem(`${key}:last-draft`);
    const id = sessionStorage.getItem('vitattendance:tab-id:v1');
    return JSON.parse((pointer && localStorage.getItem(pointer)) ||
      (id && localStorage.getItem(`${key}:${id}`)) || localStorage.getItem(key));
  }, key);
}
async function writerKey(page, key = draftKey) {
  const id = await tabId(page);
  check(Boolean(id), 'Missing stable per-tab session ID');
  const pointer = await page.evaluate(key => sessionStorage.getItem(`${key}:last-draft`), key);
  if (pointer) {
    check(pointer.startsWith(`${key}:${id}:`) && pointer.length > `${key}:${id}:`.length,
      'Draft pointer does not identify a document-specific writer namespace');
    return pointer;
  }
  return `${key}:${id}`; // Compatibility with the previous per-tab storage format.
}
async function ownDraft(page, key = draftKey) {
  return storedDraft(page, await writerKey(page, key));
}
async function storedDraft(page, key) {
  return page.evaluate(key => JSON.parse(localStorage.getItem(key)), key);
}
async function legacyRaw(page, key = draftKey) {
  return page.evaluate(key => localStorage.getItem(key), key);
}
async function remainsUnchanged(read, expected, message, duration = 1400) {
  // Negative assertions must span multiple 650 ms debounce windows.
  const end = Date.now() + duration;
  do {
    assert.deepEqual(await read(), expected, message);
    await sleep(50);
  } while (Date.now() < end);
}
async function confirm(page, label) {
  await page.getByRole('button', { name: label, exact: true }).click();
  await page.getByRole('group', { name: label, exact: true }).getByRole('button', { name: `Confirm ${label}`, exact: true }).click();
}
async function scenario(name, work) {
  stage = name;
  await bounded(work(), 20000, 'Scenario exceeded 20 seconds');
  console.log(`PASS ${name}`);
}

async function ssr() {
  const { context, page, faults } = await newContext({ javaScriptEnabled: false });
  await page.goto(origin);
  await text(page.getByTestId('welcome'), 'attendance anxiety');
  await page.getByTestId('try-sandbox').click();
  await text(page.getByRole('heading', { level: 1 }), 'Your timetable & attendance');
  check(await page.getByLabel('Course code Mon slot 1', { exact: true }).inputValue() === 'CSE1001', 'SSR sample timetable missing');
  await page.goto(origin + '/demo/planner');
  await text(cse(page), '94.7%');
  clean(faults);
  await close(context);
}
async function preview() {
  const { context, page, faults } = await newContext();
  const forbidden = [];
  context.on('request', request => {
    const url = new URL(request.url());
    if (request.method() === 'PUT' || /^\/api\/(plans|helper|image|ai|import)/.test(url.pathname)) forbidden.push(url.pathname);
  });
  await hydrated(page, '/demo');
  await until(async () => Boolean(await draft(page, sandboxKey)), 'Sandbox draft was not initialized');
  await page.getByTestId('planner-tab').click();
  await text(cse(page), '94.7%');
  check(await cell(page).getAttribute('aria-pressed') === 'mixed', 'Sample Nov 21 must start partial');
  await cell(page).click(); // Existing partial selection toggles OFF, not to a whole day.
  await text(cse(page), '100.0%');
  check(await cell(page).getAttribute('aria-pressed') === 'false', 'Whole-day toggle did not clear partial leave');
  await cell(page).click();
  check(await cell(page).getAttribute('aria-pressed') === 'true', 'Whole day was not selected');
  await text(page.locator('.hero-stats'), '2 classes missed');
  await page.getByRole('radio', { name: 'Per classes', exact: true }).check();
  await cell(page).click();
  const picker = page.getByRole('region', { name: 'Choose classes on 2026-11-21' });
  const cseBox = picker.getByRole('checkbox', { name: /CSE1001/ });
  const matBox = picker.getByRole('checkbox', { name: /MAT1001/ });
  check(await cseBox.isChecked() && await matBox.isChecked(), 'Whole-day selection did not populate class checkboxes');
  await cseBox.uncheck();
  check(await matBox.isChecked(), 'Partial selection lost the other course');
  check(await cell(page).getAttribute('aria-pressed') === 'mixed', 'Partial day not exposed accessibly');
  await text(cse(page), '100.0%');
  await text(page.locator('.hero-stats'), '1 classes missed');
  await cseBox.check();
  await text(page.locator('.hero-stats'), '2 classes missed');
  const saved = await draft(page, sandboxKey);
  check(saved.plan.leave.length === 1 && saved.plan.leave[0].kind === 'day', 'Leave was double-counted instead of normalized');
  await page.reload();
  await hydrated(page);
  await until(async () => await cell(page).getAttribute('aria-pressed') === 'true', 'Sandbox selection did not survive reload');
  await noOverflow(page);
  const screenshots = path.join(root, '.tools/screenshots');
  fs.mkdirSync(screenshots, { recursive: true });
  await page.screenshot({ path: path.join(screenshots, 'desktop-planner.png'), fullPage: true });
  await page.getByTestId('setup-tab').click();
  const attendance = page.getByLabel('Classes attended CSE1001', { exact: true });
  await change(attendance, String(Number(await attendance.getAttribute('max')) + 1));
  await text(status(page), 'Invalid plan');
  await page.getByTestId('planner-tab').click();
  await text(page.getByText(/Fix the plan to see reliable projections:/), 'Fix the plan');
  check(await page.locator('.projection-course').count() === 0, 'Invalid attendance still shows reliable projections');
  await page.getByTestId('setup-tab').click();
  await change(page.getByLabel('Classes attended CSE1001', { exact: true }), '4');
  await page.getByRole('button', { name: 'Clear timetable', exact: true }).click();
  check(await page.getByLabel('Course code Mon slot 1', { exact: true }).inputValue() === 'CSE1001', 'Clear changed data before confirmation');
  await page.getByRole('button', { name: 'Cancel', exact: true }).click();
  await confirm(page, 'Clear timetable');
  check(await page.getByLabel('Course code Mon slot 1', { exact: true }).inputValue() === '', 'Confirmed clear did not clear timetable');
  await confirm(page, 'Load sample timetable');
  const downloading = page.waitForEvent('download');
  await page.getByRole('button', { name: 'Export draft', exact: true }).click();
  const download = await downloading;
  check(download.suggestedFilename().endsWith('.json'), 'Export is not JSON');
  const stream = await download.createReadStream();
  const chunks = [];
  for await (const chunk of stream) chunks.push(chunk);
  const exported = JSON.parse(Buffer.concat(chunks).toString('utf8'));
  assert.deepEqual(exported, sample, 'Draft export must contain only the sample Plan');
  check(!/password|token|secret|csrf/i.test(JSON.stringify(exported)), 'Export contains credential fields');
  await page.locator('.desktop-helper').getByRole('button', { name: 'Check my status', exact: true }).click();
  await text(page.locator('.desktop-helper .message.answer').last(), 'CSE1001');
  await page.getByTestId('planner-tab').click();
  await text(cse(page), '94.7%');
  for (const asset of ['/assets/boot.js', '/assets/app.js', '/assets/app_bg.wasm', '/assets/app.css']) {
    check(faults.assets.has(asset), `Browser did not load ${asset}`);
  }
  check(forbidden.length === 0, 'Sandbox contacted cloud plans/helper/image APIs');
  clean(faults);
  await close(context);
}
async function mobile() {
  const { context, page, faults } = await newContext({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true });
  await hydrated(page, '/demo');
  await noOverflow(page);
  await page.getByTestId('planner-tab').click();
  await text(cse(page), '94.7%');
  await noOverflow(page);
  const toggle = page.getByRole('button', { name: 'Attendance helper', exact: true });
  await toggle.focus();
  check(await toggle.evaluate(el => el === document.activeElement), 'Mobile helper toggle is not keyboard-focusable');
  await page.keyboard.press('Enter');
  const dialog = page.getByRole('dialog', { name: 'Attendance helper', exact: true });
  await dialog.waitFor({ state: 'visible' });
  const closeButton = dialog.getByRole('button', { name: 'Close attendance helper' });
  await closeButton.focus();
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached' });
  check(await toggle.getAttribute('aria-expanded') === 'false', 'Escape left helper expanded');
  await toggle.click();
  await dialog.getByRole('button', { name: 'Close attendance helper' }).click();
  await dialog.waitFor({ state: 'detached' });
  await page.screenshot({ path: path.join(root, '.tools/screenshots/mobile-planner.png'), fullPage: true });
  clean(faults);
  await close(context);
}

// Browser-only contract mocks. They do not establish cookies or test real auth,
// CSRF enforcement, RLS, Supabase, quotas enforcement, or database persistence.
async function mocked({ getStatus = 200, putStatus = 200, holdFirst = false, seed, probeDraftWrites = false } = {}) {
  const result = await newContext();
  const { context, faults } = result;
  const state = { plan: clone(sample), revision: 1, puts: [], gets: 0, active: 0, maxActive: 0, getStatus, putStatus, release: null, unexpected: 0, quota: 0,
      csrf: 'TEST', me: 0, holdGetPage: null, releaseGet: null, logout: 0, deleted: false };
  if (seed || probeDraftWrites) await context.addInitScript(({ key, value, probe }) => {
    if (value && !localStorage.getItem(key)) localStorage.setItem(key, JSON.stringify(value));
    if (probe) {
      // Seed and instrument together: Playwright does not order separate init scripts.
      const original = Storage.prototype.setItem;
      window.__failedLoadDraftWrites = 0;
      Storage.prototype.setItem = function (name, content) {
        if (this === localStorage && (name === key || name.startsWith(`${key}:`))) window.__failedLoadDraftWrites++;
        return original.call(this, name, content);
      };
    }
  }, { key: draftKey, value: seed, probe: probeDraftWrites });
  await context.route('**/api/**', async route => {
    try {
      const request = route.request();
      const url = new URL(request.url());
      let body, code = 200;
      if (url.pathname === '/api/runtime' && request.method() === 'GET') {
        body = { mode: 'local', auth_available: true, local_password_enabled: false, ai_enabled: false, today: '2026-11-21' };
      } else if (url.pathname === '/api/me' && request.method() === 'GET') {
              state.me++;
              code = state.deleted ? 401 : 200;
              body = state.deleted ? {} : { ...user, csrf_token: state.csrf };
            }
      else if (url.pathname === '/api/quota' && request.method() === 'GET') {
        state.quota++;
        body = { helper: { limit: 10, used: 2, remaining: 8 }, image: { limit: 3, used: 0, remaining: 3 } };
      } else if (url.pathname === '/api/plans/winter-2026' && request.method() === 'GET') {
        state.gets++;
        code = state.getStatus;
        body = { plan: clone(state.plan), revision: state.revision };
        if (request.frame().page() === state.holdGetPage) {
          await new Promise(resolve => { state.releaseGet = resolve; });
        }
      } else if (url.pathname === '/api/plans/winter-2026' && request.method() === 'PUT') {
        const payload = request.postDataJSON();
        state.puts.push({ payload, headers: request.headers() });
        state.active++;
        state.maxActive = Math.max(state.maxActive, state.active);
        if (holdFirst && state.puts.length === 1) {
          await new Promise(resolve => { state.release = resolve; });
        }
        code = state.putStatus;
        if (code === 200) { state.plan = clone(payload.plan); state.revision++; }
        body = { plan: state.plan, revision: state.revision };
        await route.fulfill({ status: code, json: body });
        state.active--;
        return;
      } else { state.unexpected++; code = 500; body = {}; }
      await route.fulfill({ status: code, json: body });
    } catch {
      faults.routeErrors++;
      await route.abort().catch(() => {});
    }
  });
  await context.route('**/auth/logout', async route => {
    state.logout++;
    await route.fulfill({ status: 500, json: {} });
  });
  return { ...result, state };
}
async function loaded(page) {
  await hydrated(page, '/setup');
  await text(status(page), 'All changes saved');
  check(await page.getByLabel('Classes attended CSE1001', { exact: true }).inputValue() === '4', 'Mock cloud inputs did not load');
  await text(page.locator('.desktop-helper'), '8 of 10 helper questions remaining');
}
function nameInput(page) {
  const course = page.locator('.attendance-course').filter({ has: page.locator('strong', { hasText: /^CSE1001$/ }) });
  return course.getByRole('textbox', { name: 'Course name', exact: true });
}
async function editName(page, value) { await change(nameInput(page), value); }
function contractClean(result) {
  clean(result.faults);
  check(result.state.unexpected === 0, 'Unexpected mocked API request');
}
async function saves() {
  const result = await mocked({ holdFirst: true });
  const { context, page, state } = result;
  await loaded(page);
  await sleep(750);
  check(state.puts.length === 0, 'Loading cloud inputs caused a spurious PUT');
  await editName(page, 'Device edit one');
  await editName(page, 'Device edit two');
  await until(() => state.puts.length === 1 && state.release, 'Debounced PUT was not sent');
  check(state.puts[0].payload.expected_revision === 1, 'First save used wrong revision');
  check(state.puts[0].payload.plan.timetable.courses[0].name === 'Device edit two', 'Debounce did not retain latest edit');
  check(state.puts[0].headers['x-csrf-token'] === 'TEST', 'PUT omitted fake CSRF header');
  check(state.puts[0].headers['x-session-user'] === user.user_id, 'PUT omitted session-user header');
  await editName(page, 'Edit during in-flight save');
  await sleep(800);
  check(state.puts.length === 1, 'Save requests overlapped');
  state.release();
  await until(() => state.puts.length === 2, 'In-flight edit did not produce a subsequent save');
  await text(status(page), 'All changes saved');
  check(state.puts[1].payload.expected_revision === 2, 'Next save did not use returned revision');
  check(state.puts[1].payload.plan.timetable.courses[0].name === 'Edit during in-flight save', 'Next save lost the in-flight edit');
  await sleep(750);
  check(state.maxActive === 1 && state.puts.length === 2, 'Saves were not serialized/exactly debounced');
  check((await draft(page)).base_revision === 3 && !(await draft(page)).dirty, 'Successful save did not update device draft');
  contractClean(result);
  await close(context);
}
async function conflict() {
  const result = await mocked({ putStatus: 409 });
  const { context, page, state } = result;
  await loaded(page);
  await editName(page, 'Keep conflicting device draft');
  await text(status(page), 'Conflict');
  state.revision = 2;
  for (let i = 0; i < 2; i++) {
    const preserved = await draft(page);
    check(preserved.dirty && preserved.base_revision === 1, 'Conflict discarded dirty draft/base revision');
    check(preserved.plan.timetable.courses[0].name === 'Keep conflicting device draft', 'Conflict lost device edits');
    await page.reload();
    await hydrated(page);
    await text(status(page), 'Conflict');
    await sleep(800);
    check(state.puts.length === 1, 'Conflict reload automatically overwrote cloud plan');
  }
  check((await draft(page)).base_revision === 1, 'Repeated reload adopted the conflicting cloud revision');
  contractClean(result);
  await close(context);
}
async function failedLoad() {
  const local = clone(sample);
  local.timetable.courses[0].name = 'Draft survives unavailable service';
  const result = await mocked({ getStatus: 503, seed: { plan: local, base_revision: 1, dirty: true } });
  const { context, page, state } = result;
  await hydrated(page, '/setup');
  await text(status(page), 'Save or load failed');
  check(await page.getByLabel('Course code Mon slot 1', { exact: true }).inputValue() === 'CSE1001', '503 replaced draft with empty plan');
  check(await page.getByLabel('Course code Mon slot 1', { exact: true }).isDisabled(), 'Editing enabled before successful initial GET');
  await sleep(800);
  check(state.puts.length === 0 && (await draft(page)).dirty, 'Failed initial load wrote an empty plan or lost draft');
  state.getStatus = 200;
  await page.getByRole('button', { name: 'Retry save / load', exact: true }).click();
  await text(status(page), 'All changes saved');
  check(state.gets === 2 && state.puts.length === 1, 'Retry did not recover through GET then draft save');
  check(state.puts[0].payload.expected_revision === 1 && state.puts[0].payload.plan.timetable.courses[0].name === local.timetable.courses[0].name, 'Retry overwrote preserved draft');
  contractClean(result);
  await close(context);
}
async function expired() {
  const result = await mocked({ putStatus: 401 });
  const { context, page, state } = result;
  await loaded(page);
  await editName(page, 'Preserved expired-session draft');
  await text(status(page), 'Session expired');
  check(await page.getByLabel('Course code Mon slot 1', { exact: true }).isDisabled(), 'Expired session can still edit');
  await page.getByTestId('planner-tab').click();
  check(await cell(page).isDisabled(), 'Expired session can still mutate leave');
  await sleep(1400);
  check(state.puts.length === 1, 'Expired session continued PUT requests');
  const preserved = await draft(page);
  check(preserved.dirty && preserved.plan.timetable.courses[0].name === 'Preserved expired-session draft', '401 lost device draft');
  contractClean(result);
  await close(context);
}
async function delayedRuntimeDraft() {
  const { context, page, faults } = await newContext();
  const local = clone(sample);
  local.attendance.attended.CSE1001 = 3;
  local.timetable.courses[0].name = 'Restored before runtime';
  const cached = { plan: local, base_revision: 0, dirty: true };
  await context.addInitScript(({ key, cached }) => {
    const original = Storage.prototype.setItem;
    original.call(localStorage, key, JSON.stringify(cached));
    window.__draftProbe = { legacyWrites: 0, enabledBeforeRestore: false };
    Storage.prototype.setItem = function (name, value) {
      if (this === localStorage && name === key) window.__draftProbe.legacyWrites++;
      return original.call(this, name, value);
    };
    const inspect = () => {
      const editor = document.querySelector('input[aria-label="Course code Mon slot 1"]');
      const attended = document.querySelector('input[aria-label="Classes attended CSE1001"]');
      if (editor && !editor.disabled && (!attended || attended.value !== '3')) {
        window.__draftProbe.enabledBeforeRestore = true;
      }
    };
    new MutationObserver(inspect).observe(document, { subtree: true, childList: true, attributes: true });
    const frame = () => { inspect(); window.__draftProbeFrame = requestAnimationFrame(frame); };
    requestAnimationFrame(frame);
  }, { key: sandboxKey, cached });
  let releaseRuntime;
  let forbidden = 0;
  context.on('request', request => {
    const pathname = new URL(request.url()).pathname;
    if (request.method() !== 'GET' || (pathname.startsWith('/api/') && pathname !== '/api/runtime')) forbidden++;
  });
  await context.route('**/api/runtime', async route => {
    await new Promise(resolve => { releaseRuntime = resolve; });
    await route.fulfill({ json: { mode: 'preview', auth_available: false, local_password_enabled: false, ai_enabled: false } });
  });
  await hydrated(page, '/demo');
  await until(() => Boolean(releaseRuntime), 'Runtime request did not reach the controlled gate');
  await until(async () => await nameInput(page).inputValue() === local.timetable.courses[0].name,
    'Sandbox did not restore custom draft while runtime response was delayed');
  check(await page.getByLabel('Classes attended CSE1001', { exact: true }).inputValue() === '3', 'Sandbox restored wrong attendance');
  check(!(await page.evaluate(() => window.__draftProbe.enabledBeforeRestore)), 'Sandbox enabled the SSR/sample editor before restoring the draft');
  // Stop the pre-restore probe before making an intentional edit.
  await page.evaluate(() => cancelAnimationFrame(window.__draftProbeFrame));
  await editName(page, 'Edited while runtime still delayed');
  const id = await tabId(page);
  check(Boolean(id), 'Sandbox did not establish a per-tab namespace');
  check((await ownDraft(page, sandboxKey)).plan.timetable.courses[0].name === 'Edited while runtime still delayed', 'Delayed runtime edit was not persisted to own namespace');
  const runtimeResponse = page.waitForResponse(response => new URL(response.url()).pathname === '/api/runtime');
  releaseRuntime();
  await runtimeResponse;
  await remainsUnchanged(async () => ({
    name: await nameInput(page).inputValue(),
    legacy: await legacyRaw(page, sandboxKey),
    writes: await page.evaluate(() => window.__draftProbe.legacyWrites),
  }), { name: 'Edited while runtime still delayed', legacy: JSON.stringify(cached), writes: 0 }, 'Runtime completion overwrote a draft or wrote the legacy base');
  check(await tabId(page) === id, 'Sandbox tab ID changed during runtime completion');
  check(forbidden === 0, 'Delayed-runtime sandbox contacted a cloud API');
  clean(faults);
  await close(context);
}
async function failedLoadSessionRefresh() {
  const local = clone(sample);
  local.timetable.courses[0].name = 'Dirty draft survives session refresh';
  const cached = { plan: local, base_revision: 1, dirty: true };
  const result = await mocked({ getStatus: 503, putStatus: 503, seed: cached, probeDraftWrites: true });
  const { context, page, state } = result;
  await hydrated(page, '/setup');
  await text(status(page), 'Save or load failed');
  const id = await tabId(page);
  check(Boolean(id), 'Failed load did not retain a per-tab identity');
  assert.deepEqual(await draft(page), cached, 'Initial GET 503 lost cached metadata');
  check(await nameInput(page).inputValue() === local.timetable.courses[0].name && await nameInput(page).isDisabled(), 'Failed load did not display a locked cached draft');
  // A canceled sign-out must warn about dirty metadata even though loading failed.
  let validWarning = false;
  await Promise.all([
    page.waitForEvent('dialog').then(async dialog => {
      validWarning = dialog.type() === 'confirm' && /not.*saved|unsaved/i.test(dialog.message());
      await dialog.dismiss();
    }),
    page.getByRole('button', { name: 'Sign out', exact: true }).click(),
  ]);
  check(validWarning, 'Dirty failed-load draft did not trigger a sign-out warning');
  check(state.logout === 0, 'Canceled sign-out sent a logout request');
  state.csrf = 'TEST-REFRESHED';
  const gets = state.gets;
  const me = state.me;
  const reloaded = page.waitForResponse(response => new URL(response.url()).pathname === '/api/plans/winter-2026');
  await page.evaluate(() => window.dispatchEvent(new Event('focus')));
  await reloaded;
  await until(() => state.me > me && state.gets > gets, 'Same-user CSRF refresh did not recheck session and reload');
  await text(status(page), 'Save or load failed');
  assert.deepEqual(await draft(page), cached, 'Focus/session refresh replaced dirty/base-revision metadata');
  check(await page.evaluate(() => window.__failedLoadDraftWrites) === 0, 'Failed cloud load persisted over the cached draft');
  check(await legacyRaw(page) === JSON.stringify(cached), 'Session refresh wrote the legacy base');
  await page.reload();
  await hydrated(page);
  await text(status(page), 'Save or load failed');
  check(await tabId(page) === id, 'Reload changed the tab namespace');
  assert.deepEqual(await draft(page), cached, 'Reload after CSRF refresh lost the cached draft');
  check(await nameInput(page).inputValue() === local.timetable.courses[0].name && await nameInput(page).isDisabled(), 'Reload after 503 lost/reenabled the cached draft');
  check(await page.evaluate(() => window.__failedLoadDraftWrites) === 0, 'Failed reload persisted draft metadata');
  check(state.puts.length === 0, 'Failed initial/focus/reload GET sent a PUT');
  // A successful retry may save the preserved draft, but failed PUT must not loop.
  state.getStatus = 200;
  await page.getByRole('button', { name: 'Retry save / load', exact: true }).click();
  await until(() => state.puts.length === 1 && state.active === 0, 'Retry did not submit the preserved draft');
  await text(status(page), 'Save or load failed');
  check(state.puts[0].payload.expected_revision === 1 && state.puts[0].headers['x-csrf-token'] === 'TEST-REFRESHED', 'Retry lost base revision or refreshed CSRF');
  assert.deepEqual(state.puts[0].payload.plan, local, 'Retry sent something other than the preserved plan');
  assert.deepEqual(await ownDraft(page), cached, 'Failed PUT did not retain own dirty draft');
  await remainsUnchanged(() => state.puts.length, 1, 'Failed PUT automatically retried');
  check(await legacyRaw(page) === JSON.stringify(cached), 'Retry wrote the read-only legacy base');
  contractClean(result);
  await close(context);
}
async function twoTabDraftIsolation() {
  const result = await mocked({ putStatus: 503 });
  const { context, page: pageA, state } = result;
  await loaded(pageA);
  const pageB = await context.newPage();
  state.holdGetPage = pageB;
  await hydrated(pageB, '/setup');
  await until(() => Boolean(state.releaseGet), 'Tab B plan GET did not reach the controlled gate');
  check(await pageB.getByLabel('Course code Mon slot 1', { exact: true }).isDisabled(), 'Tab B edited before its cloud load completed');
  await editName(pageA, 'Tab A unsaved private draft');
  await text(status(pageA), 'Save or load failed');
  const idA = await tabId(pageA);
  const idB = await tabId(pageB);
  check(Boolean(idA) && Boolean(idB) && idA !== idB, 'Same-context pages did not receive distinct sessionStorage namespaces');
  const savedA = await ownDraft(pageA);
  check(savedA.dirty && savedA.base_revision === 1 && savedA.plan.timetable.courses[0].name === 'Tab A unsaved private draft', 'Tab A failed PUT did not preserve its own draft');
  state.releaseGet();
  state.holdGetPage = null;
  await text(status(pageB), 'All changes saved');
  check(await nameInput(pageB).inputValue() === sample.timetable.courses[0].name, 'Delayed tab B unexpectedly restored tab A edits');
  assert.deepEqual(await ownDraft(pageA), savedA, 'Delayed tab B GET clobbered tab A unsaved storage');
  await editName(pageB, 'Tab B separate unsaved draft');
  await text(status(pageB), 'Save or load failed');
  check((await ownDraft(pageB)).plan.timetable.courses[0].name === 'Tab B separate unsaved draft', 'Tab B did not persist to its own namespace');
  assert.deepEqual(await ownDraft(pageA), savedA, 'Tab B edit clobbered tab A unsaved storage');
  await remainsUnchanged(() => state.puts.length, 2, 'A failed tab PUT entered an automatic retry loop');
  check(await legacyRaw(pageA) === null, 'Per-tab draft write created the read-only legacy base');
  await pageA.reload();
  await hydrated(pageA);
  await until(async () => await nameInput(pageA).inputValue() === 'Tab A unsaved private draft', 'Tab A reload lost its private draft');
  await until(() => state.puts.length === 3 && state.active === 0, 'Tab A reload did not restore its dirty draft for save');
  await text(status(pageA), 'Save or load failed');
  check(await tabId(pageA) === idA && await tabId(pageB) === idB, 'Tab IDs were not stable across reload');
  assert.deepEqual(await ownDraft(pageA), savedA, 'Tab A reload changed its unsaved draft metadata');
  check((await ownDraft(pageB)).plan.timetable.courses[0].name === 'Tab B separate unsaved draft', 'Tab A reload clobbered tab B draft');
  await remainsUnchanged(() => state.puts.length, 3, 'Reloaded failed PUT automatically retried');
  const pageC = await context.newPage();
  await loaded(pageC);
  check(await nameInput(pageC).inputValue() === sample.timetable.courses[0].name,
    'Fresh cloud tab automatically adopted an archived device draft instead of server plan');
  const shelf = pageC.locator('details').filter({ has: pageC.locator('summary', { hasText: /^Device drafts$/ }) });
  await shelf.waitFor({ state: 'visible' });
  if (await shelf.getAttribute('open') === null) await shelf.locator('summary').click();
  const archivedA = shelf.getByTestId('device-draft').filter({ hasText: 'Tab A unsaved private draft' }).first();
  await archivedA.getByRole('button', { name: 'Restore draft', exact: true }).click();
  check(await nameInput(pageC).inputValue() === sample.timetable.courses[0].name && state.puts.length === 3,
    'Cloud shelf adopted/saved an archived draft before confirmation');
  await archivedA.getByRole('group', { name: 'Restore draft', exact: true })
    .getByRole('button', { name: 'Confirm Restore draft', exact: true }).click();
  await until(async () => await nameInput(pageC).inputValue() === 'Tab A unsaved private draft', 'Explicit cloud shelf restore did not recover A');
  await until(() => state.puts.length === 4 && state.active === 0, 'Explicit cloud shelf restore did not save recovered draft');
  await text(status(pageC), 'Save or load failed');
  assert.deepEqual(await ownDraft(pageA), savedA, 'Cloud shelf restore overwrote another document draft');
  check((await ownDraft(pageB)).plan.timetable.courses[0].name === 'Tab B separate unsaved draft', 'Cloud shelf restore overwrote tab B');
  contractClean(result);
  await close(context);
}
async function duplicatedSandboxRecovery() {
  const { context, page: pageA, faults } = await newContext();
  markPage(pageA, 'A', 'initial sandbox load and retained edit');
  await context.addInitScript(({ base, allowedOrigin }) => {
    // Fresh pages run init scripts on opaque about:blank before navigating.
    // Only inspect storage on the preview document whose behavior we test.
    // A popup's initial about:blank inherits the opener's security origin even
    // though location.origin is "null". Window.origin reflects that inheritance.
    window.__draftSnapshotOrigin = { actual: location.origin, effective: window.origin, expected: allowedOrigin };
    if (window.origin !== allowedOrigin) return;
    window.__draftAtDocumentStart = {
      tab: sessionStorage.getItem('vitattendance:tab-id:v1'),
      pointer: sessionStorage.getItem(`${base}:last-draft`),
    };
  }, { base: sandboxKey, allowedOrigin: origin });
  let cloudRequests = 0;
  context.on('request', request => {
    const pathname = new URL(request.url()).pathname;
    if (request.method() !== 'GET' || (pathname.startsWith('/api/') && pathname !== '/api/runtime')) cloudRequests++;
  });
  await hydrated(pageA, '/demo');
  await editName(pageA, 'A retained');
  const idA = await tabId(pageA);
  const keyA = await writerKey(pageA, sandboxKey);
  check(keyA.startsWith(`${sandboxKey}:${idA}:`), 'Tab A did not create a document-specific writer');
  const savedA = await storedDraft(pageA, keyA);
  assert.deepEqual(Object.keys(savedA).sort(), ['base_revision', 'dirty', 'plan'], 'Document storage must remain a raw Draft');
  check(savedA.plan.timetable.courses[0].name === 'A retained', 'Tab A draft was not persisted');
  // Unlike context.newPage(), window.open copies the opener's sessionStorage.
  const popup = pageA.waitForEvent('popup');
  await pageA.evaluate(url => { window.open(url, '_blank'); }, origin + '/demo');
  const pageB = await popup;
  markPage(pageB, 'B (window.open duplicate)', 'hydrate copied draft and edit B');
  await pageB.waitForLoadState('load');
  await hydrated(pageB);
  const copied = await pageB.evaluate(() => window.__draftAtDocumentStart);
  check(Boolean(copied), `Duplicate snapshot missing: ${sanitizeDiagnostic(JSON.stringify(await pageB.evaluate(() => window.__draftSnapshotOrigin)))}`);
  check(copied.tab === idA && copied.pointer === keyA, 'Duplicate did not actually copy opener sessionStorage');
  await until(async () => await nameInput(pageB).inputValue() === 'A retained', 'Duplicate did not restore the copied draft pointer');
  await editName(pageB, 'B recovered latest');
  const keyB = await writerKey(pageB, sandboxKey);
  check(keyB !== keyA, 'Duplicate reused the opener document writer key');
  const savedB = await storedDraft(pageB, keyB);
  check(savedB.plan.timetable.courses[0].name === 'B recovered latest', 'Duplicate did not persist its independent edit');
  assert.deepEqual(await storedDraft(pageB, keyA), savedA, 'Duplicate edit clobbered opener draft key');
  check(await nameInput(pageA).inputValue() === 'A retained', 'Duplicate edit changed opener UI');
  markPage(pageA, 'A', 'reload opener while duplicate stays open');
  await pageA.reload();
  await hydrated(pageA);
  await until(async () => await nameInput(pageA).inputValue() === 'A retained', 'Reloaded opener lost its retained plan');
  check(await tabId(pageA) === idA, 'Reload changed the stable tab ID');
  const reloadedKeyA = await writerKey(pageA, sandboxKey);
  check(reloadedKeyA !== keyA && reloadedKeyA !== keyB, 'Reload reused an existing document writer key');
  assert.deepEqual(await storedDraft(pageA, keyA), savedA, 'Reload overwrote archived opener draft');
  assert.deepEqual(await storedDraft(pageA, keyB), savedB, 'Reload overwrote duplicate draft');
  // Re-edit B after A's reload so B is unambiguously the most recent draft.
  // Use its timestamp companion as a barrier, not an arbitrary delay.
  await until(async () => {
    const updated = await pageA.evaluate(keys => keys.map(key => Number(localStorage.getItem(`${key}:updated`))), [reloadedKeyA, keyB]);
    return updated.every(Number.isFinite) && updated.every(value => value > 0) && Date.now() > Math.max(...updated);
  }, 'Document draft timestamp companions were not recorded');
  markPage(pageB, 'B (window.open duplicate)', 'edit B after opener reload');
  await editName(pageB, 'B recovered latest after reload');
  const latestB = await storedDraft(pageB, keyB);
  check(latestB.plan.timetable.courses[0].name === 'B recovered latest after reload', 'Latest duplicate edit was not stored');
  check(await writerKey(pageB, sandboxKey) === keyB, 'A document changed writer namespace during editing');
  markPage(pageA, 'A', 'close opener');
  markPage(pageB, 'B (window.open duplicate)', 'close duplicate');
  await pageA.close();
  await pageB.close();
  // Keep the context alive: localStorage survives, but this fresh tab has no pointer.
  const pageC = await context.newPage();
  markPage(pageC, 'C (fresh sandbox)', 'auto-discover latest archived B draft');
  await hydrated(pageC, '/demo');
  const fresh = await pageC.evaluate(() => window.__draftAtDocumentStart);
  check(Boolean(fresh), `Fresh sandbox snapshot missing: ${sanitizeDiagnostic(JSON.stringify(await pageC.evaluate(() => window.__draftSnapshotOrigin)))}`);
  check(fresh.tab === null && fresh.pointer === null, 'Recovery tab inherited sessionStorage instead of starting fresh');
  await until(async () => await nameInput(pageC).inputValue() === 'B recovered latest after reload', 'Fresh sandbox did not auto-discover the latest archived draft');
  const keyC = await writerKey(pageC, sandboxKey);
  check(![keyA, reloadedKeyA, keyB].includes(keyC), 'Fresh sandbox reused an archived writer key');
  const shelf = pageC.locator('details').filter({ has: pageC.locator('summary', { hasText: /^Device drafts$/ }) });
  await shelf.waitFor({ state: 'visible' });
  if (await shelf.getAttribute('open') === null) await shelf.locator('summary').click();
  markPage(pageC, 'C (fresh sandbox)', 'open Device drafts and request restore A');
  const archivedA = shelf.getByTestId('device-draft').filter({ hasText: 'A retained' }).first();
  await archivedA.waitFor({ state: 'visible' });
  await archivedA.getByRole('button', { name: 'Restore draft', exact: true }).click();
  check(await nameInput(pageC).inputValue() === 'B recovered latest after reload', 'Shelf restore changed plan before confirmation');
  markPage(pageC, 'C (fresh sandbox)', 'confirm Restore draft A and refresh shelf');
  await archivedA.getByRole('group', { name: 'Restore draft', exact: true })
    .getByRole('button', { name: 'Confirm Restore draft', exact: true }).click();
  await until(async () => await nameInput(pageC).inputValue() === 'A retained', 'Confirmed shelf restore did not recover archived A');
  check((await ownDraft(pageC, sandboxKey)).plan.timetable.courses[0].name === 'A retained', 'Shelf restore did not persist to the current document');
  check(await writerKey(pageC, sandboxKey) === keyC, 'Shelf restore adopted an archived writer key');
  assert.deepEqual(await storedDraft(pageC, keyA), savedA, 'Shelf restore modified archived A');
  assert.deepEqual(await storedDraft(pageC, keyB), latestB, 'Shelf restore modified archived B');
  check(await legacyRaw(pageC, sandboxKey) === null, 'Document writes created a legacy sandbox base');
  check(cloudRequests === 0, 'Sandbox recovery contacted a cloud API');
  clean(faults);
  await close(context);
}
// Account fixtures mirror the server's explicit export allowlist, not AuthUser.
async function accountMock(options = {}) {
  const result = await mocked(options);
  const { context, state, faults } = result;
  state.exports = [];
  state.deletions = [];
  state.deletionStatus = 202;
  state.exportFixture = {
    version: 1, generated_at: '2026-11-21T10:00:00Z',
    profile: { user_id: user.user_id, email: user.email, display_name: user.display_name },
    plans: [{ plan: clone(sample), revision: 1, updated_at: '2026-11-21T09:00:00Z' }],
    ai_usage: [{ day: '2026-11-21', kind: 'helper', used: 2 }],
  };
  await context.route('**/api/account/**', async route => {
    try {
      const request = route.request();
      const pathname = new URL(request.url()).pathname;
      if (pathname === '/api/account/export' && request.method() === 'GET') {
        state.exports.push({ headers: request.headers(), body: request.postData() });
        await route.fulfill({ status: 200, json: state.exportFixture });
      } else if (pathname === '/api/account/deletion' && request.method() === 'POST') {
        state.deletions.push({ headers: request.headers(), body: request.postDataJSON() });
        if (state.deletionStatus === 202) state.deleted = true;
        await route.fulfill({ status: state.deletionStatus, json: state.deleted
          ? { status: 'pending_identity_deletion', app_data_deleted: true }
          : { error: { message: 'Sign in again: an interactive login within the last 5 minutes is required.' } } });
      } else {
        state.unexpected++;
        await route.fulfill({ status: 500, json: {} });
      }
    } catch {
      faults.routeErrors++;
      await route.abort().catch(() => {});
    }
  });
  return result;
}
async function downloadedJson(page, button) {
  const downloading = page.waitForEvent('download');
  await button.click();
  const download = await downloading;
  check(download.suggestedFilename().endsWith('.json'), 'Account/device export must download JSON');
  check(await download.failure() === null, 'JSON download failed');
  const chunks = [];
  for await (const chunk of await download.createReadStream()) chunks.push(chunk);
  return JSON.parse(Buffer.concat(chunks).toString('utf8'));
}
async function accountPage(page) {
  await page.getByRole('button', { name: 'Settings / Account', exact: true }).click();
  await text(page.getByRole('heading', { level: 1 }), 'Account & privacy');
}
async function legalPages() {
  const ssrResult = await newContext({ javaScriptEnabled: false });
  for (const [route, heading] of [['/privacy', 'Your data & privacy'], ['/terms', 'Using this planning tool']]) {
    await ssrResult.page.goto(origin + route);
    await text(ssrResult.page.getByRole('heading', { level: 1 }), heading);
    await text(ssrResult.page.locator('main'), 'operator review required before publishing');
    await text(ssrResult.page.locator('main'), 'Support contact must be configured before launch');
    await text(ssrResult.page.locator('main'), 'not an official college service');
    check(await ssrResult.page.locator('meta[name="support-email"]').count() === 0, 'Credential-free SSR inherited a support address');
    check(!/legally compliant|guaranteed compliance|GDPR compliant/i.test(await ssrResult.page.locator('main').innerText()), 'Draft policy claims legal compliance');
  }
  clean(ssrResult.faults);
  await close(ssrResult.context);

  const { context, page, faults } = await newContext();
  const requests = [];
  let releaseRuntime;
  const contact = 'support+browser@example.invalid';
  await context.addInitScript(() => {
    window.__legalDraftAccesses = [];
    for (const method of ['getItem', 'setItem', 'removeItem', 'key']) {
      const original = Storage.prototype[method];
      Storage.prototype[method] = function (...args) {
        if (method === 'key' || String(args[0]).startsWith('vitattendance:')) window.__legalDraftAccesses.push(method);
        return original.apply(this, args);
      };
    }
  });
  await context.route('**/api/**', async route => {
    const request = route.request();
    requests.push(`${request.method()} ${new URL(request.url()).pathname}`);
    if (new URL(request.url()).pathname !== '/api/runtime' || request.method() !== 'GET') {
      return route.fulfill({ status: 500, json: {} });
    }
    const email = contact;
    await new Promise(resolve => { releaseRuntime = resolve; });
    await route.fulfill({ json: { mode: 'local', auth_available: true, local_password_enabled: false, ai_enabled: false, support_email: email } });
  });
  for (const [route, heading] of [['/privacy', 'Your data & privacy'], ['/terms', 'Using this planning tool']]) {
    releaseRuntime = null;
    await hydrated(page, route);
    await until(() => Boolean(releaseRuntime), 'Public page did not request runtime');
    // Runtime/auth are still unresolved: legal text must already be public.
    await text(page.getByRole('heading', { level: 1 }), heading);
    await text(page.locator('.support-contact').first(), 'Support contact must be configured before launch');
    releaseRuntime();
    await text(page.locator('.support-contact').first(), contact);
    check(await page.locator('.support-contact a').count() > 0, 'Configured support contact has no mail link');
    for (const link of await page.locator('.support-contact a').all()) {
      check(await link.textContent() === contact, 'Support address was not rendered as literal text');
      check(await link.getAttribute('href') === `mailto:${contact}`, 'Support address has unsafe/unexpected href');
    }
    assert.deepEqual(await page.evaluate(() => window.__legalDraftAccesses), [], 'Public policy boot accessed device drafts');
  }
  assert.deepEqual(requests, ['GET /api/runtime', 'GET /api/runtime'], 'Public legal pages contacted private APIs');
  clean(faults);
  await close(context);
}
async function sandboxAccount() {
  const { context, page, faults } = await newContext();
  const api = [];
  context.on('request', request => {
    const pathname = new URL(request.url()).pathname;
    if (pathname.startsWith('/api/') || pathname.startsWith('/auth/')) api.push(`${request.method()} ${pathname}`);
  });
  await hydrated(page, '/demo');
  await until(async () => Boolean(await draft(page, sandboxKey)), 'Sandbox draft missing before account navigation');
  await accountPage(page);
  check(new URL(page.url()).pathname === '/demo/account', 'Sandbox account route lost demo mode');
  await text(page.locator('main'), 'Device-only sandbox');
  check(await page.getByRole('button', { name: 'Download my app data', exact: true }).count() === 0, 'Sandbox exposes private account export');
  check(await page.getByRole('button', { name: 'Request account deletion unavailable in sandbox', exact: true }).isDisabled(), 'Sandbox enables account deletion');
  assert.deepEqual(await downloadedJson(page, page.getByRole('button', { name: 'Export device draft', exact: true })), sample, 'Sandbox account export is not the device plan');
  await remainsUnchanged(() => api.filter(request => request !== 'GET /api/runtime'), [], 'Sandbox settings contacted a private endpoint');
  clean(faults);
  await close(context);
}
async function accountExportAndRejections() {
  const result = await accountMock();
  const { context, page, state } = result;
  await loaded(page);
  await accountPage(page);
  await text(page.locator('.account-profile'), user.email);
  await text(page.locator('.account-profile'), user.display_name);
  const before = await page.evaluate(() => Object.fromEntries(Object.entries(localStorage)));
  const exported = await downloadedJson(page, page.getByRole('button', { name: 'Download my app data', exact: true }));
  assert.deepEqual(exported, state.exportFixture, 'Account export must contain exactly the server allowlisted fixture, not client auth/device state');
  check(!/csrf|token|secret|password/i.test(JSON.stringify(exported)), 'Account export leaked credentials');
  check(state.exports.length === 1 && state.exports[0].body === null, 'Account export was not a bodyless GET');
  check(state.exports[0].headers['x-session-user'] === user.user_id, 'Account export omitted session identity');
  check(!state.exports[0].headers['x-csrf-token'], 'GET account export sent CSRF token');
  const phrase = page.getByRole('textbox', { name: 'Deletion confirmation', exact: true });
  const request = page.getByRole('button', { name: 'Request account deletion', exact: true });
  for (const wrong of ['', 'delete my account', 'DELETE MY ACCOUNT ']) {
    await phrase.fill(wrong);
    check(await request.isDisabled(), 'Wrong deletion phrase enabled deletion');
  }
  await remainsUnchanged(() => state.deletions.length, 0, 'Wrong phrase sent a deletion request', 250);
  await phrase.fill('DELETE MY ACCOUNT');
  await request.click();
  check(state.deletions.length === 0, 'Exact phrase bypassed second confirmation');
  await page.getByRole('group', { name: 'Request account deletion', exact: true }).getByRole('button', { name: 'Cancel', exact: true }).click();
  check(state.deletions.length === 0, 'Cancel sent deletion request');
  state.deletionStatus = 400;
  await confirm(page, 'Request account deletion');
  await text(page.locator('.notice'), 'interactive login within the last 5 minutes is required');
  assert.deepEqual(state.deletions[0].body, { confirmation: 'DELETE MY ACCOUNT' }, 'Deletion payload must contain only exact confirmation');
  check(state.deletions[0].headers['x-session-user'] === user.user_id, 'Deletion omitted session identity');
  check(state.deletions[0].headers['x-csrf-token'] === 'TEST', 'POST deletion omitted CSRF');
  await text(page.locator('.account-profile'), user.email);
  assert.deepEqual(await page.evaluate(() => Object.fromEntries(Object.entries(localStorage))), before, 'Recent-login rejection changed device cache');
  check(!state.deleted && state.puts.length === 0, 'Rejected deletion changed account state');
  state.deletionStatus = 503;
  await phrase.fill('DELETE MY ACCOUNT');
  await confirm(page, 'Request account deletion');
  await text(page.locator('.notice'), 'Could not confirm the deletion outcome');
  await text(page.locator('.notice'), 'the server may already have accepted the request');
  check(!/no data (was )?changed|nothing (was )?deleted|saved plan has not been replaced/i.test((await page.locator('.notice, main').allInnerTexts()).join('\n')), 'Uncertain deletion falsely guarantees no data changed');
  assert.deepEqual(await page.evaluate(() => Object.fromEntries(Object.entries(localStorage))), before, 'Uncertain deletion silently removed local cache');
  await text(page.locator('.account-profile'), user.email);
  check(state.deletions.length === 2, 'Deletion rejection/uncertainty requests were not exactly once');
  contractClean(result);
  await close(context);
}
async function accountDeletionAcrossTabs() {
  const result = await accountMock({ holdFirst: true, putStatus: 503 });
  const { context, page: pageA, state } = result;
  const marker = `vitattendance:v1:account-deleted:${user.user_id}`;
  const prefix = `vitattendance:v1:draft:${user.user_id}:`;
  const pageB = await context.newPage();
  // Delay delivery to model a frozen document resuming its in-flight callback
  // before its queued storage event. The original event, not a fake server, is captured.
  await pageB.addInitScript(marker => {
    window.__queuedDeletion = null;
    window.addEventListener('storage', event => {
      if (event.key === marker && !window.__deliverDeletion) {
        window.__queuedDeletion = { key: event.key, newValue: event.newValue, oldValue: event.oldValue, url: event.url };
        event.stopImmediatePropagation();
      }
    });
  }, marker);
  await loaded(pageA);
  await loaded(pageB);
  check(state.me === 2, 'Both deletion tabs must first authenticate as the same fake user');
  await editName(pageB, 'Frozen tab unsaved edit');
  await until(() => state.puts.length === 1 && Boolean(state.release), 'Delayed save was not in flight before deletion');
  const preserved = {
    'vitattendance:v1:draft:browser-contract-user-b:winter-2026:archive': 'other-account-byte-for-byte',
    [`${sandboxKey}:archive`]: 'sandbox-byte-for-byte',
  };
  await pageA.evaluate(({ prefix, marker, preserved, sample }) => {
    const cached = JSON.stringify({ plan: sample, base_revision: 1, dirty: true });
    localStorage.setItem(`${prefix}summer-2025:archived-tab:old-document`, cached);
    localStorage.setItem(`${prefix}summer-2025:archived-tab:old-document:updated`, '123');
    for (const [key, value] of Object.entries(preserved)) localStorage.setItem(key, value);
    window.__deletionStorageOrder = [];
    for (const method of ['setItem', 'removeItem']) {
      const original = Storage.prototype[method];
      Storage.prototype[method] = function (key, ...args) {
        if (this === localStorage && (key === marker || key.startsWith(prefix))) window.__deletionStorageOrder.push({ method, key, marked: localStorage.getItem(marker) !== null });
        return original.call(this, key, ...args);
      };
    }
  }, { prefix, marker, preserved, sample });
  await accountPage(pageA);
  await pageA.getByRole('textbox', { name: 'Deletion confirmation', exact: true }).fill('DELETE MY ACCOUNT');
  await confirm(pageA, 'Request account deletion');
  await text(pageA.getByTestId('welcome'), 'Supabase identity deletion is pending operator review');
  await text(pageA.getByTestId('welcome'), 'your Google account was not deleted');
  await until(() => pageB.evaluate(() => Boolean(window.__queuedDeletion)), 'Other tab did not receive real deletion storage event');
  const order = await pageA.evaluate(() => window.__deletionStorageOrder);
  check(order[0]?.method === 'setItem' && order[0].key === marker, 'Deletion tombstone must be durable before draft removals');
  check(order.some(entry => entry.method === 'removeItem') && order.filter(entry => entry.method === 'removeItem').every(entry => entry.marked), 'Account draft removal occurred without durable tombstone');
  const ownedKeys = () => pageA.evaluate(prefix => Object.keys(localStorage).filter(key => key.startsWith(prefix)), prefix);
  assert.deepEqual(await ownedKeys(), [], 'Deletion retained owned drafts, including archived semesters');
  check(await pageA.evaluate(marker => localStorage.getItem(marker) !== null, marker), 'Deletion tombstone was removed');
  state.release();
  await until(() => state.active === 0, 'Delayed PUT 503 callback did not complete');
  await remainsUnchanged(ownedKeys, [], 'Frozen tab resurrected cached draft after deletion tombstone');
  // Resume the queued cross-tab notification only after the stale callback ran.
  await pageB.evaluate(() => {
    window.__deliverDeletion = true;
    window.dispatchEvent(new StorageEvent('storage', { ...window.__queuedDeletion, storageArea: localStorage }));
  });
  await text(pageB.getByTestId('welcome'), 'identity deletion remains pending operator review');
  await remainsUnchanged(() => state.puts.length, 1, 'Deleted account continued saving after cross-tab reset');
  assert.deepEqual(await ownedKeys(), [], 'Cross-tab cleanup recreated account drafts');
  assert.deepEqual(await pageA.evaluate(keys => Object.fromEntries(keys.map(key => [key, localStorage.getItem(key)])), Object.keys(preserved)), preserved, 'Deletion touched other-account or sandbox records');
  check(state.deleted && state.deletions.length === 1, 'Deletion was not accepted exactly once');
  assert.deepEqual(state.deletions[0].body, { confirmation: 'DELETE MY ACCOUNT' });
  // Fresh /me now returns 401, not a resurrected fake session.
  await hydrated(pageB, '/account');
  await text(pageB.getByTestId('welcome'), 'attendance anxiety');
  check(state.me === 3, 'Deleted account reload did not check the now-revoked fake session');
  assert.deepEqual(await ownedKeys(), [], 'Reload read cached drafts after account deletion');
  contractClean(result);
  await close(context);
}

async function brokenBoot() {
  const { context, page, faults } = await newContext();
  let aborted = 0;
  await context.route('**/app_bg.wasm*', route => { aborted++; return route.abort('failed'); });
  await page.goto(origin + '/demo');
  await text(page.getByTestId('boot-error'), 'interactive planner could not start');
  await text(page.getByRole('heading', { level: 1 }), 'Your timetable & attendance');
  check(await page.getByRole('link', { name: 'Reload page', exact: true }).isVisible(), 'Boot failure has no recovery link');
  check(await page.getByRole('link', { name: 'Sign in instead', exact: true }).isVisible(), 'Boot failure removed meaningful SSR navigation');
  check(await page.evaluate(() => document.documentElement.dataset.hydrated !== 'true'), 'Failed boot claims successful hydration');
  check(aborted === 1, 'WASM failure test did not exercise asset loading');
  clean(faults, { brokenWasm: true });
  await close(context);
}
async function run() {
  await scenario('preview startup (credential-free native binary)', start);
  await scenario('preview: meaningful SSR with JavaScript disabled', ssr);
  await scenario('preview: hydration, leave math, persistence, validation, confirmations, JSON export', preview);
  await scenario('preview: mobile layout and keyboard helper', mobile);
  await scenario('preview: delayed runtime restores sandbox before editing; legacy base stays read-only', delayedRuntimeDraft);
  await scenario('preview: actual duplicated tab has independent writer; fresh sandbox and shelf recover archives', duplicatedSandboxRecovery);
  await scenario('browser contract mocks (NOT actual auth): debounce and serialized revisions/headers/quotas', saves);
  await scenario('browser contract mocks (NOT actual auth): 409 draft survives repeated reload without PUT', conflict);
  await scenario('browser contract mocks (NOT actual auth): initial GET 503 and retry preserve draft', failedLoad);
  await scenario('browser contract mocks (NOT actual auth): PUT 401 expires and stops writes', expired);
  await scenario('browser contract mocks (NOT actual auth): GET 503, CSRF refresh, reload and dirty sign-out safety', failedLoadSessionRefresh);
  await scenario('browser contract mocks (NOT actual auth): two-tab delayed GET draft isolation and no failed-PUT loop', twoTabDraftIsolation);
  await scenario('preview: public privacy/terms SSR and runtime-only hydration without draft access', legalPages);
  await scenario('preview: sandbox account settings export only the device plan; private endpoints unavailable', sandboxAccount);
  await scenario('browser contract mocks (NOT actual auth): account JSON export, exact confirmation, recent-login rejection and uncertain deletion', accountExportAndRejections);
  await scenario('browser contract mocks (NOT actual auth): cross-tab deletion tombstone blocks delayed save resurrection and preserves other accounts', accountDeletionAcrossTabs);
  await scenario('preview: aborted WASM shows recoverable boot error, not blank page', brokenBoot);
}
async function cleanup() {
  // Closing contexts also cancels delayed route handlers and pending downloads.
  await Promise.allSettled([...contexts].map(context => context.close()));
  contexts.clear();
  if (browser) await browser.close().catch(() => {});
  if (child && child.exitCode === null && child.signalCode === null) {
    const exited = once(child, 'exit').catch(() => {});
    child.kill('SIGTERM');
    await Promise.race([exited, sleep(1500)]);
    if (child.exitCode === null && child.signalCode === null) {
      child.kill('SIGKILL');
      await Promise.race([exited, sleep(1000)]);
    }
  }
}
(async () => {
  let passed = false;
  try {
    await bounded(run(), 220000, 'Overall browser suite exceeded 220 seconds');
    passed = true;
  } catch (error) {
    // Keep the headline concise; print bounded, sanitized browser diagnostics separately.
    const message = error instanceof assert.AssertionError ? error.message :
      /Timeout|timeout/i.test(error.message) ? 'Browser operation timed out' : error.message;
    console.error(`FAIL ${stage}: ${sanitizeDiagnostic(message, 240).split('\n')[0]}`);
    printDiagnostics(currentFaults);
    process.exitCode = 1;
  } finally {
    // Hard bound even if Playwright's transport stops responding during cleanup.
    const hardStop = setTimeout(() => {
      if (child && child.exitCode === null) child.kill('SIGKILL');
      console.error('FAIL cleanup: shutdown exceeded 6 seconds');
      process.exit(1);
    }, 6000);
    try { await cleanup(); } finally {
      clearTimeout(hardStop);
      process.removeListener('SIGINT', onSignal);
      process.removeListener('SIGTERM', onSignal);
    }
  }
  if (passed) console.log('PASS browser suite; auth coverage is mocked browser contract only (no actual DB/auth tested).');
})();
