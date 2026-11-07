const api = globalThis.browser || globalThis.chrome;
const status = document.querySelector('#status');
const token = document.querySelector('#token');

function message(payload) {
  if (globalThis.browser) return api.runtime.sendMessage(payload);
  return new Promise((resolve, reject) => {
    api.runtime.sendMessage(payload, reply => {
      if (api.runtime.lastError) reject(new Error(api.runtime.lastError.message));
      else resolve(reply);
    });
  });
}
async function activeTab() {
  const tabs = globalThis.browser
    ? await api.tabs.query({active: true, currentWindow: true})
    : await new Promise((resolve, reject) => {
      api.tabs.query({active: true, currentWindow: true}, tabs => {
        if (api.runtime.lastError) reject(new Error(api.runtime.lastError.message));
        else resolve(tabs);
      });
    });
  const tab = tabs[0];
  if (!tab || !/^https?:\/\//.test(tab.url || '')) {
    throw new Error('Only an ordinary http(s) tab can be connected.');
  }
  return tab;
}
document.querySelector('#connect').addEventListener('click', async () => {
  try {
    const tab = await activeTab();
    // With a live bridge an empty token moves it to this tab.
    const reply = await message({type: 'connect', tabId: tab.id, token: token.value});
    if (!reply?.ok) throw new Error(reply?.error || 'Connection failed.');
    token.value = '';
    status.textContent = 'This tab is connected to veles. Disconnect to end access.';
  } catch (error) { status.textContent = String(error.message || error); }
});
// Disconnect works from any tab: ending access must never need an http page.
document.querySelector('#disconnect').addEventListener('click', async () => {
  try {
    const reply = await message({type: 'disconnect'});
    status.textContent = reply?.ok ? 'Disconnected.' : String(reply?.error || 'Not connected.');
  } catch (error) { status.textContent = String(error.message || error); }
});
message({type: 'status'}).then(reply => {
  if (reply?.connected) {
    status.textContent = 'veles is connected to a tab. Click Connect this tab to move it here (no token needed), or Disconnect to end access.';
  }
}, () => {});
