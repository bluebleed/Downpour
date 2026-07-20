# Downpour

Downpour is a fast, cross-platform desktop download manager built with Tauri 2:
a Rust download engine and a lightweight web UI packaged as one native app.

> Responsible use: download only content you are allowed to access. Downpour
> does not support bypassing DRM, paywalls, or access controls.

> [!NOTE]
> **AI-Designed & Active Development**: Downpour has been designed and implemented in collaboration with AI coding assistants (Antigravity and Codex). The project is in active development and will continue to improve over time.

## Features

- Parallel HTTP downloads with pause/resume and restart persistence
- Queue, concurrency controls, speed limit, search, filtering, and batch URL add
- Optional categorization of completed files
- Optional system tray and native notifications
- Optional browser extension for new downloads you explicitly choose to capture
- Optional yt-dlp/ffmpeg integration for permitted media downloads

## Run locally

### Prerequisites

- Node.js 18 or newer
- Rust stable via [rustup](https://rustup.rs/)
- Platform prerequisites for [Tauri v2](https://v2.tauri.app/start/prerequisites/)
- Optional: `yt-dlp` and `ffmpeg` for the Media view

### Development

```bash
npm install
npm run tauri dev
```

The first run compiles the Rust application and can take a few minutes. On
Windows, `run.bat` is also available as a convenience launcher.

### Checks

```bash
npm.cmd run build
node extension/filter.property.test.js
cargo fmt --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings
```

## Install the browser extension

The companion extension is optional and disabled by default.

1. Start Downpour with `npm run tauri dev`.
2. Open `chrome://extensions` in Chrome or `edge://extensions` in Edge.
3. Enable **Developer mode**, choose **Load unpacked**, and select the
   repository's `extension` directory.
4. Open the Downpour Capture popup and verify it says **Connected to
   Downpour**.
5. Turn on **Capture downloads** only when you want future browser downloads
   sent to Downpour. It does not import existing browser downloads.

The extension only communicates with the local app at `127.0.0.1`. It does not
request browsing-history, cookie, or all-sites permissions.

## Privacy and security

See [PRIVACY.md](PRIVACY.md) and [SECURITY.md](SECURITY.md). Local app state,
workspace notes, credentials, certificates, build outputs, and temporary files
are excluded from Git. Review `git status` before every commit.

## Contributing

Contributions are welcome. Please read [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT. See [LICENSE](LICENSE).
