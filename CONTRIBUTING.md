# Contributing to Downpour

Thanks for contributing.

## Before you start

- Keep downloads lawful and authorized. Do not add DRM, paywall, or access-control bypasses.
- Do not commit personal downloads, browser cookies, API keys, `.env` files, local paths, or build artifacts.
- Discuss material UX or engine changes in an issue before opening a large pull request.

## Local workflow

```bash
npm install
npm run tauri dev
cargo fmt --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml
cargo clippy --manifest-path src-tauri/Cargo.toml -- -D warnings
```

Run the extension property test with:

```bash
node extension/filter.property.test.js
```

## Pull requests

Keep a pull request focused, describe the user-facing effect, and include tests
for Rust behavior where practical. Do not change the `DownloadItem` event shape
without updating every consumer and documenting the compatibility impact.
