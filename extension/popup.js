// Popup controller: capture on/off toggle, filter configuration UI (size +
// whitelist/blacklist), connection status, and a read-only list of media links
// detected on the current page by the content script.

const F = globalThis.DownpourFilter;
const DEFAULT_FILTER = (F && F.DEFAULT_FILTER_CONFIG) || {
  minSizeBytes: 51200,
  whitelist: [],
  blacklist: [],
};
const MAX_LIST = (F && F.MAX_LIST_ENTRIES) || 200;
const BYTES_PER_MB = 1024 * 1024;

const $ = (id) => document.getElementById(id);
const checkbox = $("enabled");
const status = $("status");
const minSizeEl = $("minSize");
const whitelistEl = $("whitelist");
const blacklistEl = $("blacklist");
const whitelistCountEl = $("whitelistCount");
const blacklistCountEl = $("blacklistCount");
const savedMsg = $("savedMsg");

// --- Capture on/off -------------------------------------------------------
chrome.storage.local.get({ enabled: true }).then(({ enabled }) => {
  checkbox.checked = enabled;
});
checkbox.addEventListener("change", () => {
  chrome.storage.local.set({ enabled: checkbox.checked });
});

// --- Filter config --------------------------------------------------------

/** Parse a textarea into a normalised, de-duplicated, capped extension list. */
function parseList(text) {
  const tokens = (text || "")
    .split(/[\s,]+/)
    .map((t) => t.trim())
    .filter((t) => t.length > 0);
  if (F) return F.normalizeList(tokens);
  // Fallback normalisation if filter.js failed to load.
  const seen = new Set();
  const out = [];
  for (const t of tokens) {
    const e = t.replace(/^\.+/, "").toLowerCase();
    if (e && !seen.has(e)) {
      seen.add(e);
      out.push(e);
    }
    if (out.length >= MAX_LIST) break;
  }
  return out;
}

function updateCount(el, listEl) {
  const n = parseList(listEl.value).length;
  el.textContent = `${n} / ${MAX_LIST} extensions`;
  el.classList.toggle("over", n >= MAX_LIST);
}

function loadFilter() {
  chrome.storage.local.get({ filter: DEFAULT_FILTER }).then(({ filter }) => {
    const cfg = F ? F.sanitizeConfig(filter) : filter;
    minSizeEl.value = (cfg.minSizeBytes / BYTES_PER_MB).toString();
    whitelistEl.value = (cfg.whitelist || []).join(", ");
    blacklistEl.value = (cfg.blacklist || []).join(", ");
    updateCount(whitelistCountEl, whitelistEl);
    updateCount(blacklistCountEl, blacklistEl);
  });
}

function saveFilter() {
  let mb = parseFloat(minSizeEl.value);
  if (!isFinite(mb) || mb < 0) mb = 0;
  if (mb > 100) mb = 100;
  const raw = {
    minSizeBytes: Math.round(mb * BYTES_PER_MB),
    whitelist: parseList(whitelistEl.value),
    blacklist: parseList(blacklistEl.value),
  };
  const cfg = F ? F.sanitizeConfig(raw) : raw;
  chrome.storage.local.set({ filter: cfg }).then(() => {
    savedMsg.hidden = false;
    setTimeout(() => (savedMsg.hidden = true), 1500);
    loadFilter();
  });
}

$("save").addEventListener("click", saveFilter);
$("reset").addEventListener("click", () => {
  chrome.storage.local.set({ filter: DEFAULT_FILTER }).then(loadFilter);
});
whitelistEl.addEventListener("input", () => updateCount(whitelistCountEl, whitelistEl));
blacklistEl.addEventListener("input", () => updateCount(blacklistCountEl, blacklistEl));

loadFilter();

// --- Connection status ----------------------------------------------------
fetch("http://127.0.0.1:53472/health")
  .then((r) => (r.ok ? (status.textContent = "Connected to Downpour ✓") : Promise.reject()))
  .catch(() => {
    status.textContent = "Downpour app not running";
    status.style.color = "#c0392b";
  });

// --- Media links on current page -----------------------------------------
function renderMediaLinks(links) {
  const ul = $("mediaLinks");
  ul.textContent = "";
  if (!links || links.length === 0) {
    const li = document.createElement("li");
    li.className = "empty";
    li.textContent = "No media detected on this page.";
    ul.appendChild(li);
    return;
  }
  for (const link of links) {
    const li = document.createElement("li");
    const kind = document.createElement("span");
    kind.className = "kind";
    kind.textContent = link.kind || "media";
    const text = document.createElement("span");
    text.textContent = link.label ? `${link.label} — ${link.url}` : link.url;
    li.appendChild(kind);
    li.appendChild(text);
    ul.appendChild(li);
  }
}

chrome.tabs.query({ active: true, lastFocusedWindow: true }).then((tabs) => {
  const tabId = tabs && tabs[0] ? tabs[0].id : undefined;
  if (typeof tabId !== "number") {
    renderMediaLinks([]);
    return;
  }
  chrome.runtime.sendMessage(
    { type: "downpour-get-media-links", tabId },
    (resp) => {
      void chrome.runtime.lastError;
      renderMediaLinks(resp ? resp.links : []);
    }
  );
});
