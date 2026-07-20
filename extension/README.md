# Downpour Capture

This optional Manifest V3 extension hands new browser downloads to the locally
running Downpour desktop app at `http://127.0.0.1:53472`.

## Privacy model

- Capture is **off by default**.
- It only handles downloads that begin after you turn capture on.
- It does not import browser download history or scan local files.
- It does not request browsing-history, cookie, or all-sites permissions.
- It sends data only to the local Downpour app, never to a remote service.

When Downpour is not running, your browser keeps the download normally.

## Install for development

1. Start the desktop app: `npm run tauri dev`.
2. In Chrome, Edge, or Brave, open the extensions page.
3. Enable **Developer mode**.
4. Choose **Load unpacked** and select this `extension` directory.
5. Open the extension popup. It should report **Connected to Downpour**.
6. Turn on **Capture downloads** only when you want future downloads routed to
   Downpour.

Use the page context menu for the explicit media actions. Only download media
you are permitted to access; the extension does not bypass DRM, paywalls, or
other access controls.
