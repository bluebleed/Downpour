// Privacy-first browser handoff for Downpour. It only talks to the local
// desktop app and capture is disabled until the user enables it in the popup.

importScripts("filter.js");

const CAPTURE_URL = "http://127.0.0.1:53472/capture";
const CAPTURE_MEDIA_URL = "http://127.0.0.1:53472/capture-media";
const DEFAULT_FILTER = {
  minSizeBytes: 51200,
  whitelist: [],
  blacklist: [],
};
const MEDIA_MIME_PREFIXES = ["video/", "audio/", "image/"];
const MEDIA_EXTENSIONS = [
  ".mp4", ".mkv", ".webm", ".avi", ".mov", ".flv", ".m4v", ".mpg", ".mpeg",
  ".mp3", ".m4a", ".aac", ".flac", ".wav", ".ogg", ".opus",
  ".jpg", ".jpeg", ".png", ".gif", ".webp", ".bmp", ".svg",
  ".m3u8", ".mpd", ".ts",
];

async function isEnabled() {
  const { enabled } = await chrome.storage.local.get({ enabled: false });
  return enabled;
}

async function getFilterConfig() {
  const { filter } = await chrome.storage.local.get({ filter: DEFAULT_FILTER });
  return globalThis.DownpourFilter
    ? globalThis.DownpourFilter.sanitizeConfig(filter)
    : filter || DEFAULT_FILTER;
}

function detectMedia(mimeType, filename, url) {
  if (mimeType && MEDIA_MIME_PREFIXES.some((prefix) => mimeType.toLowerCase().startsWith(prefix))) {
    return true;
  }
  const value = `${filename || ""} ${url || ""}`.toLowerCase();
  return MEDIA_EXTENSIONS.some((extension) => value.includes(extension));
}

function pickFilesize(item) {
  if (typeof item.totalBytes === "number" && item.totalBytes > 0) return item.totalBytes;
  if (typeof item.fileSize === "number" && item.fileSize > 0) return item.fileSize;
  return null;
}

chrome.downloads.onCreated.addListener(async (item) => {
  if (!(await isEnabled()) || (!item.finalUrl && !item.url)) return;

  const url = item.finalUrl || item.url;
  const filename = item.filename || null;
  const mimeType = item.mime || null;
  const referer = item.referrer || null;
  const payload = {
    url,
    filename,
    filesize: pickFilesize(item),
    mimeType,
    // The extension deliberately does not read or forward browser cookies.
    cookies: null,
    headers: referer ? { Referer: referer } : {},
    referer,
    pageUrl: null,
    isMedia: detectMedia(mimeType, filename, url),
  };

  const filter = await getFilterConfig();
  const passes = globalThis.DownpourFilter
    ? globalThis.DownpourFilter.shouldCapture(payload, filter)
    : true;
  if (!passes) return;

  try {
    const response = await fetch(CAPTURE_URL, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(payload),
    });
    if (response.ok) {
      chrome.downloads.cancel(item.id);
      chrome.downloads.erase({ id: item.id });
    }
  } catch {
    // Downpour is not running: leave the browser download unchanged.
  }
});

const MENU_OPTIONS = "downpour-download-options";
const MENU_QUICK = "downpour-download-quick";

function registerContextMenus() {
  chrome.contextMenus.removeAll(() => {
    const contexts = ["page", "video", "link"];
    chrome.contextMenus.create({ id: MENU_OPTIONS, title: "Download with Downpour", contexts });
    chrome.contextMenus.create({
      id: MENU_QUICK,
      title: "Quick download (Best quality) with Downpour",
      contexts,
    });
  });
}

chrome.runtime.onInstalled.addListener(registerContextMenus);
chrome.runtime.onStartup.addListener(registerContextMenus);

function flashBadge(text, color, title) {
  chrome.action.setBadgeBackgroundColor({ color });
  chrome.action.setBadgeText({ text });
  if (title) chrome.action.setTitle({ title });
  setTimeout(() => {
    chrome.action.setBadgeText({ text: "" });
    chrome.action.setTitle({ title: "Downpour Capture" });
  }, 3500);
}

chrome.contextMenus.onClicked.addListener(async (info) => {
  if (info.menuItemId !== MENU_OPTIONS && info.menuItemId !== MENU_QUICK) return;
  if (!(await isEnabled())) return;

  const url = info.linkUrl || info.pageUrl;
  if (!url) return;
  const mode = info.menuItemId === MENU_QUICK ? "quick" : "options";
  try {
    const response = await fetch(CAPTURE_MEDIA_URL, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ url, mode, quality: "best", title: null, cookies: null }),
    });
    if (response.ok) {
      flashBadge(mode === "quick" ? "D" : "+", "#7c5cfc");
    } else {
      flashBadge("!", "#f87272", "Downpour rejected the request");
    }
  } catch {
    flashBadge("!", "#f87272", "Open Downpour first, then try again");
  }
});
