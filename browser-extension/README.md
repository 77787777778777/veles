# veles browser tab bridge

A small extension for Chrome, Chromium, Microsoft Edge and Firefox that connects one tab you choose to your local veles, through native messaging. It does not enable Chrome remote debugging. It runs no arbitrary JavaScript: the only code it puts into a page is its own fixed snapshot, click, fill and navigate functions. It reads no cookies or browser storage, and a snapshot never contains the value of a password, one-time-code, payment-card or hidden field (only whether the field is filled).

It installs on Linux, Windows and macOS. macOS is untested on a Mac: the install writes the directories Chrome and Firefox document for macOS, and a unit test holds those paths, but no Mac has run it.

## Install

```sh
veles browser-extension install
```

The command creates an owner-only bridge token, writes a launcher and a copy of the extension under the veles data directory, and registers the native host with each browser. It prints the token, the extension directory, the native host and every registration. If any registration already exists it changes nothing and asks for `--overwrite`.

| | launcher and extension copy | where each browser finds the native host |
| --- | --- | --- |
| Linux | `~/.local/share/veles/browser-extension/` (`native-host`, `extension/`) | Chrome `~/.config/google-chrome/NativeMessagingHosts/veles_browser.json`, Chromium `~/.config/chromium/NativeMessagingHosts/`, Firefox (Developer Edition included) `~/.mozilla/native-messaging-hosts/` |
| macOS | `browser-extension/` in the veles data directory | `~/Library/Application Support/Google/Chrome/NativeMessagingHosts/`, `~/Library/Application Support/Chromium/NativeMessagingHosts/`, `~/Library/Application Support/Mozilla/NativeMessagingHosts/` |
| Windows | `%LOCALAPPDATA%\veles\browser-extension\` (`native-host.cmd`, `extension\`, and the manifests `veles_browser.chromium.json` and `veles_browser.firefox.json`) | the default value of `HKCU\Software\Google\Chrome\NativeMessagingHosts\veles_browser`, `HKCU\Software\Chromium\NativeMessagingHosts\veles_browser` and `HKCU\Software\Microsoft\Edge\NativeMessagingHosts\veles_browser` (the Chromium manifest's path), and of `HKCU\Software\Mozilla\NativeMessagingHosts\veles_browser` (the Firefox manifest's path). Nothing needs administrator rights. |

The launcher runs veles from a path that survives upgrades (your Nix profile's `veles`, the AppImage or bundle launcher, an installer release's `current`, or the installed binary) and fixes the veles data directory for the browser-started host. On Windows it is a `.cmd` that Chrome, Edge and Firefox start through `cmd.exe`. Rerun the install with `--overwrite` after moving veles or switching profile.

The Windows zip and `scripts\install\windows.ps1` ship the extension as `browser-extension\` beside `bin\`; the install copies it from there.

The extension is for developer loading; it is not in the Chrome Web Store, the Edge add-ons store or on addons.mozilla.org.

- Chrome or Chromium: open `chrome://extensions`, enable Developer mode, choose **Load unpacked**, and select the printed extension directory.
- Edge: open `edge://extensions`, turn on Developer mode, choose **Load unpacked**, and select the printed extension directory.
- Firefox: open `about:debugging#/runtime/this-firefox`, choose **Load Temporary Add-on**, and select `manifest.json` in that directory. A temporary add-on is removed when Firefox exits. Release Firefox installs only signed add-ons permanently, and this one is not signed. Developer Edition and Nightly accept unsigned add-ons when `xpinstall.signatures.required` is `false` in `about:config`: zip the directory's files into a `.xpi` and use **Install Add-on From File** in `about:addons`.

## Uninstall

```sh
veles browser-extension uninstall
```

removes the registrations (the manifests, and on Windows the registry keys), the launcher, the extension copy and the token, and lists what it removed. Remove the extension on the browser's extensions page as well. A tab that is still connected keeps its native host until you click **Disconnect**.

## Connect a tab

Open an ordinary `http` or `https` tab, click the veles toolbar button, paste the bridge token, and click **Connect this tab**. Clicking the button is what gives the extension access to that tab and nothing else (the browser's `activeTab` grant).

The access does not follow the tab far. Firefox ends it on any navigation, even to another page of the same site; Chrome ends it when the tab leaves the origin it was granted on. A `navigate` action or a click on a link can therefore end it. veles then answers that it no longer has access and asks for a reconnect. To continue on the new page, open the popup on that tab and click **Connect this tab** again; while the bridge is connected no token is needed. Click **Disconnect** in the popup, from any tab, to close the native host and end access.

## Use

Ask veles to use the connected tab, or invoke the `browser_extension` tool with:

- `status`: whether the token and a connected tab exist.
- `snapshot`: page title, URL, visible text (cut to about 48 KB), and up to 120 element refs.
- `click` with `ref`: click an element from the latest snapshot.
- `fill` with `ref` and `text`: fill a text field.
- `navigate` with `url`: navigate the connected tab to a public HTTP(S) URL.

The bridge is owner-only, and only sessions on the machine running the browser can use it. Plan mode allows `status` and `snapshot` and refuses the rest. Every request carries the local token, and the token is removed before the request reaches the extension. Element refs are kept inside the extension's isolated script world, so a page cannot plant or move a ref.

## Verification

`node browser-extension/popup-test.cjs` and `node browser-extension/background-test.cjs` test the popup and the background against Chrome- and Firefox-shaped APIs and a hostile fake page. The Rust tests cover the native host protocol, a wrong token, garbage on the loopback port, large non-ASCII snapshots, late replies, the manifest, and where the install writes on Linux, macOS and Windows (the Windows registry logic against an in-memory registry; on Windows itself, the real `HKCU` calls). `crates/veles-cli/tests/browser_bridge.rs` runs the installed launcher the way Chrome and Firefox do: on Linux with their arguments, on Windows through `cmd.exe` with each browser's own command line, from a plain path and from one with a space, `%` and Cyrillic in it, and then uninstalls.

`scripts/browser-bridge-e2e.sh` runs the whole bridge in real headless Chrome and Firefox (and Edge, when installed) with throwaway profiles: install, loading the installed extension copy, the toolbar press and the popup's Connect, snapshot, click, fill, an idle period, navigation and reconnecting, Disconnect, and a wrong token. A browser that is not installed is skipped. On Windows the same checks run through `scripts\win\browser-bridge-e2e.ps1` (or `scripts/win/browser-bridge-e2e.sh <host>` from Linux); because Windows browsers read only the real `HKCU`, that run installs under a test name of its own and ends by uninstalling it and checking that no registry key or process of the run is left (its scratch directory is then deleted). Neither is part of the commit gate. Before 0.2.0 they passed against Chrome 154 and Firefox Developer Edition 156 on Linux, and against Firefox Developer Edition 160 and Edge 156 on Windows 11. Google Chrome on Windows, macOS, and the store builds have not been run.
