// No browser binary is needed for this API-shape test.
const fs = require('fs');
const vm = require('vm');
const assert = require('assert');
const source = fs.readFileSync(require('path').join(__dirname, 'popup.js'), 'utf8');

async function run(firefox, url = 'https://example.org/') {
  const listeners = {};
  let sent;
  const api = {
    runtime: {
      sendMessage(message, callback) {
        sent = message;
        if (firefox) return Promise.resolve({ok: true});
        callback({ok: true});
      }
    },
    tabs: {
      query(_query, callback) {
        const tab = {id: 7, url};
        if (firefox) return Promise.resolve([tab]);
        callback([tab]);
      }
    }
  };
  const elements = {
    '#token': {value: 'secret'},
    '#status': {textContent: ''}
  };
  const context = {
    browser: firefox ? api : undefined,
    chrome: firefox ? undefined : api,
    document: {querySelector(selector) {
      if (selector === '#connect' || selector === '#disconnect') return {addEventListener(_event, fn) { listeners[selector] = fn; }};
      return elements[selector];
    }},
    Promise, Error, console
  };
  vm.createContext(context);
  vm.runInContext(source, context);
  if (!/^https?:/.test(url)) {
    // BX-15: ending access must work from any tab.
    await listeners['#disconnect']();
    assert.strictEqual(sent.type, 'disconnect');
    assert.strictEqual(elements['#status'].textContent, 'Disconnected.');
    return;
  }
  await listeners['#connect']();
  assert.strictEqual(sent.type, 'connect');
  assert.strictEqual(sent.tabId, 7);
  assert.strictEqual(sent.token, 'secret');
  assert.strictEqual(elements['#status'].textContent,
    'This tab is connected to veles. Disconnect to end access.');
}
(async () => {
  await run(false);
  await run(true);
  await run(false, 'chrome://newtab/');
  await run(true, 'about:blank');
  console.log('browser-extension popup API compatibility: 4 passed, 0 failed');
})().catch(error => { console.error(error); process.exit(1); });
