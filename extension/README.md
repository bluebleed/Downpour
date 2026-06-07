# Downpour Capture (browser extension)

Intercepts downloads in your browser and forwards them to the Downpour desktop
app (which must be running) over `http://127.0.0.1:53472`.

For each intercepted download it captures the full request context so
authenticated/protected downloads succeed once handed to the engine:

- the download URL, suggested filename, declared size and MIME type
- the `Cookie` header the browser would send to the download domain
  (via `chrome.cookies.getAll`, including subdomain-scoped cookies)
- the referer and the URL of the page that triggered the download
- a `isMedia` hint based on MIME type / file extension

## Permissions

- `downloads` — intercept and cancel browser downloads
- `cookies` + `<all_urls>` host access — read the cookies a download needs
- `tabs` — read the active tab's URL for referer/page context
- `storage` — remember the on/off toggle

The extension only sends this data to the local Downpour app on
`127.0.0.1:53472`; nothing leaves your machine.

## Load it (unpacked)

**Chrome / Edge / Brave**
1. Go to `chrome://extensions`
2. Enable **Developer mode** (top-right)
3. Click **Load unpacked** and select this `extension/` folder

**Firefox**
1. Go to `about:debugging#/runtime/this-firefox`
2. Click **Load Temporary Add-on…**
3. Select `manifest.json` in this folder

## Use

- Click the toolbar icon to toggle capture on/off and see connection status.
- When ON and the Downpour app is running, new downloads are handed to Downpour.
- When the app is **not** running, the browser downloads normally (safe fallback).

> Note: Firefox uses Manifest V3 slightly differently; for production you may want
> a separate `background.scripts` entry. This works for temporary/dev loading.
