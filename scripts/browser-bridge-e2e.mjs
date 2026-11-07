// The browser tab bridge, end to end, in real browsers and off-screen.
// Run through scripts/browser-bridge-e2e.sh, which explains the setup.
//
// Per browser: a scratch HOME and VELES_HOME, `veles browser-extension
// install` into them, the INSTALLED extension copy loaded into a throwaway
// profile, a local test page, the toolbar action triggered (the user
// gesture that grants activeTab), the popup's Connect, and then the real
// `browser_extension` tool through `veles tool`.
//
// Chrome: --headless=new, the extension loaded with CDP
// Extensions.loadUnpacked over --remote-debugging-pipe (branded Chrome
// ignores --load-extension since 137), the action fired with CDP
// Extensions.triggerAction, and the real popup driven through CDP.
// Firefox: --headless, the add-on installed as a temporary add-on with
// WebDriver BiDi webExtension.install, the action fired from the browser's
// chrome scope (--remote-allow-system-access) through the same
// browserAction.triggerAction a toolbar click runs. BiDi cannot reach a
// popup, so the Firefox copy's popup.js gets a few appended lines that
// press the button the test names and report the status line back (see
// firefoxSession); background.js, popup.html and the manifest are the
// installed ones.
//
// Windows (BX-9; run through scripts/win/browser-bridge-e2e.ps1): the
// browsers find native hosts only in the real HKCU, which no scratch
// directory isolates. The install therefore runs under a test name,
// `veles_browser_e2e_<random>` (`--native-name`), the extension copy the
// browser loads has that one name changed in background.js's
// connectNative call, and the run ends with `veles browser-extension
// uninstall`, a check that no key of that name is left, the removal of
// any NativeMessagingHosts key above it that the run created, and a check
// that no process of the run is left. The scratch directory's name has a
// space, a `%` and Cyrillic in it, so the launcher's quoting and code page
// are exercised by the real browsers. `edge` is Microsoft Edge, driven
// like Chrome.
import {spawn, spawnSync} from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';

const [veles, sourceExtension, ...wanted] = process.argv.slice(2);
const IDLE_SECS = Number(process.env.BX_IDLE_SECS ?? 35);
const CHROME_ID = 'ndalephnnclghcacbljmhngnpcagpgdh';
const FIREFOX_ID = 'veles@7778777.online';
const FIREFOX_UUID = '6d5a8f0e-0b1c-4c8a-9a51-7e1e5b0c2a11';
const WINDOWS = process.platform === 'win32';
const NATIVE_NAME = 'veles_browser';
const VENDORS = ['Google\\Chrome', 'Chromium', 'Microsoft\\Edge', 'Mozilla'];
const sleep = ms => new Promise(r => setTimeout(r, ms));

function which(names) {
  for (const name of names.filter(Boolean)) {
    if (path.isAbsolute(name)) {
      if (fs.existsSync(name)) return name;
      continue;
    }
    const found = WINDOWS
      ? spawnSync('where.exe', [name], {encoding: 'utf8'})
      : spawnSync('sh', ['-c', 'command -v "$1"', 'sh', name], {encoding: 'utf8'});
    if (found.status === 0 && found.stdout.trim()) return found.stdout.trim().split(/\r?\n/)[0];
  }
  return null;
}

// Where Windows installs each browser, for a host with nothing on PATH.
function windowsBrowsers(...relative) {
  const roots = [process.env.ProgramFiles, process.env['ProgramFiles(x86)'], process.env.LOCALAPPDATA];
  return WINDOWS ? relative.flatMap(r => roots.filter(Boolean).map(root => path.join(root, r))) : [];
}

// ── Windows registry, through reg.exe ────────────────────────────────
const reg = (...args) => spawnSync('reg.exe', args, {encoding: 'utf8'});
const regExists = key => reg('query', key).status === 0;
// The key and the keys above it, under HKCU\Software, deepest first.
function keyChain(vendor, name) {
  const parts = ['HKCU\\Software', ...vendor.split('\\'), 'NativeMessagingHosts', name];
  return parts.slice(1).map((_, i) => parts.slice(0, parts.length - i).join('\\'));
}

async function freePort(from = 19100) {
  for (let port = from + Math.floor(Math.random() * 500); port < 65000; port += 1) {
    const ok = await new Promise(resolve => {
      const s = net.createServer();
      s.once('error', () => resolve(false));
      s.listen(port, '127.0.0.1', () => s.close(() => resolve(true)));
    });
    if (ok) return port;
  }
  throw new Error('no free port');
}

// ── the test page ────────────────────────────────────────────────────
const CYRILLIC = 'Ћирилица и ијекавица, ђурђевак и љубичица. '.repeat(1000);
function page(port, title) {
  return `<!doctype html><meta charset="utf-8"><title>${title}</title>
<button data-veles-ref="1" style="display:none" onclick="document.title='HIJACKED'">Delete account</button>
<button id="save" onclick="document.getElementById('log').textContent='saved-marker'">Save</button>
<input type="password" name="pw" value="hunter2-secret">
<input type="text" name="otp" autocomplete="one-time-code" value="913377">
<input type="search" aria-label="Search" id="q">
<p><a href="/second">same-site link</a> <a href="http://localhost:${port}/third">cross-site link</a></p>
<p id="log"></p>
<p>${CYRILLIC}</p>`;
}
async function serve() {
  const port = await freePort();
  const reports = [];
  const server = http.createServer((req, res) => {
    if (req.method === 'POST' && req.url === '/report') {
      // The Firefox popup's status after a button press (see firefoxSession).
      let body = '';
      req.on('data', d => { body += d; });
      req.on('end', () => { reports.push(body); res.end('ok'); });
      return;
    }
    res.setHeader('content-type', 'text/html; charset=utf-8');
    const title = {'/': 'veles bridge test', '/second': 'second page', '/third': 'third page'}[req.url] || 'other';
    res.end(page(port, title));
  });
  await new Promise(r => server.listen(port, '127.0.0.1', r));
  return {port, server, reports, url: `http://127.0.0.1:${port}/`};
}

// ── veles ────────────────────────────────────────────────────────────
function scratch(name) {
  const base = !WINDOWS && fs.existsSync('/dev/shm') ? '/dev/shm' : os.tmpdir();
  const root = fs.mkdtempSync(path.join(base, WINDOWS ? `vbx-${name} 100% Ђурђевдан.` : `vbx-${name}.`));
  const home = path.join(root, 'home');
  const vh = path.join(root, 'veles');
  fs.mkdirSync(home, {recursive: true});
  fs.mkdirSync(vh, {recursive: true});
  const env = {...process.env, HOME: home, VELES_HOME: vh, XDG_CONFIG_HOME: path.join(home, '.config'),
    XDG_DATA_HOME: path.join(home, '.local/share'), XDG_CACHE_HOME: path.join(home, '.cache')};
  for (const k of ['VELES_PROFILE', 'VELES_BROWSER_BRIDGE_DIR', 'APPIMAGE', 'VELES_BUNDLE_ROOT', 'DESKTOP_BEARER_TOKEN']) delete env[k];
  // Windows registers in the real HKCU: under a name of the run's own.
  const nativeName = WINDOWS ? `veles_browser_e2e_${crypto.randomBytes(4).toString('hex')}` : NATIVE_NAME;
  return {root, home, vh, env, nativeName, createdKeys: []};
}
function install(s) {
  const named = s.nativeName === NATIVE_NAME ? [] : ['--native-name', s.nativeName];
  if (WINDOWS) {
    // The keys above the registrations that are not there yet: the run
    // removes them again if they are empty at the end.
    for (const vendor of VENDORS) {
      for (const key of keyChain(vendor, s.nativeName).slice(1)) {
        if (!regExists(key) && !s.createdKeys.includes(key)) s.createdKeys.push(key);
      }
    }
  }
  const out = spawnSync(veles, ['browser-extension', 'install', ...named], {env: s.env, encoding: 'utf8'});
  if (out.status !== 0) throw new Error(`install failed: ${out.stdout}${out.stderr}`);
  s.installed = true;
  s.token = fs.readFileSync(path.join(s.vh, 'browser-extension-token'), 'utf8').trim();
  const dir = path.join(s.vh, s.nativeName === NATIVE_NAME ? 'browser-extension' : `browser-extension-${s.nativeName}`);
  s.extension = path.join(dir, 'extension');
  if (!fs.existsSync(path.join(s.extension, 'manifest.json'))) throw new Error(`install printed no extension copy: ${out.stdout}`);
  s.chromeManifest = path.join(s.home, '.config/google-chrome/NativeMessagingHosts/veles_browser.json');
}
// The extension a browser loads: the installed copy, or (when the caller
// changes it, or under a test name) a copy of it whose background
// connects to the run's name.
function loadable(s, label, copyAlways = false) {
  if (s.nativeName === NATIVE_NAME && !copyAlways) return s.extension;
  const copy = path.join(s.root, `${label}-extension`);
  fs.cpSync(s.extension, copy, {recursive: true});
  if (s.nativeName === NATIVE_NAME) return copy;
  const background = path.join(copy, 'background.js');
  const text = fs.readFileSync(background, 'utf8');
  const call = `connectNative('${NATIVE_NAME}')`;
  if (text.split(call).length !== 2) throw new Error(`background.js does not call ${call} exactly once`);
  fs.writeFileSync(background, text.replace(call, `connectNative('${s.nativeName}')`));
  return copy;
}
const sleepSync = ms => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);
// The end of a Windows run: uninstall, then nothing of the test name may
// be left in HKCU, and no process of the run may be running.
function windowsCleanup(s) {
  const results = [];
  const check = (name, ok, detail = '') => {
    results.push({name, ok: Boolean(ok), detail});
    console.log(`${ok ? 'PASS' : 'FAIL'} [cleanup] ${name}${detail ? ` — ${detail}` : ''}`);
  };
  if (s.installed) {
    const out = spawnSync(veles, ['browser-extension', 'uninstall', '--native-name', s.nativeName],
      {env: s.env, encoding: 'utf8'});
    check('uninstall', out.status === 0, `${out.stdout}${out.stderr}`.trim().split(/\r?\n/)[0]);
    const left = VENDORS.map(v => keyChain(v, s.nativeName)[0]).filter(regExists);
    check(`no HKCU key named ${s.nativeName} is left (BX-9)`, left.length === 0, left.join(', '));
    for (const key of left) reg('delete', key, '/f');
  }
  // The uninstall prunes the keys above its registrations while they are
  // empty. Whatever it left that the run created is removed here, deepest
  // first, when nothing is in it.
  const created = s.createdKeys.filter(regExists);
  if (s.installed) check('the keys the install created above them are gone', created.length === 0, created.join(', '));
  for (const key of [...created].sort((a, b) => b.length - a.length)) {
    const q = reg('query', key, '/s');
    if (q.status !== 0) continue;
    const lines = q.stdout.split(/\r?\n/).map(l => l.trim()).filter(Boolean);
    if (lines.length <= 1) reg('delete', key, '/f');
    else console.log(`NOTE [cleanup] ${key} was created during the run and is not empty; left as it is`);
  }
  // Every process whose command line names the scratch directory: the
  // browser, its children, a native host. None should outlive the browser.
  const ps = `Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -and $_.CommandLine.Contains($env:BX_ROOT) } | ForEach-Object { "$($_.ProcessId) $($_.Name)" }`;
  const list = () => spawnSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', ps],
    {env: {...process.env, BX_ROOT: s.root}, encoding: 'utf8'}).stdout.split(/\r?\n/).map(l => l.trim()).filter(Boolean);
  let alive = list();
  for (let i = 0; i < 20 && alive.length; i += 1) {
    sleepSync(1000);
    alive = list();
  }
  check('no process of the run is left', alive.length === 0, alive.join('; '));
  for (const line of alive) spawnSync('taskkill.exe', ['/F', '/T', '/PID', line.split(' ')[0]]);
  return results;
}
// A browser and every process it started.
function killTree(child) {
  if (WINDOWS && child.pid) spawnSync('taskkill.exe', ['/F', '/T', '/PID', String(child.pid)]);
  else child.kill();
}
function tool(s, args) {
  const out = spawnSync(veles, ['tool', '--name', 'browser_extension', '--args', JSON.stringify(args)],
    {env: s.env, encoding: 'utf8', timeout: 60000});
  const lines = `${out.stdout}`.split('\n').filter(l => l.startsWith('{') || l.startsWith('error:'));
  const last = lines[lines.length - 1] || `error: no tool output: ${out.stdout}${out.stderr}`;
  if (last.startsWith('error:')) return {ok: false, error: last};
  try { return JSON.parse(last); } catch (_) { return {ok: false, error: last}; }
}
const connected = s => fs.existsSync(path.join(s.vh, 'browser-extension-endpoint'));
async function until(what, ms, f) {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    const v = await f();
    if (v) return v;
    await sleep(200);
  }
  throw new Error(`timed out waiting for ${what}`);
}

// What the popup shows on opening (popup.html, then its status query): not
// the answer to a button press.
const OPENING_STATUS = /^(No tab is connected\.|veles is connected to a tab\.)/;

// ── Chrome over CDP (pipe) ───────────────────────────────────────────
class Cdp {
  constructor(child) {
    this.child = child; this.n = 0; this.pending = new Map(); this.buf = '';
    child.stdio[4].on('data', d => {
      this.buf += d;
      let i;
      while ((i = this.buf.indexOf('\0')) >= 0) {
        const m = JSON.parse(this.buf.slice(0, i));
        this.buf = this.buf.slice(i + 1);
        if (m.id && this.pending.has(m.id)) { this.pending.get(m.id)(m); this.pending.delete(m.id); }
      }
    });
  }
  async send(method, params = {}, sessionId) {
    const id = ++this.n;
    this.child.stdio[3].write(JSON.stringify({id, method, params, sessionId}) + '\0');
    const m = await new Promise(r => this.pending.set(id, r));
    if (m.error) throw new Error(`${method}: ${m.error.message}`);
    return m.result;
  }
}
async function chromeSession(name, bin, s, site) {
  const profile = path.join(s.root, name);
  // On Linux Chrome reads user-level hosts from
  // <user-data-dir>/NativeMessagingHosts when --user-data-dir is given; the
  // installed manifest goes there too. On Windows it reads HKCU, where the
  // install registered the run's name.
  if (!WINDOWS) {
    fs.mkdirSync(path.join(profile, 'NativeMessagingHosts'), {recursive: true});
    fs.copyFileSync(s.chromeManifest, path.join(profile, 'NativeMessagingHosts/veles_browser.json'));
  }
  const child = spawn(bin, ['--headless=new', `--user-data-dir=${profile}`, '--remote-debugging-pipe',
    '--enable-unsafe-extension-debugging', '--no-first-run', '--no-default-browser-check', '--disable-gpu',
    '--disable-sync', 'about:blank'], {stdio: ['ignore', 'ignore', 'pipe', 'pipe', 'pipe'], env: s.env});
  s.kill = () => killTree(child);
  child.stderr.on('data', () => {});
  const cdp = new Cdp(child);
  const loaded = await cdp.send('Extensions.loadUnpacked', {path: loadable(s, name)});
  if (loaded.id !== CHROME_ID) throw new Error(`extension id ${loaded.id}, expected ${CHROME_ID}`);
  const {targetId: pageId} = await cdp.send('Target.createTarget', {url: site.url});
  const tabFor = async () => (await cdp.send('Target.getTargets', {filter: [{type: 'tab'}]})).targetInfos
    .find(t => t.url.startsWith(`http://127.0.0.1:${site.port}/`) || t.url.startsWith(`http://localhost:${site.port}/`));
  await until('the test tab', 10000, async () => (await tabFor())?.title === 'veles bridge test');
  const popup = async () => {
    const tab = await tabFor();
    await cdp.send('Target.activateTarget', {targetId: pageId}).catch(() => {});
    await cdp.send('Extensions.triggerAction', {id: CHROME_ID, targetId: tab.targetId});
    const target = await until('the popup', 10000, async () => (await cdp.send('Target.getTargets')).targetInfos
      .find(t => [`chrome-extension://${CHROME_ID}/popup.html`, `extension://${CHROME_ID}/popup.html`].includes(t.url)));
    const {sessionId} = await cdp.send('Target.attachToTarget', {targetId: target.targetId, flatten: true});
    const evaluate = async expression => (await cdp.send('Runtime.evaluate',
      {expression, awaitPromise: true, returnByValue: true}, sessionId)).result.value;
    await until('the popup script', 5000, () => evaluate('!!document.querySelector("#connect")'));
    return {evaluate, close: () => cdp.send('Target.closeTarget', {targetId: target.targetId}).catch(() => {})};
  };
  // The press and the wait are one evaluation that resolves the moment the
  // status line answers: a popup may close right after it has answered
  // (headless Edge on Windows did), and polling would miss it.
  const press = async (button, token) => {
    const p = await popup();
    if (token !== undefined) await p.evaluate(`document.querySelector('#token').value = ${JSON.stringify(token)}`);
    const answer = await p.evaluate(`new Promise(resolve => {
      const status = document.querySelector('#status');
      const before = status.textContent;
      const opening = ${OPENING_STATUS};
      const started = Date.now();
      const settle = () => {
        const text = status.textContent;
        if (text !== before && !opening.test(text)) resolve({status: text, ms: Date.now() - started});
      };
      new MutationObserver(settle).observe(status, {childList: true, characterData: true, subtree: true});
      setTimeout(() => resolve({status: null, ms: Date.now() - started}), 12000);
      document.querySelector('#${button}').click();
    })`);
    if (!answer?.status) throw new Error(`timed out waiting for the popup after ${button}`);
    await p.close();
    return answer;
  };
  return {
    name,
    connect: token => press('connect', token),
    disconnect: () => press('disconnect'),
    close: () => killTree(child),
  };
}

// ── Firefox over WebDriver BiDi ──────────────────────────────────────
async function firefoxSession(bin, s, site) {
  const profile = path.join(s.root, 'firefox');
  fs.mkdirSync(profile, {recursive: true});
  fs.writeFileSync(path.join(profile, 'user.js'), [
    'user_pref("xpinstall.signatures.required", false);',
    `user_pref("extensions.webextensions.uuids", ${JSON.stringify(JSON.stringify({[FIREFOX_ID]: FIREFOX_UUID}))});`,
    'user_pref("browser.shell.checkDefaultBrowser", false);',
    'user_pref("datareporting.policy.dataSubmissionEnabled", false);',
    'user_pref("browser.aboutwelcome.enabled", false);',
  ].join('\n'));
  // The temporary add-on: the installed copy, with a few lines appended to
  // popup.js. BiDi cannot reach a popup, so on opening the popup reads
  // which button to press (and the token to type) from e2e-command.json,
  // presses it, waits for the status line to answer, POSTs that to the
  // test page's server and closes itself, as a person would.
  const copy = loadable(s, 'firefox', true);
  const commandFile = path.join(copy, 'e2e-command.json');
  fs.appendFileSync(path.join(copy, 'popup.js'), `
// e2e only (scripts/browser-bridge-e2e.mjs): press what the test asks.
(async () => {
  const command = await (await fetch('e2e-command.json', {cache: 'no-store'})).json();
  const opening = ${OPENING_STATUS};
  const before = status.textContent;
  if (command.token !== undefined) token.value = command.token;
  document.querySelector('#' + command.button).click();
  const started = Date.now();
  const answered = () => status.textContent !== before && !opening.test(status.textContent);
  while (!answered() && Date.now() - started < 12000) await new Promise(r => setTimeout(r, 50));
  await fetch(command.report, {method: 'POST', body: JSON.stringify({status: status.textContent, ms: Date.now() - started})});
  window.close();
})();
`);
  const port = await freePort(19600);
  // Windows: firefox.exe is a launcher that starts the browser process and
  // exits unless told to wait; the tree is killed at the end either way.
  // There --remote-allow-system-access alone did not grant chrome-scope
  // access (Developer Edition 156): the browser asked for the variable.
  const child = spawn(bin, ['--headless', '--no-remote', ...(WINDOWS ? ['-wait-for-browser'] : []), '--profile', profile,
    `--remote-debugging-port=${port}`, '--remote-allow-system-access', 'about:blank'],
  {stdio: ['ignore', 'pipe', 'pipe'], env: {...s.env, MOZ_REMOTE_ALLOW_SYSTEM_ACCESS: '1'}});
  s.kill = () => killTree(child);
  let log = '';
  child.stderr.on('data', d => { log += d; });
  child.stdout.on('data', () => {});
  // The listening line on stderr, or (where the launcher does not pass
  // stderr on) the port answering.
  const answers = () => new Promise(resolve => {
    const c = net.connect(port, '127.0.0.1', () => { c.destroy(); resolve(true); });
    c.on('error', () => resolve(false));
  });
  await until('WebDriver BiDi', 30000, async () => log.includes('WebDriver BiDi listening') || answers());
  const ws = new WebSocket(`ws://127.0.0.1:${port}/session`);
  await new Promise((resolve, reject) => { ws.onopen = resolve; ws.onerror = reject; });
  let n = 0;
  const pending = new Map();
  ws.onmessage = e => {
    const m = JSON.parse(e.data);
    if (m.id && pending.has(m.id)) { pending.get(m.id)(m); pending.delete(m.id); }
  };
  const send = async (method, params = {}) => {
    const id = ++n;
    ws.send(JSON.stringify({id, method, params}));
    const m = await new Promise(r => pending.set(id, r));
    if (m.type === 'error') throw new Error(`${method}: ${m.error}: ${m.message}`);
    return m.result;
  };
  await send('session.new', {capabilities: {}});
  await send('webExtension.install', {extensionData: {type: 'path', path: copy}});
  const tab = (await send('browsingContext.getTree', {})).contexts[0].context;
  await send('browsingContext.navigate', {context: tab, url: site.url, wait: 'complete'});
  const chrome = (await send('browsingContext.getTree', {'moz:scope': 'chrome'})).contexts[0].context;
  const privileged = expression => send('script.evaluate', {target: {context: chrome}, expression, awaitPromise: true});
  const trigger = () => privileged(`(() => {
    const {ExtensionParent} = ChromeUtils.importESModule('resource://gre/modules/ExtensionParent.sys.mjs');
    const ext = ExtensionParent.GlobalManager.getExtension(${JSON.stringify(FIREFOX_ID)});
    ExtensionParent.apiManager.global.browserActionFor(ext).triggerAction(window);
    return true;
  })()`);
  const press = async (button, token) => {
    fs.writeFileSync(commandFile, JSON.stringify({button, token, report: `http://127.0.0.1:${site.port}/report`}));
    const seen = site.reports.length;
    // A toolbar press while a popup is still open closes it instead, as a
    // click does; press again until a fresh popup answers.
    let report;
    for (let attempt = 0; attempt < 4 && !report; attempt += 1) {
      await trigger();
      report = await until(`the Firefox popup after ${button}`, 5000, () => site.reports[seen]).catch(() => null);
    }
    if (!report) throw new Error(`no Firefox popup answered ${button}`);
    // Let the popup close before the next toolbar press.
    await sleep(300);
    return JSON.parse(report);
  };
  return {
    name: 'firefox',
    connect: token => press('connect', token),
    disconnect: () => press('disconnect'),
    close: () => { try { ws.close(); } catch (_) {} killTree(child); },
  };
}

// ── the checks, the same for both browsers ───────────────────────────
async function exercise(browser, s, site) {
  const results = [];
  const check = (name, ok, detail = '') => {
    results.push({name, ok: Boolean(ok), detail});
    console.log(`${ok ? 'PASS' : 'FAIL'} [${browser.name}] ${name}${detail ? ` — ${detail}` : ''}`);
  };
  const note = (name, detail) => {
    results.push({name, ok: true, note: true, detail});
    console.log(`NOTE [${browser.name}] ${name} — ${detail}`);
  };
  const c = await browser.connect(s.token);
  check('Connect this tab', /This tab is connected/.test(c.status), c.status);
  const status = tool(s, {action: 'status'});
  check('status reports a connected tab', status.connected === true, JSON.stringify(status));
  const snap = tool(s, {action: 'snapshot'});
  const json = JSON.stringify(snap);
  check('snapshot returns the page', snap.ok === true && snap.result?.title === 'veles bridge test', snap.error || snap.result?.title);
  check('a large Cyrillic page fits one frame (BX-4)', Buffer.byteLength(json) < 64 * 1024 && snap.result?.truncated === true,
    `${Buffer.byteLength(json)} bytes`);
  check('no secret field value leaves the page (BX-5)', !json.includes('hunter2-secret') && !json.includes('913377'));
  const elements = snap.result?.elements || [];
  check('a filled password says only that it is filled', elements.some(e => e.secret && e.filled === true));
  const save = elements.find(e => e.text === 'Save');
  const search = elements.find(e => e.text === 'Search');
  const click = save ? tool(s, {action: 'click', ref: save.ref}) : {ok: false, error: 'Save not listed'};
  check('click a ref', click.ok === true, click.error);
  const fill = search ? tool(s, {action: 'fill', ref: search.ref, text: 'Ђурђевдан'}) : {ok: false, error: 'Search not listed'};
  check('fill a ref', fill.ok === true, fill.error);
  const after = tool(s, {action: 'snapshot'});
  check('the click hit the named element, not a planted ref (BX-6)',
    after.result?.title === 'veles bridge test' && (after.result?.text || '').includes('saved-marker'), after.result?.title);
  check('the fill reached the field', (after.result?.elements || []).some(e => e.value === 'Ђурђевдан'));
  const guarded = tool(s, {action: 'navigate', url: `http://127.0.0.1:${site.port}/second`});
  check('navigate is refused for a loopback URL (urlguard)', guarded.ok === false && /refused/.test(guarded.error || ''), guarded.error);
  if (IDLE_SECS > 0) {
    await sleep(IDLE_SECS * 1000);
    const idle = tool(s, {action: 'snapshot'});
    check(`the bridge survives ${IDLE_SECS}s idle (background not suspended)`, idle.ok === true, idle.error);
  }
  // BX-12: does activeTab survive the tab leaving the page? Recorded per
  // browser; where access is lost, the error must say how to get it back
  // and Connect this tab (no token: the bridge is still up) must restore it.
  const regain = async (where, snap) => {
    note(`after a ${where} navigation`, snap.ok ? `snapshot works (title ${snap.result?.title})` : snap.error);
    if (snap.ok) return snap;
    check(`lost access after a ${where} navigation says how to reconnect`, /Connect this tab/.test(snap.error || ''), snap.error);
    const again = await browser.connect('');
    check(`Connect again (no token) after a ${where} navigation`, /This tab is connected/.test(again.status), again.status);
    const back = tool(s, {action: 'snapshot'});
    check(`snapshot after reconnecting (${where})`, back.ok === true, back.error || back.result?.title);
    return back;
  };
  const links = (after.result?.elements || []).filter(e => e.tag === 'a');
  const same = links.find(e => e.text === 'same-site link');
  tool(s, {action: 'click', ref: same?.ref || '0'});
  await sleep(1500);
  const sameSite = await regain('same-origin', tool(s, {action: 'snapshot'}));
  check('the same-origin link was followed', sameSite.result?.title === 'second page', sameSite.result?.title);
  const cross = (sameSite.result?.elements || []).find(e => e.text === 'cross-site link');
  tool(s, {action: 'click', ref: cross?.ref || '0'});
  await sleep(1500);
  const crossSite = await regain('cross-origin', tool(s, {action: 'snapshot'}));
  check('the cross-origin link was followed', crossSite.result?.title === 'third page', crossSite.result?.title);
  {
    const d = await browser.disconnect();
    check('Disconnect', d.status === 'Disconnected.', d.status);
    await until('the endpoint to go', 5000, () => !connected(s)).catch(() => {});
    check('the host is gone after Disconnect', !connected(s) && tool(s, {action: 'status'}).connected === false);
    const wrong = await browser.connect('0'.repeat(32));
    check('a wrong token is reported at once (BX-14)', /authentication failed/.test(wrong.status) && wrong.ms < 4000,
      `${wrong.status} in ${wrong.ms} ms`);
  }
  return results;
}

async function run(name, bin) {
  const s = scratch(name);
  const site = await serve();
  let browser;
  let results;
  try {
    install(s);
    browser = name === 'firefox' ? await firefoxSession(bin, s, site) : await chromeSession(name, bin, s, site);
    results = await exercise(browser, s, site);
  } catch (error) {
    console.log(`FAIL [${name}] ${error.stack || error}`);
    results = [{name: 'setup', ok: false, detail: String(error)}];
  } finally {
    // A session that failed while starting has no `close`: kill what it spawned.
    if (browser) browser.close(); else s.kill?.();
    site.server.close();
    await sleep(500);
  }
  if (WINDOWS) results.push(...windowsCleanup(s));
  fs.rmSync(s.root, {recursive: true, force: true});
  return results;
}

const browsers = {
  chrome: which([process.env.CHROME_BIN, 'google-chrome-stable', 'google-chrome', 'chromium', 'chromium-browser',
    ...windowsBrowsers('Google\\Chrome\\Application\\chrome.exe')]),
  firefox: which([process.env.FIREFOX_BIN, 'firefox-devedition', 'firefox-developer-edition', 'firefox',
    ...windowsBrowsers('Firefox Developer Edition\\firefox.exe', 'Mozilla Firefox\\firefox.exe', 'Firefox Nightly\\firefox.exe')]),
  // Not registered by a Linux or macOS install; on Windows it is.
  edge: which([process.env.EDGE_BIN, ...(WINDOWS ? [] : ['microsoft-edge-stable', 'microsoft-edge']),
    ...windowsBrowsers('Microsoft\\Edge\\Application\\msedge.exe')]),
};
let failed = 0;
let ran = 0;
for (const name of wanted.length ? wanted : Object.keys(browsers)) {
  if (!browsers[name]) {
    console.log(`SKIP [${name}] no ${name} binary found`);
    continue;
  }
  ran += 1;
  const results = await run(name, browsers[name]);
  failed += results.filter(r => !r.ok).length;
}
console.log(ran ? `browser bridge e2e: ${failed ? `${failed} failed` : 'all passed'} (${ran} browser${ran === 1 ? '' : 's'})`
  : 'browser bridge e2e: SKIP (no browser installed)');
process.exit(failed ? 1 : 0);
