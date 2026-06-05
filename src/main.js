import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

const form = document.querySelector("#add-form");
const urlInput = document.querySelector("#url-input");
const list = document.querySelector("#downloads");

// id -> <li> element, so we can update progress in place.
const rows = new Map();

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

function renderRow(item) {
  let li = rows.get(item.id);
  if (!li) {
    document.querySelector(".empty")?.remove();
    li = document.createElement("li");
    li.className = "download";
    list.prepend(li);
    rows.set(item.id, li);
  }

  const pct =
    item.total_size > 0
      ? Math.min(100, Math.round((item.downloaded / item.total_size) * 100))
      : 0;

  li.innerHTML = `
    <div class="row-head">
      <span class="name" title="${item.url}">${item.filename}</span>
      <span class="status status-${item.status}">${item.status}</span>
    </div>
    <div class="bar"><div class="bar-fill" style="width:${pct}%"></div></div>
    <div class="meta">
      <span>${humanSize(item.downloaded)} / ${humanSize(item.total_size)}</span>
      <span>${pct}%</span>
    </div>
  `;
}

form.addEventListener("submit", async (e) => {
  e.preventDefault();
  const url = urlInput.value.trim();
  if (!url) return;
  urlInput.value = "";
  try {
    // Returns the created DownloadItem so we can render immediately.
    const item = await invoke("start_download", { url });
    renderRow(item);
  } catch (err) {
    alert(`Could not start download: ${err}`);
  }
});

// Live progress events emitted from the Rust core.
listen("download-progress", (event) => {
  renderRow(event.payload);
});

// On launch, hydrate any in-flight/queued downloads.
(async () => {
  try {
    const items = await invoke("list_downloads");
    items.forEach(renderRow);
  } catch (_) {
    /* core not ready yet — ignore */
  }
})();
