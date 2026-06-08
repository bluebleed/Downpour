// Auto-capture layer: when the browser starts a download, gather the full
// request context (cookies, referer, page URL, content type, size) and hand it
// to the local Downpour app instead of letting the browser download natively.
// Toggle on/off from the popup.
//
// Shares the http://127.0.0.1:53472 contract with src-tauri/src/capture_server.rs.
// The CaptureReq there expects camelCase JSON fields: url, filename, filesize,
// mimeType, cookies, headers, referer, pageUrl, isMedia.

// Pure capture-filter logic (size + extension whitelist/blacklist).
// Loaded into the service-worker global scope as `DownpourFilter`.
// Req 6.5 / 6.6, Property 12.
importScripts("filter.js");

const CAPTURE_URL = "http://127.0.0.1:53472/capture";
const CAPTURE_MEDIA_URL = "http://127.0.0.1:53472/capture-media";

// Default filter config — mirrors DownpourFilter.DEFAULT_FILTER_CONFIG so the
// stored shape is stable even if filter.js is unavailable.
const DEFAULT_FILTER = {
  minSizeBytes: 51200, // 50 KB (Req 6.5: configurable 0–100 MB)
  whitelist: [], // capture ONLY these extensions when non-empty
  blacklist: [], // skip these extensions when non-empty
};

// MIME-type prefixes / extensions we treat as "media" so the engine can route
// them to the media extractor. This is only a hint; the server may override.
const MEDIA_MIME_PREFIXES = ["video/", "audio/", "image/"];
const MEDIA_EXTENSIONS = [
  ".mp4", ".mkv", ".webm", ".avi", ".mov", ".flv", ".m4v", ".mpg", ".mpeg",
  ".mp3", ".m4a", ".aac", ".flac", ".wav", ".ogg", ".opus",
  ".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp", ".svg",
  ".m3u8", ".mpd", ".ts",
];

async function isEnabled() {
  const { enabled } = await chrome.storage.local.get({ enabled: true });
  return enabled;
}

/**
 * Load the stored capture-filter config, falling back to defaults for any
 * missing fields. (Req 6.5, 6.6)
 * @returns {Promise<{minSizeBytes: number, whitelist: string[], blacklist: string[]}>}
 */
async function getFilterConfig() {
  const stored = await chrome.storage.local.get({ filter: DEFAULT_FILTER });
  const raw = stored.filter || DEFAULT_FILTER;
  // Sanitise via the pure helper when available (caps lists at 200, clamps size).
  if (globalThis.DownpourFilter) {
    return globalThis.DownpourFilter.sanitizeConfig(raw);
  }
  return {
    minSizeBytes:
      typeof raw.minSizeBytes === "number" ? raw.minSizeBytes : DEFAULT_FILTER.minSizeBytes,
    whitelist: Array.isArray(raw.whitelist) ? raw.whitelist : [],
    blacklist: Array.isArray(raw.blacklist) ? raw.blacklist : [],
  };
}

/**
 * Build a `Cookie` header value for the given download URL by reading every
 * cookie the browser would send to it (this includes subdomain- and
 * path-scoped cookies). Returns `null` when there are no cookies or the
 * cookies API is unavailable. (Req 6.1)
 *
 * @param {string} url
 * @returns {Promise<string|null>}
 */
async function getCookieHeader(url) {
  if (!chrome.cookies || typeof chrome.cookies.getAll !== "function") {
    return null;
  }
  try {
    const cookies = await chrome.cookies.getAll({ url });
    if (!cookies || cookies.length === 0) return null;
    const header = cookies
      .map((c) => `${c.name}=${c.value}`)
      .join("; ");
    return header.length > 0 ? header : null;
  } catch (_) {
    // Missing "cookies" permission or an invalid URL — proceed without cookies.
    return null;
  }
}

/**
 * Resolve the referer and page URL from the tab that initiated the download.
 * Prefers the download item's own tab, falling back to the active tab in the
 * focused window. (Req 6.2)
 *
 * @param {chrome.downloads.DownloadItem} item
 * @returns {Promise<{ referer: string|null, pageUrl: string|null }>}
 */
async function getTabContext(item) {
  // `referrer` is populated by Chrome on the download item itself when known.
  let referer = item.referrer && item.referrer.length > 0 ? item.referrer : null;
  let pageUrl = null;

  try {
    const tabs = await chrome.tabs.query({
      active: true,
      lastFocusedWindow: true,
    });
    if (tabs && tabs.length > 0 && tabs[0].url) {
      pageUrl = tabs[0].url;
      if (!referer) referer = tabs[0].url;
    }
  } catch (_) {
    // "tabs" permission missing or no accessible tab — keep what we have.
  }

  return { referer, pageUrl };
}

/**
 * Best-effort guess at whether a download is media, from its MIME type and
 * filename extension. Only a hint for the engine. (matches CapturePayload.isMedia)
 *
 * @param {string|null} mimeType
 * @param {string|null} filename
 * @param {string} url
 * @returns {boolean}
 */
function detectMedia(mimeType, filename, url) {
  if (mimeType) {
    const mime = mimeType.toLowerCase();
    if (MEDIA_MIME_PREFIXES.some((p) => mime.startsWith(p))) return true;
  }
  const haystack = `${filename || ""} ${url || ""}`.toLowerCase();
  return MEDIA_EXTENSIONS.some((ext) => haystack.includes(ext));
}

/**
 * Pick the most reliable known file size from a download item, or `null` when
 * the size is not yet known. `totalBytes` is the server-declared size;
 * `fileSize` is the final on-disk size (often -1 mid-download).
 *
 * @param {chrome.downloads.DownloadItem} item
 * @returns {number|null}
 */
function pickFilesize(item) {
  if (typeof item.totalBytes === "number" && item.totalBytes > 0) {
    return item.totalBytes;
  }
  if (typeof item.fileSize === "number" && item.fileSize > 0) {
    return item.fileSize;
  }
  return null;
}

chrome.downloads.onCreated.addListener(async (item) => {
  if (!(await isEnabled())) return;
  if (!item.finalUrl && !item.url) return;

  const url = item.finalUrl || item.url;
  const mimeType = item.mime && item.mime.length > 0 ? item.mime : null;
  const filename = item.filename && item.filename.length > 0 ? item.filename : null;

  // Gather request context in parallel — neither call depends on the other.
  const [cookies, tabContext] = await Promise.all([
    getCookieHeader(url),
    getTabContext(item),
  ]);

  const headers = {};
  if (tabContext.referer) {
    // The engine forwards captured headers verbatim onto every request (Req 6.4),
    // so surface the referer both as a dedicated field and as a header.
    headers["Referer"] = tabContext.referer;
  }

  /** @type {{
   *   url: string, filename: string|null, filesize: number|null,
   *   mimeType: string|null, cookies: string|null,
   *   headers: Record<string,string>, referer: string|null,
   *   pageUrl: string|null, isMedia: boolean
   * }} */
  const payload = {
    url,
    filename,
    filesize: pickFilesize(item),
    mimeType,
    cookies,
    headers,
    referer: tabContext.referer,
    pageUrl: tabContext.pageUrl,
    isMedia: detectMedia(mimeType, filename, url),
  };

  // Apply size + extension whitelist/blacklist filtering (Req 6.5, 6.6).
  // When the filter rejects the download we do nothing and let the browser
  // handle it natively.
  const filterConfig = await getFilterConfig();
  const passes = globalThis.DownpourFilter
    ? globalThis.DownpourFilter.shouldCapture(payload, filterConfig)
    : true;
  if (!passes) return;

  try {
    // Hand off to Downpour. Only cancel the browser download if Downpour accepted it.
    const res = await fetch(CAPTURE_URL, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
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

// ─── Right-click "Download with Downpour" context menu ──────────────────────
//
// Adds two items to the page/video/link right-click menu. They POST the media
// page URL to the app's /capture-media endpoint:
//   - "Download with Downpour"        → opens the Media tab (pick quality/playlist)
//   - "Quick download (Best quality)" → enqueues immediately at highest quality
// The app must be running; if it isn't, we flash a "!" badge as feedback.

const MENU_OPTIONS = "downpour-download-options";
const MENU_QUICK = "downpour-download-quick";

function registerContextMenus() {
  if (!chrome.contextMenus) return;
  chrome.contextMenus.removeAll(() => {
    const contexts = ["page", "video", "link"];
    chrome.contextMenus.create({
      id: MENU_OPTIONS,
      title: "Download with Downpour",
      contexts,
    });
    chrome.contextMenus.create({
      id: MENU_QUICK,
      title: "Quick download (Best quality) with Downpour",
      contexts,
    });
  });
}

chrome.runtime.onInstalled.addListener(registerContextMenus);
chrome.runtime.onStartup.addListener(registerContextMenus);

/** Briefly show a badge on the toolbar icon as click feedback. */
function flashBadge(text, color, title) {
  try {
    chrome.action.setBadgeBackgroundColor({ color });
    chrome.action.setBadgeText({ text });
    if (title) chrome.action.setTitle({ title });
    setTimeout(() => {
      chrome.action.setBadgeText({ text: "" });
      chrome.action.setTitle({ title: "Downpour Capture" });
    }, 3500);
  } catch (_) {
    // action API unavailable — ignore.
  }
}

if (chrome.contextMenus && chrome.contextMenus.onClicked) {
  chrome.contextMenus.onClicked.addListener(async (info, tab) => {
    if (info.menuItemId !== MENU_OPTIONS && info.menuItemId !== MENU_QUICK) return;
    // Reuse the popup's master on/off switch.
    if (!(await isEnabled())) return;

    // A right-clicked <video> srcUrl is usually a blob: we can't fetch, so prefer
    // the link URL, then the page URL — the page URL is what yt-dlp needs.
    const url = info.linkUrl || info.pageUrl || (tab && tab.url) || null;
    if (!url) return;

    const mode = info.menuItemId === MENU_QUICK ? "quick" : "options";
    const cookies = await getCookieHeader(url);
    const payload = {
      url,
      mode,
      quality: "best",
      title: tab && tab.title ? tab.title : null,
      cookies,
    };

    try {
      const res = await fetch(CAPTURE_MEDIA_URL, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(payload),
      });
      if (res.ok) {
        flashBadge(mode === "quick" ? "↓" : "→", "#7c5cfc");
      } else {
        flashBadge("!", "#f87272", "Downpour rejected the request");
      }
    } catch (_) {
      // App not running — the capture server isn't listening.
      flashBadge("!", "#f87272", "Open Downpour first, then try again");
    }
  });
}

// Receive media-link reports from the content script (content.js). These are
// in-page <video>/<audio>/<img>/<source> sources detected on the page the user
// is viewing (Req 6: "detect media links on page"). We keep the most recent
// per-tab list so the popup can offer them for capture; nothing is sent off the
// machine here. (Responsible-use: detection only, on pages the user opened.)
const mediaLinksByTab = new Map();

chrome.runtime.onMessage.addListener((message, sender, sendResponse) => {
  if (!message || message.type !== "downpour-media-links") return false;

  const tabId = sender && sender.tab ? sender.tab.id : undefined;
  if (typeof tabId === "number") {
    const links = Array.isArray(message.links) ? message.links.slice(0, 500) : [];
    mediaLinksByTab.set(tabId, links);
  }
  sendResponse({ ok: true });
  return false;
});

// The popup asks for the active tab's detected media links.
chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  if (!message || message.type !== "downpour-get-media-links") return false;
  const tabId = message.tabId;
  sendResponse({ links: mediaLinksByTab.get(tabId) || [] });
  return false;
});

// Drop cached links when a tab closes to avoid leaking memory.
if (chrome.tabs && chrome.tabs.onRemoved) {
  chrome.tabs.onRemoved.addListener((tabId) => {
    mediaLinksByTab.delete(tabId);
  });
}
