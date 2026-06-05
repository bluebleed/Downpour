# App icons

Tauri needs icon files here before `dev`/`build` will run. Generate them from a
single square PNG (1024x1024 recommended) on your machine:

```bash
# from desktop-apps/downpour/
npm run tauri icon path/to/logo.png
```

This produces `32x32.png`, `128x128.png`, `128x128@2x.png`, `icon.icns` (macOS),
and `icon.ico` (Windows) — exactly the files referenced in `tauri.conf.json`.
