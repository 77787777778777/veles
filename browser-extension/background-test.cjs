// background.js against a Chrome-shaped API and a fake page, without a
// browser binary. The real-browser run is scripts/browser-bridge-e2e.sh.
//
// The Chrome stub honours only sendResponse (a returned Promise is
// ignored, as older Chrome does), executeScript serialises the page
// function with toString() as the browser does (so a page function that
// leans on an outer helper fails here too), and the page is hostile: it
// planted a data-veles-ref="1" on a hidden Delete button.
const fs = require('fs');
const path = require('path');
const vm = require('vm');
const assert = require('assert');
const source = fs.readFileSync(path.join(__dirname, 'background.js'), 'utf8');

function element(tag, {text = '', attrs = {}, value = '', visible = true, planted} = {}) {
  const el = {
    tagName: tag.toUpperCase(), innerText: text, value, attrs, dataset: {},
    clicked: 0, isConnected: true,
    getAttribute(name) {
      if (name === 'data-veles-ref') return this.dataset.velesRef ?? null;
      return name in this.attrs ? this.attrs[name] : null;
    },
    getBoundingClientRect() { return visible ? {width: 50, height: 20} : {width: 0, height: 0}; },
    click() { this.clicked += 1; },
    focus() {},
    dispatchEvent() { return true; },
  };
  if (attrs.type) el.type = attrs.type;
  if (planted) el.dataset.velesRef = planted;
  return el;
}

function page() {
  const planted = element('button', {text: 'Delete account', visible: false, planted: '1'});
  const save = element('button', {text: 'Save'});
  const password = element('input', {attrs: {type: 'password', name: 'pw'}, value: 'hunter2-secret'});
  const otp = element('input', {attrs: {type: 'text', autocomplete: 'one-time-code', name: 'otp'}, value: '913377'});
  const card = element('input', {attrs: {autocomplete: 'billing cc-number'}, value: '4111111111111111'});
  const search = element('input', {attrs: {type: 'search', 'aria-label': 'Search'}, value: 'query words'});
  const elements = [planted, save, password, otp, card, search];
  const body = 'Ћирилица и ијекавица — '.repeat(2000);
  const document = {
    title: 'Пример', body: {innerText: body},
    querySelectorAll() { return elements; },
    querySelector(selector) {
      const m = /\[data-veles-ref="(.*)"\]/.exec(selector);
      return m ? elements.find(el => el.dataset.velesRef === m[1]) || null : null;
    },
  };
  const context = {
    document, location: {href: 'https://example.org/', assign() {}},
    TextEncoder, Event: function Event(type) { this.type = type; },
    CSS: {escape: s => s},
  };
  vm.createContext(context);
  return {context, planted, save, password};
}

function harness() {
  const tab = page();
  let listener;
  const frames = [];
  let host;
  const timers = [];
  const port = {
    onMessage: {listeners: [], addListener(fn) { this.listeners.push(fn); }},
    onDisconnect: {listeners: [], addListener(fn) { this.listeners.push(fn); }},
    postMessage(frame) { frames.push(JSON.parse(JSON.stringify(frame))); setImmediate(() => host && host(frame)); },
    disconnect() {},
    deliver(frame) { for (const fn of this.onMessage.listeners) fn(JSON.parse(JSON.stringify(frame))); },
  };
  const chrome = {
    runtime: {
      id: 'veles-ext',
      lastError: undefined,
      connectNative(name) { assert.strictEqual(name, 'veles_browser'); return port; },
      onMessage: {addListener(fn) { listener = fn; }},
    },
    scripting: {
      async executeScript({target, func, args}) {
        assert.strictEqual(target.tabId, 7, 'actions go to the connected tab');
        const call = `(${func.toString()})(...${JSON.stringify(args)})`;
        const result = vm.runInContext(call, tab.context);
        return [{result: JSON.parse(JSON.stringify(result))}];
      },
    },
  };
  const context = {
    chrome, crypto: require('crypto'), console, Promise, Error, JSON,
    setTimeout(fn, ms) { const t = setTimeout(fn, ms); timers.push(t); return t; },
    clearTimeout,
  };
  vm.createContext(context);
  vm.runInContext(source, context);
  // The popup's side of runtime.sendMessage, Chrome style.
  const ask = (message, ms = 2000, sender = {id: 'veles-ext', url: 'chrome-extension://veles-ext/popup.html'}) => new Promise((resolve, reject) => {
    let answered = false;
    const giveUp = setTimeout(() => reject(new Error(`no sendResponse for ${message.type} within ${ms} ms`)), ms);
    const keep = listener(message, sender,
      reply => { answered = true; clearTimeout(giveUp); resolve(JSON.parse(JSON.stringify(reply))); });
    if (keep !== true && !answered) {
      // Chrome closes the channel unless the listener returned true.
      setImmediate(() => { clearTimeout(giveUp); reject(new Error('message port closed before a response was received')); });
    }
  });
  const done = () => timers.forEach(clearTimeout);
  return {tab, port, frames, ask, setHost(fn) { host = fn; }, done};
}

const tests = [];
const test = (name, fn) => tests.push([name, fn]);

test('connect answers through sendResponse and hello carries the connect id (BX-13, BX-14)', async () => {
  const h = harness();
  h.setHost(frame => {
    if (frame.type === 'connect') h.port.deliver({type: 'hello-ok', id: frame.id});
  });
  const reply = await h.ask({type: 'connect', tabId: 7, token: 'tok'});
  assert.deepStrictEqual(reply, {ok: true});
  const [hello, connect] = h.frames;
  assert.strictEqual(hello.type, 'hello');
  assert.strictEqual(hello.token, 'tok');
  assert.ok(hello.id && hello.id === connect.id, 'the host echoes hello.id in its refusal');
  h.done();
});

test('a refused token is reported at once, not after the 8 s timeout (BX-14)', async () => {
  const h = harness();
  h.setHost(frame => {
    if (frame.type === 'hello') h.port.deliver({type: 'error', id: frame.id ?? null, error: 'authentication failed'});
  });
  const reply = await h.ask({type: 'connect', tabId: 7, token: 'wrong'}, 1500);
  assert.deepStrictEqual(reply, {ok: false, error: 'authentication failed'});
  h.done();
});

async function connected() {
  const h = harness();
  const results = [];
  h.setHost(frame => {
    if (frame.type === 'connect') h.port.deliver({type: 'hello-ok', id: frame.id});
    if (frame.type === 'result') results.push(frame);
  });
  assert.deepStrictEqual(await h.ask({type: 'connect', tabId: 7, token: 'tok'}), {ok: true});
  const id = h.frames[1].id;
  const act = async (action) => {
    const before = results.length;
    h.port.deliver({type: 'action', id, ...action});
    for (let i = 0; i < 200 && results.length === before; i++) await new Promise(r => setTimeout(r, 5));
    assert.ok(results.length > before, `no result for ${JSON.stringify(action)}`);
    return results[results.length - 1];
  };
  return {h, act};
}

test('a snapshot never carries a secret field value and fits the frame budget (BX-4, BX-5)', async () => {
  const {h, act} = await connected();
  const reply = await act({action: 'snapshot', seq: 41});
  assert.strictEqual(reply.ok, true, reply.error);
  assert.strictEqual(reply.seq, 41, 'the reply echoes the request seq (BX-11)');
  const json = JSON.stringify(reply);
  for (const secret of ['hunter2-secret', '913377', '4111111111111111']) {
    assert.ok(!json.includes(secret), `snapshot leaked ${secret}`);
  }
  const bytes = Buffer.byteLength(json, 'utf8');
  assert.ok(bytes <= 56 * 1024, `snapshot is ${bytes} bytes of JSON`);
  const fields = reply.result.elements.filter(e => e.tag === 'input');
  assert.ok(fields.some(e => e.secret && e.filled === true), 'a filled secret field says only that it is filled');
  assert.ok(fields.some(e => e.value === 'query words'), 'an ordinary field still shows its value');
  assert.strictEqual(reply.result.title, 'Пример');
  assert.strictEqual(reply.result.truncated, true);
  h.done();
});

test('a ref the page planted in the DOM cannot redirect a click (BX-6)', async () => {
  const {h, act} = await connected();
  const snap = await act({action: 'snapshot', seq: 1});
  const save = snap.result.elements.find(e => e.text === 'Save');
  assert.ok(save, 'Save is listed');
  const click = await act({action: 'click', ref: save.ref, seq: 2});
  assert.strictEqual(click.ok, true, click.error);
  assert.strictEqual(h.tab.planted.clicked, 0, 'the planted hidden button was clicked');
  assert.strictEqual(h.tab.save.clicked, 1);
  const bad = await act({action: 'click', ref: '99', seq: 3});
  assert.strictEqual(bad.ok, false);
  h.done();
});

test('only the extension itself may message the background', async () => {
  const h = harness();
  const reply = await h.ask({type: 'connect', tabId: 7, token: 'tok'}, 500, {id: 'another-extension'});
  assert.strictEqual(reply.ok, false);
  assert.strictEqual(h.frames.length, 0, 'a foreign sender never opens the native port');
  h.done();
});

(async () => {
  let failed = 0;
  for (const [name, fn] of tests) {
    try { await fn(); console.log(`ok   ${name}`); }
    catch (error) { failed += 1; console.log(`FAIL ${name}\n     ${error.message}`); }
  }
  console.log(`browser-extension background: ${tests.length - failed} passed, ${failed} failed`);
  process.exit(failed ? 1 : 0);
})();
