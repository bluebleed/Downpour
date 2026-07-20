---
type: reference
title: Disk-Space Cleanup & App Improvements — Plan & Tasks
created: 2026-07-05
last-updated: 2026-07-05
load-behavior: on-demand
---

# Disk-Space Cleanup & App Improvements — Plan & Tasks

**Date:** 2026-07-05
**Status:** In progress (implementation complete, awaiting manual verification)
**Owner:** Downpour
**Executor:** Opus (follow phases in order; each phase is independently shippable)

---

## 0. Why the project is 13 GB (root-cause, measured 2026-07-05)

| Path | Size | What it is |
|---|---|---|
| `src-tauri/target/debug/deps/` | **7.0 GB** | Compiled dependency artifacts **with full debuginfo** (tokio `full`, tauri, axum, reqwest, etc.) |
| `src-tauri/target/debug/incremental/` | **4.5 GB** | Incremental-compilation caches (grow with every edit-rebuild cycle) |
| `src-tauri/target/debug/build/` | 0.6 GB | Build-script outputs |
| Everything else (src, node_modules, .git, assets) | < 60 MB | The actual project |

**Conclusion:** ~99.5 % of the footprint is regenerable Rust build cache, already
gitignored. The app itself is tiny. Full debuginfo for ~400 dependency crates is the
main driver; nobody steps through dependency code in a debugger here.

Secondary (repo bloat, committed to git): `Logo 2.png` (6.3 MB) at repo root, plus
stray `test_out.txt` / `test_out2.txt`.

---

## Phase 1 — Reclaim the 13 GB and stop it regrowing

1. **Trim dev-profile debuginfo.** Append to `src-tauri/Cargo.toml`:

   ```toml
   [profile.dev]
   # Keep file/line info for our own backtraces, drop the rest.
   debug = "line-tables-only"

   [profile.dev.package."*"]
   # No debuginfo for the ~400 dependency crates — the biggest target/ cost.
   debug = false
   ```

2. **Wipe the stale cache** (safe — fully regenerable):

   ```bash
   cargo clean --manifest-path src-tauri/Cargo.toml
   ```

   Frees ~13 GB immediately. The next `npm run tauri dev` does ONE full rebuild
   (several minutes), then stays fast.

3. **Verify:** run `cargo test --manifest-path src-tauri/Cargo.toml` (expect 194 pass),
   then `du -sh src-tauri/target` — expect roughly 2–4 GB steady-state instead of 13 GB.

4. **Document the habit:** add a short "Disk usage" note to `HOW_TO_RUN.md`:
   `cargo clean` any time `target/` balloons; it is pure cache.

> Do NOT touch `[profile.release]` — `opt-level = "s"`, `lto`, `strip` are already
> correct for a small shipping binary.

## Phase 2 — Repo hygiene (small, do in one commit)

1. `git rm test_out.txt test_out2.txt` and add `test_out*.txt` to `.gitignore`.
2. Create `assets/`, move `Logo 2.png` and `Logo.jpg` into it, and re-export the
   6.3 MB PNG at 1024×1024 (icon-generation source needs no more; target < 1 MB).
   Do **not** rewrite git history — just fix it going forward.
3. Update stale sprint memory via the Case C script (never edit the file by hand):
   the right-click context-menu feature **is implemented** (commit `7a47952`),
   pending in-browser verification only.

## Phase 3 — Finish the existing roadmap (from `ARCHITECTURE.md` / `AGENTS.md`)

1. **System tray + minimize-to-tray** (the last big open TODO; `tray-icon` feature
   is already enabled in `Cargo.toml`):
   - Tray icon with menu: Show/Hide, Pause All, Resume All, Quit.
   - `AppSettings.minimize_to_tray` (default `false`, `#[serde(default)]` for
     forward-compat) + Settings-view checkbox, same pattern as `resume_on_startup`.
   - Window close → hide-to-tray when enabled; Quit menu item actually exits.
   - ⚠ Needs a display: implement + unit-test settings plumbing headless, but final
     verification runs on the user's machine (`npm run tauri dev`).
2. **In-browser verification of the right-click feature** (status in
   `plans/right-click-download/task.md` is "pending in-browser verification"):
   load `extension/` unpacked, test both menu items against a permitted URL.
3. **Expand `extension/content.js` media detection** for more platforms
   (`<video>`/`<source>`/og:video patterns). Keep the responsible-use boundary:
   no DRM/paywall bypass; DRM sites stay blocked.

## Phase 4 — Make the app better (new, highest value-per-line first)

Implement in this order; each item ships alone. Iron Rule: simplicity first.

1. **Search + status filter in the Downloads view** (`src/main.js` + `index.html`):
   text filter on filename/URL, status chips (All/Active/Paused/Done/Error).
   Pure frontend; no engine changes.
2. **Batch add** — paste multiple URLs (one per line) into the Add dialog; enqueue
   each via the existing command. Frontend split + loop; engine untouched.
3. **Clipboard URL watcher** (opt-in setting, default OFF): poll clipboard while the
   app is focused; when a URL with a downloadable extension appears, show the
   existing "Add download" toast/dialog pre-filled. Respect
   `AppSettings` validation pattern.
4. *(Optional, only if 1–3 land clean)* **Post-download checksum** — show SHA-256 of
   completed files in the details pane for manual verification.

## Constraints for the executor (Opus)

- Keep the `download-progress` payload (`DownloadItem`) **byte-for-byte stable** —
  it is the primary UI/core contract (`ARCHITECTURE.md` §Event Contract).
- After any Rust change: `cargo fmt` + `cargo clippy` (manifest in `src-tauri/`),
  zero warnings; run the full test suite.
- Never run `npm run tauri build` headless; dev-mode only unless on the user's machine.
- Respect the responsible-use boundary in `AGENTS.md`.
- Memory routing: architectural decisions → `ARCHITECTURE.md` (Case B); sprint notes →
  `memory_manager.py` (Case C). Update `ARCHITECTURE.md` open-TODO list as items close.

## Verification checklist (per phase)

- [x] Phase 1: tests pass after rebuild; `target/` ≤ ~4 GB; note added to HOW_TO_RUN.md
- [x] Phase 2: repo root clean; logo assets < 1 MB total in `assets/`; memory updated
- [/] Phase 3: tray works on user machine; right-click verified in Chrome; content.js removed for privacy
- [/] Phase 4: each feature demo-able in dev mode; clippy clean; 207 tests still green
