# TALLY - AI Usage Monitor

Live Windows desktop widget for tracking Claude Code and Codex CLI subscription usage. Tally is local-first: it reads your existing CLI auth/session data, shows live quota gauges, and estimates API-equivalent spend from local logs.

## What It Shows

- 5-hour and weekly utilization rings for Claude Code and Codex.
- Plan labels such as Pro, Max 5x, Max 20x, Plus, Pro 5x, and Team.
- Token totals for input, output, cache read/write, cached input, and reasoning tokens.
- Per-model cost accounting using public list-price estimates.
- API-equivalent spend and ROI/leverage versus subscription cost.
- Data views: `Now`, `Today`, `MTD`, `1D`, `7D`, `14D`, and `30D`.

## Vendor Coverage

| Vendor | Live Limits Source | Token Cost Source |
|---|---|---|
| Claude Code | Claude CLI `/usage`, with OAuth usage endpoint fallback | `~/.claude/projects/**/*.jsonl` plus Claude desktop Cowork session roots |
| Codex CLI | local `codex app-server` JSON-RPC `account/rateLimits/read` | `~/.codex/sessions/**/*.jsonl` |

The app does not proxy your traffic or run a server. It reads local CLI files and calls the same vendor endpoints your authenticated tools already use.

## Single-Vendor Mode

If only one CLI is installed and authenticated, the widget collapses to a single centered card. If neither is installed, it shows install prompts for Claude Code and Codex CLI.

## Settings

The settings icon in the expanded panel opens:

- Refresh interval: 15s, 30s, 60s, 2m, or 5m.
- Glass opacity slider.
- Theme: Dark, Light, or Auto.
- Plan label overrides.
- Tray and taskbar visibility toggles.
- GitHub release update check.

Settings persist to localStorage.

## Install

### Prerequisites

At least one of:

- [Claude Code CLI](https://docs.anthropic.com/claude-code) installed and authenticated.
- [Codex CLI](https://github.com/openai/codex) installed and authenticated with `codex login`.

If neither is present, the widget still launches and shows connection prompts.

### From Release

Download the Windows `.msi` or `.exe` installer from the [Releases page](../../releases). On first run, Windows SmartScreen may warn because the binary is not code-signed. Choose **More info -> Run anyway** if you trust the downloaded release.

### From Source

```bash
git clone https://github.com/cjmedia72/tally
cd tally
npm install
npx tauri dev
npx tauri build
```

Build prerequisites: Rust 1.77+, Node.js 18+, and [Tauri 2 prerequisites](https://tauri.app/start/prerequisites/) for your platform.

### Windows Build Notes

1. `.cargo/config.toml` redirects Cargo output to `C:/rust-target/tally` to avoid Windows username paths with spaces. Delete or edit this if you prefer a normal local `target/` directory.
2. With the GNU Rust toolchain, put MSYS2 mingw64 on PATH before running tests/builds:

```powershell
$env:Path = 'C:\msys64\mingw64\bin;' + $env:Path
cargo test --manifest-path src-tauri\Cargo.toml
npm run build
```

## Privacy

- Local processing only; no analytics and no telemetry.
- Claude OAuth is read from `~/.claude/.credentials.json` and used for Anthropic Claude usage/profile requests.
- Codex usage is read through local Codex CLI/session data.
- The updater checks the public GitHub Releases API only when you ask it to check for updates.
- Tally writes local cache/history files under the OS cache/data directories, currently under `tally`.

## Rate-Limit Safety

Claude usage endpoints can rate-limit. Tally:

- Caches live data in-process.
- Persists the last successful Claude usage response to disk.
- Falls back to cached values if a live request fails or returns 429.
- Recovers automatically on the next successful refresh.

You'll see a `data: Nh ago` stale indicator under each vendor card if cached values are being served.

## Development Checks

```powershell
$env:Path = 'C:\msys64\mingw64\bin;' + $env:Path
cargo check --manifest-path src-tauri\Cargo.toml
cargo test --manifest-path src-tauri\Cargo.toml
node --check src\main.js
npm audit --omit=dev
```

## License

[MIT](LICENSE)

## Acknowledgements

Tally's Codex-side approach was informed by [CodexBar by Peter Steinberger](https://github.com/steipete/CodexBar). CodexBar is excellent on macOS and covers many more providers; Tally focuses on a compact Windows widget for Claude Code plus Codex.
