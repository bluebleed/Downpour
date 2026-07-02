---
type: rule-pointer
title: Project Rules Pointer
created: 2026-06-07
last-updated: 2026-07-02
load-behavior: eager
---

# CLAUDE.md

Guidance for Claude Code working in this repository.

**Read [AGENTS.md](./AGENTS.md) for the full project overview, architecture, commands,
conventions, and roadmap.** It is the single source of truth; this file only adds
Claude-specific notes.

## Quick facts

- **Downpour** — a universal (macOS/Windows/Linux) IDM-style download manager. **Sprint 1 COMPLETE** (Kiro built the full spec: downloader, queue, speed limiter, persistence, categorizer, media extractor, capture server, UI, browser extension).
- **Stack**: Tauri 2 (Rust core in `src-tauri/`, web UI in `src/`, browser extension in `extension/`).
- **Engine**: `src-tauri/src/downloader.rs` (parallel segmented downloads via HTTP `Range`).
- **Architecture decisions**: Read `ARCHITECTURE.md` for the full module map, event contract, constraints, and open TODOs.
- **Sprint notes**: Read `context/WORKING_MEMORY.md` for active sprint context.

## Working agreements

- Respect the responsible-use boundary in AGENTS.md (no DRM/paywall bypass; mind site ToS).
- Run `cargo fmt` and `cargo clippy` (manifest in `src-tauri/`) after Rust changes.
- Don't run a full `tauri build` in an environment without a display/webview libs; it builds on the user's machine.
- Keep the `download-progress` event payload (`DownloadItem`) stable across UI and core — it is the primary event contract.
- Memory routing: Case A → `_workspace-config/antigravity-knowledge/cheatsheet.md`, Case B → `ARCHITECTURE.md`, Case C → `memory_manager.py`.

**CRITICAL RULE:** Before executing any task, you MUST read the global meta-workspace rules.
> Read: `../../_workspace-config/CLAUDE.md`
> **At session start:** run the context primer → `../../_workspace-config/context-primer.md`
