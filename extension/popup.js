// Popup controller: opt-in capture toggle, local connection status, and
// size/extension filter configuration. This code never reads browser history.

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

// Capture is explicitly disabled until the user turns it on.
chrome.storage.local.get({ enabled: false }).then(({ enabled }) => {
  checkbox.checked = enabled;
});
checkbox.addEventListener("change", () => {
  chrome.storage.local.set({ enabled: checkbox.checked });
});

function parseList(text) {
  const tokens = (text || "")
    .split(/[\s,]+/)
    .map((token) => token.trim())
    .filter(Boolean);
  if (F) return F.normalizeList(tokens);
  return [...new Set(tokens.map((token) => token.replace(/^\.+/, "").toLowerCase()))]
    .filter(Boolean)
    .slice(0, MAX_LIST);
}

function updateCount(element, input) {
  const count = parseList(input.value).length;
  element.textContent = `${count} / ${MAX_LIST} extensions`;
  element.classList.toggle("over", count >= MAX_LIST);
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
  let mb = Number.parseFloat(minSizeEl.value);
  if (!Number.isFinite(mb) || mb < 0) mb = 0;
  const raw = {
    minSizeBytes: Math.round(Math.min(mb, 100) * BYTES_PER_MB),
    whitelist: parseList(whitelistEl.value),
    blacklist: parseList(blacklistEl.value),
  };
  chrome.storage.local.set({ filter: F ? F.sanitizeConfig(raw) : raw }).then(() => {
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

fetch("http://127.0.0.1:53472/health")
  .then((response) => {
    status.textContent = response.ok ? "Connected to Downpour" : "Downpour app not running";
    if (!response.ok) status.style.color = "#c0392b";
  })
  .catch(() => {
    status.textContent = "Downpour app not running";
    status.style.color = "#c0392b";
  });
