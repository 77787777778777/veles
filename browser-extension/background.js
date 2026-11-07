// The veles tab bridge's background: Chrome runs it as the MV3 service
// worker, Firefox as an event page (manifest background.scripts). One
// native port to the local veles host, one connected tab at a time.
const api = globalThis.browser || globalThis.chrome;
let port;
let connection = null;
let tabId = null;

function closePort() {
  const open = port;
  port = undefined;
  connection = null;
  tabId = null;
  if (open) {
    try { open.disconnect(); } catch (_) { /* already gone */ }
  }
}

async function execute(func, args = []) {
  const [result] = await api.scripting.executeScript({target: {tabId}, func, args});
  return result?.result;
}

// The page functions below run in the tab through scripting.executeScript,
// in the extension's isolated content-script world. They are serialised,
// so each one is self-contained. Element refs live in that world's
// globalThis, which persists across calls in the same document and which
// the page cannot read or write; the DOM is never trusted for a ref
// (BX-6: a page could plant or keep a data-veles-ref attribute).
function snapshot(budget) {
  const encoder = new TextEncoder();
  const size = value => encoder.encode(JSON.stringify(value)).length;
  // The longest prefix of `text` whose JSON form fits in `max` bytes.
  const clip = (text, max) => {
    if (size(text) <= max) return text;
    let lo = 0;
    let hi = text.length;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (size(text.slice(0, mid)) <= max) lo = mid; else hi = mid - 1;
    }
    let out = text.slice(0, lo);
    if (/[\uD800-\uDBFF]$/.test(out)) out = out.slice(0, -1);
    return out;
  };
  const attr = (el, name) => (el.getAttribute(name) || '').trim();
  // Never return the value of a secret field: passwords, one-time codes,
  // payment cards, or anything hidden (BX-5). Only whether it is filled.
  const secret = el => {
    const tag = el.tagName.toLowerCase();
    if (tag !== 'input' && tag !== 'textarea') return false;
    const type = (attr(el, 'type') || el.type || '').toLowerCase();
    if (type === 'password' || type === 'hidden') return true;
    return /password|one-time-code|(^|\s)cc-/.test(attr(el, 'autocomplete').toLowerCase());
  };
  const refs = [];
  const elements = [];
  for (const el of document.querySelectorAll('a,button,input,textarea,select,[role="button"]')) {
    if (elements.length >= 120) break;
    const rect = el.getBoundingClientRect();
    if (!rect.width || !rect.height) continue;
    refs.push(el);
    const tag = el.tagName.toLowerCase();
    const item = {ref: String(refs.length), tag};
    const type = attr(el, 'type').toLowerCase();
    if (type) item.type = type;
    const field = tag === 'input' || tag === 'textarea' || tag === 'select';
    const label = field
      ? attr(el, 'aria-label') || attr(el, 'placeholder') || attr(el, 'name') || attr(el, 'title')
      : (el.innerText || '').trim() || attr(el, 'aria-label') || attr(el, 'title');
    item.text = clip(label, 300);
    if (field) {
      const value = String(el.value ?? '');
      if (secret(el)) {
        item.secret = true;
        item.filled = value !== '';
      } else if (value) {
        item.value = clip(value, 300);
      }
    }
    elements.push(item);
  }
  globalThis.__velesRefs = refs;
  const result = {title: clip(document.title || '', 1000), url: clip(location.href, 2000), text: '', elements};
  const page = document.body?.innerText || '';
  const room = budget - size(result);
  result.text = clip(page, Math.max(0, room));
  if (result.text.length < page.length) result.truncated = true;
  return result;
}
function clickElement(ref) {
  const el = /^[0-9]+$/.test(ref) ? (globalThis.__velesRefs || [])[Number(ref) - 1] : undefined;
  if (!el || !el.isConnected) throw new Error('No such element ref; take a fresh snapshot.');
  el.click();
  return {ok: true};
}
function fillElement(ref, text) {
  const el = /^[0-9]+$/.test(ref) ? (globalThis.__velesRefs || [])[Number(ref) - 1] : undefined;
  if (!el || !el.isConnected) throw new Error('No such element ref; take a fresh snapshot.');
  if (!('value' in el)) throw new Error('Ref is not a text field.');
  el.focus();
  el.value = text;
  el.dispatchEvent(new Event('input', {bubbles: true}));
  el.dispatchEvent(new Event('change', {bubbles: true}));
  return {ok: true};
}
function navigate(url) { location.assign(url); return {ok: true}; }

// A snapshot stays well under the host's frame limit and the model's
// patience: about 48 KB of JSON whatever the page's script (BX-4).
const SNAPSHOT_BYTES = 48 * 1024;

async function runAction(request) {
  if (tabId === null) throw new Error('No tab is connected.');
  try {
    if (request.action === 'snapshot') return await execute(snapshot, [SNAPSHOT_BYTES]);
    if (request.action === 'click') return await execute(clickElement, [request.ref]);
    if (request.action === 'fill') return await execute(fillElement, [request.ref, request.text]);
    if (request.action === 'navigate') return await execute(navigate, [request.url]);
  } catch (error) {
    const text = String(error?.message || error);
    // The activeTab grant ends when the tab leaves the page it was given
    // on. Say what to do, not only what the browser said.
    if (/permission|cannot access|missing host|cannot be scripted/i.test(text)) {
      throw new Error(`veles no longer has access to this tab (${text}). Open the veles popup on the tab and click Connect this tab to grant it again.`);
    }
    throw error;
  }
  throw new Error('Unsupported action.');
}

function connect(token, tab) {
  if (port && connection) {
    // Already authenticated: the popup was opened on a tab (which grants
    // fresh activeTab access to it), so this only moves the bridge there.
    if (!token) {
      tabId = tab;
      return Promise.resolve({ok: true});
    }
  }
  if (!token) return Promise.resolve({ok: false, error: 'Enter the local bridge token.'});
  closePort();
  const id = crypto.randomUUID();
  let opened;
  try {
    opened = api.runtime.connectNative('veles_browser');
  } catch (error) {
    return Promise.resolve({ok: false, error: String(error?.message || error)});
  }
  port = opened;
  return new Promise(resolve => {
    let settled = false;
    const finish = reply => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      resolve(reply);
    };
    const timer = setTimeout(() => {
      if (port === opened) closePort();
      finish({ok: false, error: 'The local bridge did not answer. Check the token and native host installation.'});
    }, 8000);
    opened.onDisconnect.addListener(() => {
      const why = api.runtime.lastError?.message || opened.error?.message;
      if (port === opened) {
        port = undefined;
        connection = null;
        tabId = null;
      }
      finish({ok: false, error: why ? `The local bridge closed: ${why}` : 'The local bridge closed the connection.'});
    });
    opened.onMessage.addListener(request => {
      if (!request || request.id !== id) return;
      if (request.type === 'error') {
        if (port === opened) closePort();
        finish({ok: false, error: request.error || 'Bridge refused the connection.'});
        return;
      }
      if (request.type === 'hello-ok') {
        connection = id;
        tabId = tab;
        finish({ok: true});
        return;
      }
      if (request.type !== 'action' || connection !== id || port !== opened) return;
      const reply = {type: 'result', id, seq: request.seq, action: request.action};
      runAction(request).then(
        result => opened.postMessage({...reply, ok: true, result}),
        error => opened.postMessage({...reply, ok: false, error: String(error?.message || error)}));
    });
    opened.postMessage({type: 'hello', id, token});
    opened.postMessage({type: 'connect', id});
  });
}

function handle(message) {
  if (message?.type === 'status') return Promise.resolve({ok: true, connected: Boolean(port && connection)});
  if (message?.type === 'disconnect') {
    if (port && connection) port.postMessage({type: 'close', id: connection});
    closePort();
    return Promise.resolve({ok: true});
  }
  if (message?.type === 'connect') return connect(message.token, message.tabId);
  return Promise.resolve({ok: false, error: 'Unknown request.'});
}

// The portable form: answer through sendResponse and return true. Older
// Chrome releases ignore a returned Promise, so the popup would see "message
// port closed" (BX-13). Only this extension's own pages may ask.
api.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (sender?.id !== api.runtime.id) {
    sendResponse({ok: false, error: 'Unknown sender.'});
    return false;
  }
  handle(message).then(sendResponse, error => sendResponse({ok: false, error: String(error?.message || error)}));
  return true;
});
