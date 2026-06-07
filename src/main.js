import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

/* ─── Element references ──────────────────────────────────────────────── */
const sidebar = document.querySelector("#sidebar");
const nav = document.querySelector("#nav");
const collapseToggle = document.querySelector("#collapse-toggle");
const views = document.querySelectorAll(".view");

const fab = document.querySelector("#fab");
const addModal = document.querySelector("#add-modal");
const addForm = document.querySelector("#add-form");
const addCancel = document.querySelector("#add-cancel");
const urlInput = document.querySelector("#url-input");
const filenameInput = document.querySelector("#filename-input");
const segmentsInput = document.querySelector("#segments-input");
const segmentsValue = document.querySelector("#segments-value");
const list = document.querySelector("#downloads");

const toastContainer = document.querySelector("#toast-container");

const statusSpeed = document.querySelector("#status-speed");
const statusActive = document.querySelector("#status-active");
const statusQueued = document.querySelector("#status-queued");
const pauseAllBtn = document.querySelector("#pause-all");
const resumeAllBtn = document.querySelector("#resume-all");

const searchInput = document.querySelector("#download-search");
const filterPills = document.querySelector("#filter-pills");
const densityToggle = document.querySelector("#density-toggle");

/* id -> DownloadItem, the source of truth for status-bar aggregates. */
const items = new Map();
/* id -> <li> element, so we can update progress in place. */
const rows = new Map();

/* Downloads view UI state. */
let activeFilter = "all"; // matches the data-filter pills
let searchQuery = ""; // lower-cased search text

/* ─── Formatting helpers ──────────────────────────────────────────────── */
function humanSize(bytes) {
  if (!bytes && bytes !== 0) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let n = bytes;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n.toFixed(1)} ${units[i]}`;
}

function humanSpeed(bytesPerSec) {
  return `${humanSize(bytesPerSec || 0)}/s`;
}

/* Parse an integer from any input, clamping to [min, max]; falls back on NaN. */
function clampInt(value, min, max, fallback) {
  const n = Math.trunc(Number(value));
  if (!Number.isFinite(n)) return fallback;
  return Math.min(max, Math.max(min, n));
}

function humanEta(seconds) {
  if (seconds === null || seconds === undefined || !isFinite(seconds)) {
    return "—";
  }
  const s = Math.max(0, Math.round(seconds));
  if (s < 60) return `${s}s`;
  if (s < 3600) {
    const m = Math.floor(s / 60);
    const rem = s % 60;
    return rem ? `${m}m ${rem}s` : `${m}m`;
  }
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return m ? `${h}h ${m}m` : `${h}h`;
}

/* Escape text destined for innerHTML to avoid breaking markup / injection. */
function escapeHtml(str) {
  return String(str ?? "").replace(
    /[&<>"']/g,
    (ch) =>
      ({
        "&": "&amp;",
        "<": "&lt;",
        ">": "&gt;",
        '"': "&quot;",
        "'": "&#39;",
      })[ch],
  );
}

/* ─── Toast notifications ─────────────────────────────────────────────── */
const TOAST_ICONS = { success: "✓", error: "⚠", info: "ℹ" };

/* Show a transient toast. `type` ∈ {success, error, info}. Auto-dismisses. */
function showToast(message, type = "info", timeout = 4000) {
  if (!toastContainer) return;
  const toast = document.createElement("div");
  toast.className = `toast toast--${type}`;
  toast.setAttribute("role", type === "error" ? "alert" : "status");
  toast.innerHTML = `
    <span class="toast__icon" aria-hidden="true">${TOAST_ICONS[type] || TOAST_ICONS.info}</span>
    <span class="toast__msg">${escapeHtml(message)}</span>
    <button class="toast__close" type="button" aria-label="Dismiss">✕</button>
  `;

  const dismiss = () => {
    if (toast.dataset.exiting) return;
    toast.dataset.exiting = "1";
    toast.classList.add("toast-exit");
    toast.addEventListener("animationend", () => toast.remove(), { once: true });
    // Fallback removal if the animation event never fires.
    setTimeout(() => toast.remove(), 400);
  };

  toast.querySelector(".toast__close").addEventListener("click", dismiss);
  toastContainer.appendChild(toast);
  if (timeout > 0) setTimeout(dismiss, timeout);
  return toast;
}

/* Pick a file icon from the download category, type, or filename extension. */
const CATEGORY_ICONS = {
  Videos: "🎬",
  Music: "🎵",
  Audio: "🎵",
  Images: "🖼️",
  Documents: "📄",
  Archives: "🗜️",
  Programs: "⚙️",
};

const EXTENSION_ICONS = {
  mp4: "🎬",
  mkv: "🎬",
  avi: "🎬",
  mov: "🎬",
  webm: "🎬",
  m4v: "🎬",
  mp3: "🎵",
  flac: "🎵",
  wav: "🎵",
  aac: "🎵",
  ogg: "🎵",
  jpg: "🖼️",
  jpeg: "🖼️",
  png: "🖼️",
  gif: "🖼️",
  svg: "🖼️",
  webp: "🖼️",
  pdf: "📄",
  doc: "📄",
  docx: "📄",
  xls: "📊",
  xlsx: "📊",
  ppt: "📊",
  pptx: "📊",
  txt: "📝",
  zip: "🗜️",
  rar: "🗜️",
  "7z": "🗜️",
  tar: "🗜️",
  gz: "🗜️",
  iso: "💿",
  exe: "⚙️",
  msi: "⚙️",
  dmg: "💿",
  deb: "📦",
  apk: "📦",
};

function fileIcon(item) {
  if (item.downloadType === "media") return "🎬";
  if (item.category && CATEGORY_ICONS[item.category]) {
    return CATEGORY_ICONS[item.category];
  }
  const name = item.filename || "";
  const dot = name.lastIndexOf(".");
  if (dot >= 0) {
    const ext = name.slice(dot + 1).toLowerCase();
    if (EXTENSION_ICONS[ext]) return EXTENSION_ICONS[ext];
  }
  return "📄";
}

/* Glyph shown on the status badge per status. */
const STATUS_GLYPHS = {
  downloading: "↓",
  queued: "•",
  paused: "⏸",
  complete: "✓",
  error: "⚠",
  merging: "⟳",
};

/* ─── Sidebar navigation + view transitions ───────────────────────────── */
function switchView(target) {
  nav.querySelectorAll(".nav-item").forEach((item) => {
    item.classList.toggle("active", item.dataset.view === target);
  });

  views.forEach((view) => {
    const isTarget = view.dataset.view === target;
    if (isTarget) {
      view.hidden = false;
      // Re-trigger the slide-in animation on each activation.
      view.classList.remove("view");
      void view.offsetWidth;
      view.classList.add("view");
      view.classList.add("active");
    } else {
      view.hidden = true;
      view.classList.remove("active");
    }
  });
}

nav.addEventListener("click", (e) => {
  const navItem = e.target.closest(".nav-item");
  if (navItem) switchView(navItem.dataset.view);
});

/* ─── Sidebar collapse ────────────────────────────────────────────────── */
collapseToggle.addEventListener("click", () => {
  // `expanded` overrides the responsive auto-collapse; `collapsed` forces it.
  if (sidebar.classList.contains("collapsed")) {
    sidebar.classList.remove("collapsed");
    sidebar.classList.add("expanded");
  } else {
    sidebar.classList.add("collapsed");
    sidebar.classList.remove("expanded");
  }
});

/* ─── Add Download modal (opened by the FAB) ──────────────────────────── */
function openModal() {
  addModal.hidden = false;
  urlInput.focus();
}

function closeModal() {
  addModal.hidden = true;
  urlInput.value = "";
  filenameInput.value = "";
  segmentsInput.value = "4";
  segmentsValue.textContent = "4";
}

fab.addEventListener("click", openModal);
addCancel.addEventListener("click", closeModal);
addModal.addEventListener("click", (e) => {
  if (e.target === addModal) closeModal();
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && !addModal.hidden) closeModal();
});

/* Live-update the segment count label as the slider moves. */
segmentsInput.addEventListener("input", () => {
  segmentsValue.textContent = segmentsInput.value;
});

addForm.addEventListener("submit", async (e) => {
  e.preventDefault();
  const url = urlInput.value.trim();
  if (!url) return;

  // Client-side validation before invoking the command (Req 11.2 bounds).
  const segments = clampInt(segmentsInput.value, 1, 32, 4);
  const filename = filenameInput.value.trim();

  try {
    const args = { url, segments };
    if (filename) args.filename = filename;
    const item = await invoke("start_download", args);
    renderRow(item);
    upsertItem(item);
    closeModal();
    showToast(`Added “${item.filename || url}” to the queue`, "success");
  } catch (err) {
    showToast(`Could not start download: ${err}`, "error");
  }
});

/* ─── Download cards ──────────────────────────────────────────────────── */

/* Per-status action buttons. Returns an array of {action, title, glyph}. */
function actionsFor(status) {
  switch (status) {
    case "downloading":
    case "merging":
      return [
        { action: "pause", title: "Pause", glyph: "⏸", danger: false },
        { action: "cancel", title: "Cancel", glyph: "✕", danger: true },
      ];
    case "queued":
      return [
        { action: "pause", title: "Pause", glyph: "⏸", danger: false },
        { action: "cancel", title: "Cancel", glyph: "✕", danger: true },
      ];
    case "paused":
      return [
        { action: "resume", title: "Resume", glyph: "▶", danger: false },
        { action: "cancel", title: "Cancel", glyph: "✕", danger: true },
      ];
    case "error":
      return [
        { action: "resume", title: "Retry", glyph: "↻", danger: false },
        { action: "cancel", title: "Remove", glyph: "✕", danger: true },
      ];
    case "complete":
      return [{ action: "cancel", title: "Remove", glyph: "✕", danger: true }];
    default:
      return [{ action: "cancel", title: "Cancel", glyph: "✕", danger: true }];
  }
}

function actionButtonsHtml(status) {
  return actionsFor(status)
    .map(
      (a) =>
        `<button class="btn-icon${a.danger ? " btn-icon--danger" : ""}" type="button" data-action="${a.action}" title="${a.title}" aria-label="${a.title}">${a.glyph}</button>`,
    )
    .join("");
}

function renderRow(item) {
  let li = rows.get(item.id);
  if (!li) {
    document.querySelector(".empty")?.remove();
    document.querySelector(".downloads__no-match")?.remove();
    li = document.createElement("li");
    li.className = "download-card";
    li.dataset.id = item.id;
    list.prepend(li);
    rows.set(item.id, li);
  }

  const pct =
    item.totalSize > 0
      ? Math.min(100, Math.round((item.downloaded / item.totalSize) * 100))
      : 0;
  const status = item.status || "queued";

  li.dataset.status = status;
  li.dataset.filename = (item.filename || "").toLowerCase();

  const showSpeed = status === "downloading" && item.speed > 0;
  const sizeText =
    item.totalSize > 0
      ? `${humanSize(item.downloaded)} / ${humanSize(item.totalSize)}`
      : humanSize(item.downloaded);

  li.innerHTML = `
    <div class="download-card__head">
      <span class="download-card__icon" aria-hidden="true">${fileIcon(item)}</span>
      <span class="download-card__filename" title="${escapeHtml(item.url)}">${escapeHtml(item.filename)}</span>
      ${showSpeed ? `<span class="download-card__speed">↓ ${humanSpeed(item.speed)}</span>` : ""}
      <span class="badge badge--${status}">${STATUS_GLYPHS[status] || ""} ${status}</span>
      <span class="download-card__inline-meta">${pct}% · ${sizeText}</span>
      <span class="download-card__actions">${actionButtonsHtml(status)}</span>
    </div>
    <div class="progress-bar">
      <div class="progress-bar__fill progress-bar__fill--${status}" style="width:${pct}%"></div>
    </div>
    <div class="download-card__meta">
      <span class="download-card__size">${sizeText}</span>
      <span class="download-card__pct">${pct}%</span>
      ${status === "downloading" && item.eta != null ? `<span class="download-card__eta">ETA ${humanEta(item.eta)}</span>` : ""}
      <span class="download-card__meta-spacer"></span>
      ${
        status === "error" && item.errorMessage
          ? `<span class="download-card__error" title="${escapeHtml(item.errorMessage)}">${escapeHtml(item.errorMessage)}</span>`
          : item.category
            ? `<span class="download-card__category">${escapeHtml(item.category)}</span>`
            : ""
      }
    </div>
  `;

  applyVisibility(li, item);
}

/* ─── Filtering + search ──────────────────────────────────────────────── */
function matchesFilter(item) {
  if (activeFilter !== "all" && item.status !== activeFilter) return false;
  if (searchQuery && !(item.filename || "").toLowerCase().includes(searchQuery)) {
    return false;
  }
  return true;
}

function applyVisibility(li, item) {
  li.hidden = !matchesFilter(item);
}

function refreshVisibility() {
  let visible = 0;
  for (const [id, li] of rows) {
    const item = items.get(id);
    if (!item) continue;
    const show = matchesFilter(item);
    li.hidden = !show;
    if (show) visible++;
  }
  updateNoMatchState(visible);
}

/* Show a "no matches" hint when filters/search hide every card. */
function updateNoMatchState(visibleCount) {
  const existing = list.querySelector(".downloads__no-match");
  if (rows.size > 0 && visibleCount === 0) {
    if (!existing) {
      const li = document.createElement("li");
      li.className = "downloads__no-match";
      li.textContent = "No downloads match the current filter.";
      list.appendChild(li);
    }
  } else if (existing) {
    existing.remove();
  }
}

/* ─── Status bar aggregates ───────────────────────────────────────────── */
function upsertItem(item) {
  items.set(item.id, item);
  refreshStatusBar();
}

function refreshStatusBar() {
  let active = 0;
  let queued = 0;
  let totalSpeed = 0;
  for (const item of items.values()) {
    if (item.status === "downloading") {
      active++;
      totalSpeed += item.speed || 0;
    } else if (item.status === "queued") {
      queued++;
    }
  }
  statusSpeed.textContent = `↓ ${humanSpeed(totalSpeed)}`;
  statusActive.textContent = `${active} active`;
  statusQueued.textContent = `${queued} queued`;
}

pauseAllBtn.addEventListener("click", async () => {
  try {
    await invoke("pause_all");
    showToast("Paused all active downloads", "info");
  } catch (err) {
    showToast(`Could not pause all: ${err}`, "error");
  }
});

resumeAllBtn.addEventListener("click", async () => {
  try {
    await invoke("resume_all");
    showToast("Resuming downloads", "info");
  } catch (err) {
    showToast(`Could not resume all: ${err}`, "error");
  }
});

/* ─── Per-card actions (pause / resume / cancel) ──────────────────────── */
const ACTION_COMMANDS = {
  pause: "pause_download",
  resume: "resume_download",
  cancel: "cancel_download",
};

list.addEventListener("click", async (e) => {
  const btn = e.target.closest("button[data-action]");
  if (!btn) return;
  const card = btn.closest(".download-card");
  const id = card?.dataset.id;
  const command = ACTION_COMMANDS[btn.dataset.action];
  if (!id || !command) return;

  btn.disabled = true;
  try {
    await invoke(command, { id });
    if (btn.dataset.action === "cancel") {
      // Optimistically drop the card; queue-changed will reconcile.
      card.remove();
      rows.delete(id);
      items.delete(id);
      refreshStatusBar();
      if (rows.size === 0) showEmptyState();
    }
  } catch (err) {
    showToast(`Action failed: ${err}`, "error");
  } finally {
    btn.disabled = false;
  }
});

/* ─── Status filter pills ─────────────────────────────────────────────── */
filterPills.addEventListener("click", (e) => {
  const pill = e.target.closest(".pill");
  if (!pill) return;
  activeFilter = pill.dataset.filter;
  filterPills.querySelectorAll(".pill").forEach((p) => {
    const isActive = p === pill;
    p.classList.toggle("active", isActive);
    p.setAttribute("aria-selected", isActive ? "true" : "false");
  });
  refreshVisibility();
});

/* ─── Search box ──────────────────────────────────────────────────────── */
searchInput.addEventListener("input", () => {
  searchQuery = searchInput.value.trim().toLowerCase();
  refreshVisibility();
});

/* ─── Compact / comfortable density toggle ────────────────────────────── */
densityToggle.addEventListener("click", () => {
  const compact = list.classList.toggle("view-compact");
  list.classList.toggle("view-comfortable", !compact);
  densityToggle.setAttribute("aria-pressed", compact ? "true" : "false");
  densityToggle.querySelector(".density-toggle__label").textContent = compact
    ? "Comfortable"
    : "Compact";
});

/* Restore the empty-state placeholder when the list becomes empty. */
function showEmptyState() {
  if (list.querySelector(".empty")) return;
  const li = document.createElement("li");
  li.className = "empty";
  li.textContent = "No downloads yet. Use the + button to add one.";
  list.appendChild(li);
}

/* ─── Core events ─────────────────────────────────────────────────────── */
// Full DownloadItem state during a download (Req 12.1).
listen("download-progress", (event) => {
  const item = event.payload;
  renderRow(item);
  upsertItem(item);
});

// Ordered queue summaries when the queue changes (Req 12.3). Each summary has
// { id, filename, status, position }. Reconcile statuses and re-render cards.
listen("queue-changed", (event) => {
  const summaries = event.payload || [];
  const seen = new Set();
  for (const summary of summaries) {
    seen.add(summary.id);
    const existing = items.get(summary.id) || {};
    const merged = { ...existing, ...summary };
    items.set(summary.id, merged);
    // Keep the card's status badge / actions in sync with the queue.
    if (rows.has(summary.id)) {
      renderRow(merged);
    }
  }
  // Drop items (and their cards) no longer present in the queue.
  for (const id of [...items.keys()]) {
    if (!seen.has(id)) {
      items.delete(id);
      rows.get(id)?.remove();
      rows.delete(id);
    }
  }
  if (rows.size === 0) showEmptyState();
  refreshStatusBar();
  refreshVisibility();
});

/* ─── Launch hydration ────────────────────────────────────────────────── */
(async () => {
  try {
    const existing = await invoke("list_downloads");
    existing.forEach((item) => {
      renderRow(item);
      upsertItem(item);
    });
  } catch (_) {
    /* core not ready yet — ignore */
  }
})();

/* ═══════════════════════════════════════════════════════════════════════
   Task 14.3 — Queue view, Media view, Settings view
   These modules reuse the helpers above (invoke, listen, items, humanSize,
   fileIcon, escapeHtml, showToast, clampInt) and extend — never replace —
   the Downloads view wired up in 14.1 / 14.2.
   ═══════════════════════════════════════════════════════════════════════ */

/* Refresh the relevant view's data when the user navigates to it. Runs in
   addition to the existing nav handler that performs the visual switch. */
nav.addEventListener("click", (e) => {
  const navItem = e.target.closest(".nav-item");
  if (!navItem) return;
  if (navItem.dataset.view === "queue") refreshQueueView();
  if (navItem.dataset.view === "settings") loadSettingsForm();
});

/* ─── Queue view ──────────────────────────────────────────────────────── */
const queueList = document.querySelector("#queue-list");
const queueRefresh = document.querySelector("#queue-refresh");

/* Per-status action buttons for a queue row. */
function queueActionsFor(status) {
  switch (status) {
    case "downloading":
    case "merging":
      return [
        { action: "pause", title: "Pause", glyph: "⏸", danger: false },
        { action: "remove", title: "Remove", glyph: "✕", danger: true },
      ];
    case "paused":
    case "error":
      return [
        { action: "resume", title: "Start", glyph: "▶", danger: false },
        { action: "remove", title: "Remove", glyph: "✕", danger: true },
      ];
    case "queued":
      return [
        { action: "pause", title: "Hold", glyph: "⏸", danger: false },
        { action: "remove", title: "Remove", glyph: "✕", danger: true },
      ];
    default:
      return [{ action: "remove", title: "Remove", glyph: "✕", danger: true }];
  }
}

function queueActionsHtml(status) {
  return queueActionsFor(status)
    .map(
      (a) =>
        `<button class="btn-icon${a.danger ? " btn-icon--danger" : ""}" type="button" data-action="${a.action}" title="${a.title}" aria-label="${a.title}">${a.glyph}</button>`,
    )
    .join("");
}

/* Build the queue list from a fresh snapshot of ordered DownloadItems. */
function renderQueue(orderedItems) {
  queueList.innerHTML = "";
  if (!orderedItems.length) {
    const li = document.createElement("li");
    li.className = "empty";
    li.textContent = "Queue is empty.";
    queueList.appendChild(li);
    return;
  }

  orderedItems.forEach((item, index) => {
    const status = item.status || "queued";
    const li = document.createElement("li");
    li.className = "queue-item";
    li.dataset.id = item.id;
    li.dataset.status = status;
    li.draggable = true;
    li.innerHTML = `
      <span class="queue-item__handle" aria-hidden="true">⠿</span>
      <span class="queue-item__pos">${index + 1}</span>
      <span class="queue-item__icon" aria-hidden="true">${fileIcon(item)}</span>
      <span class="queue-item__name" title="${escapeHtml(item.filename || item.url)}">${escapeHtml(item.filename || item.url)}</span>
      <span class="badge badge--${status}">${status}</span>
      <span class="queue-item__actions">${queueActionsHtml(status)}</span>
    `;
    queueList.appendChild(li);
  });
}

/* Fetch the ordered queue state from the core and render it. */
async function refreshQueueView() {
  try {
    const state = await invoke("get_queue_state");
    renderQueue(state || []);
  } catch (err) {
    showToast(`Could not load queue: ${err}`, "error");
  }
}

queueRefresh.addEventListener("click", refreshQueueView);

/* Queue row actions: start (resume), pause, remove. */
const QUEUE_ACTION_COMMANDS = {
  pause: "pause_download",
  resume: "resume_download",
  remove: "remove_download",
};

queueList.addEventListener("click", async (e) => {
  const btn = e.target.closest("button[data-action]");
  if (!btn) return;
  const row = btn.closest(".queue-item");
  const id = row?.dataset.id;
  const command = QUEUE_ACTION_COMMANDS[btn.dataset.action];
  if (!id || !command) return;

  btn.disabled = true;
  try {
    await invoke(command, { id });
    await refreshQueueView();
  } catch (err) {
    showToast(`Action failed: ${err}`, "error");
    btn.disabled = false;
  }
});

/* Drag-to-reorder (Req 3.3). On drop, invoke reorder_download(id, position). */
let dragSrcId = null;

queueList.addEventListener("dragstart", (e) => {
  const row = e.target.closest(".queue-item");
  if (!row) return;
  dragSrcId = row.dataset.id;
  row.classList.add("dragging");
  e.dataTransfer.effectAllowed = "move";
  // Firefox requires data to be set for dragging to start.
  e.dataTransfer.setData("text/plain", dragSrcId);
});

queueList.addEventListener("dragend", () => {
  dragSrcId = null;
  queueList.querySelectorAll(".queue-item").forEach((el) => {
    el.classList.remove("dragging", "drop-before", "drop-after");
  });
});

queueList.addEventListener("dragover", (e) => {
  const row = e.target.closest(".queue-item");
  if (!row || row.dataset.id === dragSrcId) return;
  e.preventDefault();
  e.dataTransfer.dropEffect = "move";
  const rect = row.getBoundingClientRect();
  const after = e.clientY - rect.top > rect.height / 2;
  queueList.querySelectorAll(".queue-item").forEach((el) => {
    el.classList.remove("drop-before", "drop-after");
  });
  row.classList.add(after ? "drop-after" : "drop-before");
});

queueList.addEventListener("drop", async (e) => {
  const row = e.target.closest(".queue-item");
  if (!row || !dragSrcId) return;
  e.preventDefault();

  const ids = [...queueList.querySelectorAll(".queue-item")].map((el) => el.dataset.id);
  const fromIndex = ids.indexOf(dragSrcId);
  let targetIndex = ids.indexOf(row.dataset.id);
  if (fromIndex < 0 || targetIndex < 0 || fromIndex === targetIndex) return;

  const rect = row.getBoundingClientRect();
  const after = e.clientY - rect.top > rect.height / 2;
  if (after) targetIndex += 1;
  // Account for removing the source item before re-inserting.
  if (fromIndex < targetIndex) targetIndex -= 1;
  targetIndex = clampInt(targetIndex, 0, ids.length - 1, fromIndex);

  const movedId = dragSrcId;
  try {
    await invoke("reorder_download", { id: movedId, position: targetIndex });
    await refreshQueueView();
  } catch (err) {
    showToast(`Could not reorder: ${err}`, "error");
    await refreshQueueView();
  }
});

/* Keep the queue view live as the core emits queue changes. */
listen("queue-changed", () => {
  if (!document.querySelector("#view-queue").hidden) refreshQueueView();
});

/* ─── Media view ──────────────────────────────────────────────────────── */
const mediaForm = document.querySelector("#media-form");
const mediaUrl = document.querySelector("#media-url");
const mediaStatus = document.querySelector("#media-status");
const mediaInfo = document.querySelector("#media-info");
const mediaThumb = document.querySelector("#media-thumb");
const mediaTitle = document.querySelector("#media-title");
const mediaMeta = document.querySelector("#media-meta");
const mediaFormatSelect = document.querySelector("#media-format");
const mediaFilename = document.querySelector("#media-filename");
const mediaDownloadBtn = document.querySelector("#media-download");
const mediaDownloads = document.querySelector("#media-downloads");
const mediaExtractBtn = document.querySelector("#media-extract");

/* The URL whose formats are currently displayed. */
let mediaCurrentUrl = "";
/* id -> <li> for media download cards in the media view. */
const mediaRows = new Map();

function setMediaStatus(message, isError = false) {
  if (!message) {
    mediaStatus.hidden = true;
    mediaStatus.textContent = "";
    return;
  }
  mediaStatus.hidden = false;
  mediaStatus.textContent = message;
  mediaStatus.classList.toggle("media__status--error", isError);
}

function humanDuration(seconds) {
  if (seconds == null || !isFinite(seconds)) return "";
  const s = Math.max(0, Math.round(seconds));
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const sec = s % 60;
  const pad = (n) => String(n).padStart(2, "0");
  return h > 0 ? `${h}:${pad(m)}:${pad(sec)}` : `${m}:${pad(sec)}`;
}

/* Describe a media format for the <option> label. */
function formatLabel(fmt) {
  const kind = fmt.hasVideo && fmt.hasAudio
    ? "video+audio"
    : fmt.hasVideo
      ? "video only"
      : fmt.hasAudio
        ? "audio only"
        : "";
  const size = fmt.filesize ? ` · ${humanSize(fmt.filesize)}` : "";
  const ext = fmt.ext ? ` · ${fmt.ext}` : "";
  return `${fmt.quality || fmt.formatId}${ext}${kind ? ` · ${kind}` : ""}${size}`;
}

function populateMediaInfo(info) {
  mediaTitle.textContent = info.title || "Untitled";
  const bits = [];
  if (info.platform) bits.push(info.platform);
  if (info.duration != null) bits.push(humanDuration(info.duration));
  mediaMeta.textContent = bits.join(" · ");

  if (info.thumbnail) {
    mediaThumb.src = info.thumbnail;
    mediaThumb.hidden = false;
  } else {
    mediaThumb.removeAttribute("src");
    mediaThumb.hidden = true;
  }

  mediaFormatSelect.innerHTML = "";
  const formats = info.formats || [];
  if (!formats.length) {
    const opt = document.createElement("option");
    opt.value = "";
    opt.textContent = "No formats available";
    mediaFormatSelect.appendChild(opt);
    mediaDownloadBtn.disabled = true;
  } else {
    formats.forEach((fmt) => {
      const opt = document.createElement("option");
      opt.value = fmt.formatId;
      opt.textContent = formatLabel(fmt);
      mediaFormatSelect.appendChild(opt);
    });
    mediaDownloadBtn.disabled = false;
  }

  mediaInfo.hidden = false;
}

mediaForm.addEventListener("submit", async (e) => {
  e.preventDefault();
  const url = mediaUrl.value.trim();
  if (!url) return;

  mediaExtractBtn.disabled = true;
  mediaInfo.hidden = true;
  setMediaStatus("Extracting media info…");
  try {
    const info = await invoke("extract_media_info", { url, cookies: null });
    mediaCurrentUrl = url;
    populateMediaInfo(info);
    setMediaStatus("");
  } catch (err) {
    setMediaStatus(`Extraction failed: ${err}`, true);
    showToast(`Media extraction failed: ${err}`, "error");
  } finally {
    mediaExtractBtn.disabled = false;
  }
});

mediaDownloadBtn.addEventListener("click", async () => {
  const formatId = mediaFormatSelect.value;
  if (!mediaCurrentUrl || !formatId) {
    showToast("Pick a format first", "info");
    return;
  }
  const filename = mediaFilename.value.trim();

  mediaDownloadBtn.disabled = true;
  try {
    const args = { url: mediaCurrentUrl, formatId };
    if (filename) args.filename = filename;
    const item = await invoke("start_media_download", args);
    renderMediaRow(item);
    upsertItem(item);
    showToast(`Started media download: ${item.filename || mediaCurrentUrl}`, "success");
  } catch (err) {
    showToast(`Media download failed: ${err}`, "error");
  } finally {
    mediaDownloadBtn.disabled = false;
  }
});

/* Render a media download card into the media view's own list. */
function renderMediaRow(item) {
  if (item.downloadType && item.downloadType !== "media") return;

  let li = mediaRows.get(item.id);
  if (!li) {
    mediaDownloads.querySelector(".empty")?.remove();
    li = document.createElement("li");
    li.className = "download-card";
    li.dataset.id = item.id;
    mediaDownloads.prepend(li);
    mediaRows.set(item.id, li);
  }

  // Media downloads report progress as a percentage (total_size === 100).
  const pct = item.totalSize > 0
    ? Math.min(100, Math.round((item.downloaded / item.totalSize) * 100))
    : 0;
  const status = item.status || "downloading";
  li.dataset.status = status;

  li.innerHTML = `
    <div class="download-card__head">
      <span class="download-card__icon" aria-hidden="true">🎬</span>
      <span class="download-card__filename" title="${escapeHtml(item.url)}">${escapeHtml(item.filename)}</span>
      ${status === "downloading" && item.speed > 0 ? `<span class="download-card__speed">↓ ${humanSpeed(item.speed)}</span>` : ""}
      <span class="badge badge--${status}">${status}</span>
      <span class="download-card__actions">
        ${status === "downloading" || status === "merging"
          ? `<button class="btn-icon btn-icon--danger" type="button" data-media-action="cancel" title="Cancel" aria-label="Cancel">✕</button>`
          : `<button class="btn-icon btn-icon--danger" type="button" data-media-action="dismiss" title="Dismiss" aria-label="Dismiss">✕</button>`}
      </span>
    </div>
    <div class="progress-bar">
      <div class="progress-bar__fill progress-bar__fill--${status}" style="width:${pct}%"></div>
    </div>
    <div class="download-card__meta">
      <span class="download-card__pct">${pct}%</span>
      ${status === "error" && item.errorMessage
        ? `<span class="download-card__meta-spacer"></span><span class="download-card__error" title="${escapeHtml(item.errorMessage)}">${escapeHtml(item.errorMessage)}</span>`
        : ""}
    </div>
  `;
}

mediaDownloads.addEventListener("click", async (e) => {
  const btn = e.target.closest("button[data-media-action]");
  if (!btn) return;
  const card = btn.closest(".download-card");
  const id = card?.dataset.id;
  if (!id) return;

  if (btn.dataset.mediaAction === "dismiss") {
    card.remove();
    mediaRows.delete(id);
    if (mediaRows.size === 0) showMediaEmptyState();
    return;
  }

  btn.disabled = true;
  try {
    await invoke("cancel_media_download", { id });
    showToast("Media download cancelled", "info");
  } catch (err) {
    showToast(`Could not cancel: ${err}`, "error");
    btn.disabled = false;
  }
});

function showMediaEmptyState() {
  if (mediaDownloads.querySelector(".empty")) return;
  const li = document.createElement("li");
  li.className = "empty";
  li.textContent = "No media downloads yet.";
  mediaDownloads.appendChild(li);
}

/* Mirror media-type progress into the media view list. */
listen("download-progress", (event) => {
  const item = event.payload;
  if (item && item.downloadType === "media") renderMediaRow(item);
});

/* ─── Settings view ───────────────────────────────────────────────────── */
const settingsForm = document.querySelector("#settings-form");
const settingsReload = document.querySelector("#settings-reload");
const categoriesList = document.querySelector("#categories-list");
const addCategoryBtn = document.querySelector("#add-category");

const setMaxConcurrent = document.querySelector("#set-max-concurrent");
const setSegments = document.querySelector("#set-default-segments");
const setSpeedLimit = document.querySelector("#set-speed-limit");
const setDownloadDir = document.querySelector("#set-download-dir");
const setAutoCategorize = document.querySelector("#set-auto-categorize");
const setYtdlpPath = document.querySelector("#set-ytdlp-path");
const setFfmpegPath = document.querySelector("#set-ffmpeg-path");

/* Settings bounds (mirror settings.rs / Req 11). */
const SETTINGS_BOUNDS = {
  maxConcurrent: { min: 1, max: 10 },
  segments: { min: 1, max: 32 },
  maxCategories: 20,
  maxExtensionsPerCategory: 50,
};

/* The last-loaded settings, kept so we preserve fields the form doesn't edit. */
let loadedSettings = null;
let settingsLoaded = false;

function setFieldError(inputEl, message) {
  const key = inputEl.id;
  const errEl = settingsForm.querySelector(`[data-error-for="${key}"]`);
  if (message) {
    inputEl.classList.add("invalid");
    if (errEl) {
      errEl.textContent = message;
      errEl.hidden = false;
    }
  } else {
    inputEl.classList.remove("invalid");
    if (errEl) errEl.hidden = true;
  }
}

function clearCategoriesError() {
  const errEl = settingsForm.querySelector('[data-error-for="categories"]');
  if (errEl) errEl.hidden = true;
}

/* Build a single editable category row. */
function categoryRowHtml(category = "", extensions = []) {
  return `
    <input class="input category-row__name" type="text" placeholder="Category" value="${escapeHtml(category)}" aria-label="Category name" />
    <input class="input category-row__exts" type="text" placeholder=".mp4, .mkv" value="${escapeHtml(extensions.join(", "))}" aria-label="Extensions" />
    <button class="btn-icon btn-icon--danger category-row__remove" type="button" title="Remove category" aria-label="Remove category">✕</button>
  `;
}

function addCategoryRow(category = "", extensions = []) {
  const row = document.createElement("div");
  row.className = "category-row";
  row.innerHTML = categoryRowHtml(category, extensions);
  categoriesList.appendChild(row);
}

categoriesList.addEventListener("click", (e) => {
  const btn = e.target.closest(".category-row__remove");
  if (!btn) return;
  btn.closest(".category-row")?.remove();
});

addCategoryBtn.addEventListener("click", () => addCategoryRow());

/* Populate the form from a settings object. Speed limit shown in KB/s. */
function fillSettingsForm(settings) {
  loadedSettings = settings;
  setMaxConcurrent.value = settings.maxConcurrent ?? 3;
  setSegments.value = settings.defaultSegments ?? 4;
  setSpeedLimit.value = Math.round((settings.speedLimit ?? 0) / 1024);
  setDownloadDir.value = settings.downloadDir ?? "";
  setAutoCategorize.checked = !!settings.autoCategorize;
  setYtdlpPath.value = settings.ytdlpPath ?? "";
  setFfmpegPath.value = settings.ffmpegPath ?? "";

  categoriesList.innerHTML = "";
  (settings.categories || []).forEach((c) => {
    addCategoryRow(c.category, c.extensions || []);
  });

  [setMaxConcurrent, setSegments, setSpeedLimit, setDownloadDir].forEach((el) =>
    setFieldError(el, null),
  );
  clearCategoriesError();
}

async function loadSettingsForm(force = false) {
  if (settingsLoaded && !force) return;
  try {
    const settings = await invoke("get_settings");
    fillSettingsForm(settings);
    settingsLoaded = true;
  } catch (err) {
    showToast(`Could not load settings: ${err}`, "error");
  }
}

settingsReload.addEventListener("click", () => loadSettingsForm(true));

/* Read + validate the form. Returns a settings object or null on error. */
function collectSettings() {
  let ok = true;

  const maxConcurrent = clampInt(setMaxConcurrent.value, -Infinity, Infinity, NaN);
  if (
    !Number.isFinite(maxConcurrent) ||
    maxConcurrent < SETTINGS_BOUNDS.maxConcurrent.min ||
    maxConcurrent > SETTINGS_BOUNDS.maxConcurrent.max
  ) {
    setFieldError(setMaxConcurrent, "Must be between 1 and 10.");
    ok = false;
  } else {
    setFieldError(setMaxConcurrent, null);
  }

  const segments = clampInt(setSegments.value, -Infinity, Infinity, NaN);
  if (
    !Number.isFinite(segments) ||
    segments < SETTINGS_BOUNDS.segments.min ||
    segments > SETTINGS_BOUNDS.segments.max
  ) {
    setFieldError(setSegments, "Must be between 1 and 32.");
    ok = false;
  } else {
    setFieldError(setSegments, null);
  }

  const speedKb = Number(setSpeedLimit.value);
  if (!Number.isFinite(speedKb) || speedKb < 0 || !Number.isInteger(speedKb)) {
    setFieldError(setSpeedLimit, "Must be 0 or a positive whole number.");
    ok = false;
  } else {
    setFieldError(setSpeedLimit, null);
  }

  const downloadDir = setDownloadDir.value.trim();
  if (!downloadDir) {
    setFieldError(setDownloadDir, "Download directory is required.");
    ok = false;
  } else {
    setFieldError(setDownloadDir, null);
  }

  // Categories: enforce the same caps the core does (Req 11.6).
  const rows = [...categoriesList.querySelectorAll(".category-row")];
  const categories = [];
  clearCategoriesError();
  if (rows.length > SETTINGS_BOUNDS.maxCategories) {
    const errEl = settingsForm.querySelector('[data-error-for="categories"]');
    if (errEl) {
      errEl.textContent = `At most ${SETTINGS_BOUNDS.maxCategories} categories allowed.`;
      errEl.hidden = false;
    }
    ok = false;
  }
  for (const row of rows) {
    const name = row.querySelector(".category-row__name").value.trim();
    const exts = row
      .querySelector(".category-row__exts")
      .value.split(",")
      .map((s) => s.trim())
      .filter(Boolean)
      .map((s) => (s.startsWith(".") ? s.toLowerCase() : `.${s.toLowerCase()}`));
    if (!name) continue;
    if (exts.length > SETTINGS_BOUNDS.maxExtensionsPerCategory) {
      const errEl = settingsForm.querySelector('[data-error-for="categories"]');
      if (errEl) {
        errEl.textContent = `“${name}” has too many extensions (max ${SETTINGS_BOUNDS.maxExtensionsPerCategory}).`;
        errEl.hidden = false;
      }
      ok = false;
    }
    // Preserve mimePatterns / subfolder from the previously loaded rule.
    const prior = (loadedSettings?.categories || []).find((c) => c.category === name);
    categories.push({
      category: name,
      extensions: exts,
      mimePatterns: prior?.mimePatterns || [],
      subfolder: prior?.subfolder || name,
    });
  }

  if (!ok) return null;

  // Merge onto the loaded settings so unedited fields are preserved.
  const base = loadedSettings || {};
  return {
    ...base,
    maxConcurrent,
    downloadDir,
    defaultSegments: segments,
    speedLimit: speedKb * 1024,
    autoCategorize: setAutoCategorize.checked,
    categories,
    ytdlpPath: setYtdlpPath.value.trim() || null,
    ffmpegPath: setFfmpegPath.value.trim() || null,
  };
}

settingsForm.addEventListener("submit", async (e) => {
  e.preventDefault();
  const collected = collectSettings();
  if (!collected) {
    showToast("Please fix the highlighted settings", "error");
    return;
  }

  const saveBtn = document.querySelector("#settings-save");
  saveBtn.disabled = true;
  try {
    const saved = await invoke("update_settings", { newSettings: collected });
    fillSettingsForm(saved);
    showToast("Settings saved", "success");
  } catch (err) {
    showToast(`Could not save settings: ${err}`, "error");
  } finally {
    saveBtn.disabled = false;
  }
});
