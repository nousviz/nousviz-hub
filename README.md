# NousViz Hub

Desktop client for NousViz. One app, all your instances: add each NousViz
server by URL, sign in once, and open it in its own window — already
authenticated. Works the same for a droplet, an office server, or an
instance running on this machine.

Built with [Tauri 2](https://tauri.app) (Rust + system webview), so the
app is a few MB and ships for **macOS, Windows, and Linux** from one
codebase.

## What it does (v0.1)

- **Instance list** — name + URL per NousViz server, stored in
  `instances.json` in the app data directory.
- **Sign in once** — the Hub logs in via `POST /api/auth/login` and keeps
  the session token in the OS keychain (macOS Keychain, Windows Credential
  Manager, Linux Secret Service). If no keychain is available it falls
  back to the config file and the UI flags it.
- **Open** — each instance opens in its own window pointed at the
  server's own web UI, with the session token seeded into
  `localStorage` (`nousviz_auth_token`) before load, so it starts signed in.
- **Health** — each card polls `GET /api/health` and shows status + version.

## Develop

```sh
npm install
npm run tauri dev
```

Requires Rust (rustup) and Node 18+. On Linux also:
`libwebkit2gtk-4.1-dev libappindicator3-dev librsvg2-dev patchelf libdbus-1-dev`.

## Build

```sh
npm run tauri build            # release bundles for the current OS
npm run tauri build -- --debug # faster debug bundle
```

CI (`.github/workflows/build.yml`) builds macOS (universal), Windows
(msi/nsis), and Linux (AppImage/deb/rpm) on every push to main. Bundles
are unsigned until signing credentials are configured.

## Roadmap

- v0.2 — fleet overview: one native screen aggregating health, version,
  and plugins across every instance.
- v0.3 — "Add this machine": guided local install of NousViz
  (managed Python via uv, embedded Postgres, background service).
- v0.4 — signed + notarized builds, auto-update.
