# CodexBar Plus

An unofficial Windows port of [CodexBar](https://github.com/steipete/CodexBar), the macOS
menu bar app that shows how much of your AI coding quota is left. This one lives in the
Windows tray, draws the same fuel-gauge icon, and reuses credentials that are already on
the machine — no extra logins.

Not affiliated with or endorsed by the upstream project.

## What works

32 of upstream's 69 providers:

- **Native**: Codex, Claude, Amp, Augment, AWS Bedrock, Gemini
- **Plugins**: 26 providers running on a QuickJS host that implements upstream's
  `codexbar-plugin.d.ts` contract. 16 of those `.js` files are upstream's own, byte for
  byte; the other 10 are Windows-side conversions of the corresponding Swift providers.

Alongside the tray app: local cost history scanned from Codex/Claude session logs, provider
status-page polling with an incident badge, DPAPI-encrypted secret storage, browser cookie
import (Firefox, and Chromium profiles predating app-bound encryption), Windows toast
notifications, a global shortcut, launch at login, and English / 简体中文 / 繁體中文.

## Building

Needs the MSVC build tools, the Windows SDK, and a recent stable Rust.

```powershell
cargo test --workspace
cargo run -p codexbar-app                    # the tray app
cargo run -p codexbar-cli -- usage           # the same data on the command line
cargo run -p codexbar-cli -- diagnose        # resolved paths and credential state
```

`codexbar` also has `cards`, `cost`, `status`, `cookie`, `providers` and `config`.

## Packaging

Release artifacts are built with [Velopack](https://velopack.io) and need the .NET SDK 8
plus `dotnet tool install -g vpk`.

```powershell
tools/package-windows.ps1 -UpdateUrl https://github.com/williamwue/codexbar-plus
```

Add `-Publish` to upload to GitHub Releases as a draft, and `-NoDraft` to publish it. The
feed URL is compiled into the binary, so a build can only ever check the feed it was built
for. Releases are not code signed yet: SmartScreen will warn on first run.

## Layout

| Path | What |
|---|---|
| `crates/codexbar-core` | Provider engine, HTTP retry, cost store, plugin host, Win32 bits |
| `crates/codexbar-cli` | `codexbar.exe` |
| `src-tauri` | Tray app (Tauri v2 / WebView2) |
| `ui` | Popover and settings pages, no build step |
| `docs/windows-port-analysis.md` | How the port was derived from upstream, milestone by milestone |

MIT licensed — see [LICENSE](LICENSE).
