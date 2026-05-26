AS_OF: 2026-05-26

# Claude Usage Breakage Registry

This is the working record of the fragile Claude-side failures we have hit in Tally, why each one happened, and which guard now protects it. Keep this file current whenever Claude usage parsing, provider order, cache behavior, or reset handling changes.

## Provider Order

Current intended order:

1. Claude OAuth `/api/oauth/usage`
2. Claude CLI `/usage` through a PTY
3. Optional Claude web session fallback when `TALLY_CLAUDE_COOKIE` or `TALLY_CLAUDE_SESSION_KEY` is configured
4. Fresh-enough disk or memory cache only when live sources fail or OAuth is cooling down after `429`

## Incident Log

| Date | Symptom CJ Saw | Root Cause | Fix Applied | Guard / Test |
|---|---|---|---|---|
| 2026-05-20 | Claude 5-hour gauge showed `0%` or a stale session while Claude web showed real usage. | Parser fallback could grab the wrong percent from the TUI capture, especially Sonnet-only or status/context lines. | Isolated Claude parser into `src-tauri/src/claude/cli.rs`; label-scoped extraction now prefers `Current session`, `Current week (all models)`, and `Sonnet only`. | `parses_current_session_without_sonnet_overwrite`; `trims_terminal_noise_and_ignores_model_table_percents`. |
| 2026-05-20 | Claude displayed `RESET PENDING` after the reset should have rolled forward. | Stale reset timestamp from cache/OAuth survived past the active reset boundary and was rendered as if still authoritative. | Added stale active-window detection in cache path and secondary CLI/Web probe when OAuth active reset is missing or past. | `stale_oauth_active_window_requests_secondary_probe`; `wall_clock_stale_cache_is_not_served_after_sleep`. |
| 2026-05-20 | A later build seemed to "go back" to old broken Claude behavior. | Claude code was too centralized and broad edits/reverts touched provider logic while changing unrelated UI. | Claude source split into its own module files; current merge touches only `claude/cli.rs` and `claude/cache.rs` unless required. | `cargo test` plus narrower diff review before build. |
| 2026-05-21 | Tally showed `data: OAuth 11h ago`; force refresh did not move Claude. | The running EXE was an old process and the cache was seeded from disk. Live OAuth then hit `429`, so the widget kept showing cached data. | Cache now uses wall-clock freshness, preserves cooldown explicitly, and marks stale cache as `Cache` instead of pretending it is live. | `wall_clock_stale_cache_is_not_served_after_sleep`; process start-time check before rebuild/restart. |
| 2026-05-21 | During OAuth `429`, CLI fallback failed with `missing Current session`; web fallback was unavailable. | Claude CLI `/usage` TUI sometimes captured startup/loading output instead of the final usage panel; no web session token was configured. | Ported CodexBar-style retry behavior: 24s normal probe, 60s retry for timeout/startup/loading, explicit loading detection, and stricter latest-panel trim. | `rejects_empty_or_loading_output`; `rejects_startup_output_as_retryable`; forced `TALLY_CLAUDE_SKIP_OAUTH=1` smoke. |
| 2026-05-21 | Windows CLI fallback produced empty raw tail after we copied upstream args. | `claude --allowed-tools ""` behaves differently on Windows and caused Claude to enter a non-interactive/print-style error path. | Windows path now launches `cmd.exe /C claude`; `--allowed-tools ""` remains only on non-Windows. | Manual `claude --version`; forced CLI smoke with debug output. |
| 2026-05-21 | Claude CLI loading state could be mistaken for valid usage or a permanent parse failure. | The loading panel can include weekly text/percents but no usable current-session value. | Added `is_usage_still_loading`, `usage_output_looks_relevant`, and retry classification based on CodexBar commits `354e0b6d`, `ef2e35f3`, `1dd2804f`. | `rejects_empty_or_loading_output`; retry helper assertion in `rejects_startup_output_as_retryable`. |
| 2026-05-21 | Status/context `0%` could contaminate the usage panel. | PTY capture includes earlier Claude TUI fragments before the `Settings: ... Usage` panel is fully drawn. | `trim_to_latest_usage_panel` now prefers the last `Settings:` block that contains `Usage`, usage words, or loading state. | `trims_to_latest_settings_usage_panel`. |
| 2026-05-21 | Forced CLI fallback returned no visible output, then later returned `source: cli` with weekly equal to the 5-hour value. | Windows Claude CLI emits terminal cursor-position request `ESC[6n`; Tally did not answer it, and the first `/usage` could be consumed by the folder-trust prompt. Once capture worked, Claude's TUI arrived as a compact cursor-positioned blob, so line-based parsing reused the session percent. | Added minimal terminal response `ESC[1;1R`, startup trust-prompt handling before sending `/usage`, and compact TUI percent/reset extraction bounded by usage-window labels. | `parses_compact_tui_capture_without_reusing_session_percent`; forced `TALLY_CLAUDE_SKIP_OAUTH=1` smoke. |
| 2026-05-25 | New PC showed Claude missing/stale after install even though v0.1.37 had OAuth refresh. | The copied `.claude/.credentials.json` token had expired, npm global `claude` was missing, and the first refresh-token implementation used `https://console.anthropic.com/v1/oauth/token`. Peter's CodexBar current implementation uses `https://platform.claude.com/v1/oauth/token`. | Recreated `%APPDATA%\npm`, installed `@anthropic-ai/claude-code`, and changed Tally's refresh endpoint to the CodexBar/Claude Code platform endpoint. | `uses_claude_code_platform_refresh_endpoint`; manual `where claude` + `claude --version`; cache moved from stale May 25 data to fresh OAuth sample. |
| 2026-05-25 | Claude card stayed visible but kept showing `data: Cache` with old reset values. | v0.1.38 fixed the card-hiding availability regression, but when OAuth refresh and CLI were both dead, the only surviving source was stale disk cache. That made the UI look alive while live acquisition was broken. | Keep the v0.1.38 file-only availability check, but treat stale `Cache` source as a diagnostic signal: verify OAuth expiry, CLI presence, and latest `claude-limit-samples` timestamp before calling the UI broken. | Local log audit: `usage-snapshots-2026.jsonl` rows 606 spanning 2026-05-18 to 2026-05-26; `claude-limit-samples-2026.jsonl` rows 1672 with fresh 2026-05-26 sample after CLI/env repair. |

## Merge Notes From CodexBar

Applicable:

- Retry CLI probes when the first capture times out, returns startup output, or reports usage still loading.
- Treat `Loading usage...` as retryable, not as valid data and not as a permanent parse failure.
- Trim to the latest `Settings: ... Usage` panel before parsing.
- Require usage-window labels and usage keywords before accepting percent fallback values.
- Answer Claude's terminal cursor-position request and clear trust prompts before issuing `/usage` on Windows.

Not directly portable:

- CodexBar's Swift/macOS `ClaudeCLISession` PTY implementation and session reuse.
- macOS-specific keychain/OAuth delegation behavior.
- Menu bar provider lifecycle code.

Open risk:

- Claude owns the CLI TUI format, so any future screen-label rename can still break parsing. The fixture registry should get a new sample every time that happens.
