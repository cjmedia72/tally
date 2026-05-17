# Tally

Live desktop widget that tracks your Claude Code + Codex CLI subscription usage in real time. Liquid-glass UI, zero servers, zero telemetry, zero tokens consumed.

![widget screenshot placeholder](#)

## What it shows

- **5-hour + weekly utilization rings** for each subscription — the same `% used` your vendor dashboards show, pulled from official APIs
- **Plan auto-detection** — reads your tier (Pro / Max 5× / Max 20× / Plus / Pro 5× / Team) from each vendor's API and labels the card accordingly
- **Token totals** (input / output / cache_read / cache_write / reasoning) parsed from local CLI session logs
- **Per-model cost accounting** — Opus 4.7 / Sonnet 4.6 / Haiku 4.5 / GPT-5.5 / GPT-5-codex / others, each priced at its real public list rate
- **API-equivalent $** — what your usage would cost on pay-as-you-go, summed across actual model mix
- **ROI / leverage** — what you've saved vs your subscription cost (MTD by default, or scoped to any period)
- **Data-view picker**: `Now / Today / MTD / 1D / 7D / 14D / 30D` — instant period swap from a cached snapshot, no re-fetch

## Vendor coverage

| Vendor | Live limits source | Token cost source |
|---|---|---|
| Claude Code | `api.anthropic.com/api/oauth/usage` (OAuth bearer) | `~/.claude/projects/**/*.jsonl` + Cowork sessions at `~/AppData/Roaming/Claude/local-agent-mode-sessions/**` |
| Codex CLI | local `codex app-server` JSON-RPC `account/rateLimits/read` | `~/.codex/sessions/**/*.jsonl` |

Both are local-only. The OAuth bearer + Codex login session you've already authorized are the only auth surfaces.

## Single-vendor mode

If only one CLI is installed + authenticated, the widget collapses to a single centered card. If neither is installed, you get a "Connect Claude Code / Connect Codex CLI" empty state with install links.

## Settings

`⚙` icon in the expanded panel opens:

- **Refresh interval** — 15s / 30s / 60s / 2m / 5m (default 30s)
- **Glass opacity** — slider, 0.20 to 0.85
- **Theme** — Dark / Light / Auto (follows OS preference)
- **Plan label overrides** — text inputs for the chip text if auto-detection is wrong

Settings persist to localStorage.

## Install

### Prerequisites

At least one of:
- [Claude Code CLI](https://docs.anthropic.com/claude-code) installed + authenticated (`claude setup-token`)
- [Codex CLI](https://github.com/openai/codex) installed + authenticated (`codex login`)

If neither is present the widget shows install prompts.

### From release (Windows)

Download the `.msi` or `.exe` installer from the [Releases page](../../releases). On first run, Windows SmartScreen may warn — click **More info → Run anyway** (the binary isn't code-signed). After install the widget appears top-right of your primary monitor.

### From source

```bash
git clone https://github.com/cjmedia72/tally
cd tally
npm install
npx tauri dev      # development
npx tauri build    # produces .msi + .exe in src-tauri/target/release/bundle/
```

Build prerequisites: Rust 1.77+, Node.js 18+, [Tauri 2 prereqs](https://tauri.app/start/prerequisites/) for your platform.

#### Windows build gotchas

1. **Path with spaces** — Cargo + MSYS2 mingw choke on usernames containing spaces. `.cargo/config.toml` redirects `target-dir` to `C:/rust-target/tally` to avoid this. Delete this file if your path has no spaces.
2. **MSYS2 mingw64** required on PATH at build time for the GNU Rust toolchain. Install via MSYS2 or use the MSVC toolchain with Visual Studio Build Tools.

## Privacy

- 100% local processing. No analytics, no telemetry, no auto-update home calls.
- The OAuth token from `~/.claude/.credentials.json` is used exclusively for calls to `api.anthropic.com` (Anthropic's official endpoint) — same calls Claude Code itself makes.
- Codex JSON-RPC runs locally; the spawned subprocess calls OpenAI's official endpoint.
- The only file the widget writes is a small cache at `%LOCALAPPDATA%/usage-widget/claude-usage-cache.json` so a rate-limit response doesn't blank the UI.

## Rate-limit safety

`/api/oauth/usage` is itself rate-limited by Anthropic. The widget:
- Caches live data in-process for 60 seconds
- Persists the last successful response to disk
- Falls back to the cached value when Anthropic returns 429
- Auto-recovers on the next successful refresh

You'll see a "data: Nh ago" stale indicator under each vendor card if cached values are being served.

## License

[MIT](LICENSE)

## Acknowledgements

If you're on macOS, [CodexBar by steipete](https://github.com/steipete/codexbar) covers more providers (29+) in a polished menu bar app. Tally exists for the Windows + dual-vendor case where most existing tools are Claude-only.
