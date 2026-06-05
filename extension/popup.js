const checkbox = document.querySelector("#enabled");
const status = document.querySelector("#status");

// Restore the saved on/off state.
chrome.storage.local.get({ enabled: true }).then(({ enabled }) => {
  checkbox.checked = enabled;
});

checkbox.addEventListener("change", () => {
  chrome.storage.local.set({ enabled: checkbox.checked });
});

// Show whether the Downpour app is reachable.
fetch("http://127.0.0.1:53472/health")
  .then((r) => (r.ok ? (status.textContent = "Connected to Downpour ✓") : Promise.reject()))
  .catch(() => {
    status.textContent = "Downpour app not running";
    status.style.color = "#c0392b";
  });
