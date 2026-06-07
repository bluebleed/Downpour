# Downpour — Active Sprint Memory
> **Case C memory** (active sprint scratchpad). Managed by `memory_manager.py`.
> Auto-archived to `context/ARCHIVE_MEMORY.md` when over 2,500 chars.
---
## Current Status (as of 2026-06-07)
**Sprint 1 COMPLETE.** Kiro built the full feature set from `.kiro/specs/idm-download-manager/tasks.md`.
All 17 task groups are marked `[x]`. The project is a functioning download manager.
## Key Context
- This is a **Tauri 2** desktop app — the Rust core and web UI are compiled together.
- Running `npm run tauri dev` (or `run.bat`) compiles Rust on first launch (slow), then hot-reloads.
- The `run.bat` script handles Windows setup (npm install, dependency checks).
- The Kiro spec lives in `.kiro/specs/idm-download-manager/` — requirements.md + tasks.md + design.md.
- The frontend is vanilla JS (no framework) — Tauri IPC via `@tauri-apps/api`.
## Known TODOs / Next Work
