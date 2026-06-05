# CLAUDE.md

Guidance for Claude Code working in this repository.

**Read [AGENTS.md](./AGENTS.md) for the full project overview, architecture, commands,
conventions, and roadmap.** It is the single source of truth; this file only adds
Claude-specific notes.

## Quick facts

- **Downpour** — a universal (macOS/Windows/Linux) IDM-style download manager.
- **Stack**: Tauri 2 (Rust core in `src-tauri/`, web UI in `src/`, browser extension in `extension/`).
- **Engine**: `src-tauri/src/downloader.rs` (parallel segmented downloads via HTTP `Range`).

## Working agreements

- Respect the responsible-use boundary in AGENTS.md (no DRM/paywall bypass; mind site ToS).
- Run `cargo fmt` and `cargo clippy` (manifest in `src-tauri/`) after Rust changes.
- Don't run a full `tauri build` in an environment without a display/webview libs; it builds on the user's Mac.
- Keep the `download-progress` event payload (`DownloadItem`) stable across UI and core.

**CRITICAL RULE:** Before executing any task, you MUST read the global meta-workspace rules.
> Read: `../../_workspace-config/CLAUDE.md`
> **At session start:** run the context primer � `../../_workspace-config/context-primer.md`
