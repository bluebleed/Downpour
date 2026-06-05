// Auto-capture layer: when the browser starts a download, cancel it and hand the
// URL to the local Downpour app instead. Toggle on/off from the popup.

const CAPTURE_URL = "http://127.0.0.1:53472/capture";

async function isEnabled() {
  const { enabled } = await chrome.storage.local.get({ enabled: true });
  return enabled;
}

chrome.downloads.onCreated.addListener(async (item) => {
  if (!(await isEnabled())) return;
  if (!item.finalUrl && !item.url) return;

  const url = item.finalUrl || item.url;

  try {
    // Hand off to Downpour. Only cancel the browser download if Downpour accepted it.
    const res = await fetch(CAPTURE_URL, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ url, filename: item.filename || null }),
    });

    if (res.ok) {
      chrome.downloads.cancel(item.id);
      // Remove the cancelled entry from the browser's download shelf/list.
      chrome.downloads.erase({ id: item.id });
    }
  } catch (_) {
    // Downpour app not running — let the browser handle the download normally.
  }
});
