// Defensive: log clearly if Tauri's global isn't injected so we can debug.
if (!window.__TAURI__) {
  console.error("[usage-widget] window.__TAURI__ not present — withGlobalTauri must be true");
}
const invoke = window.__TAURI__?.core?.invoke ||
  (() => Promise.reject(new Error("Tauri invoke unavailable")));

const RING_CIRCUMFERENCE = 188; // 2*pi*30
const SETTINGS_KEY = "usage-widget:settings:v1";

const DEFAULT_SETTINGS = {
  refreshMs: 120_000,
  glassAlpha: 0.55,
  theme: "dark", // "dark" | "light" | "auto"
  claudeTier: "MAX 5× · $100",
  codexTier:  "PRO 5× · $100",
  showTrayIcon: true,
  showTaskbarIcon: false,
  autoCheckUpdates: true,
  compactDataView: false,
  // ROI denominator behavior:
  //   "monthly"  → always divide by full monthly sub cost (legacy)
  //   "prorated" → divide by sub cost scaled to selected period
  //                e.g. 7D leverage = 7d_spend / (7/30 × monthly_sub)
  roiMode: "prorated",
};

function normalizeSettings(s) {
  const next = { ...DEFAULT_SETTINGS, ...s };
  next.showTrayIcon = next.showTrayIcon !== false;
  next.showTaskbarIcon = next.showTaskbarIcon === true;
  next.compactDataView = next.compactDataView === true;
  if (!next.showTrayIcon && !next.showTaskbarIcon) {
    next.showTaskbarIcon = true;
  }
  return next;
}

// Days-in-period for proration. "today" is partial — we round up to 1 day.
const PERIOD_DAYS = {
  now:   30,  // Now stays MTD-anchored
  today: 1,
  "1d":  1,
  "7d":  7,
  "14d": 14,
  "30d": 30,
  // mtd uses live days-elapsed from snapshot.roi.mtd_days_elapsed
};

function loadSettings() {
  try {
    const raw = localStorage.getItem(SETTINGS_KEY);
    if (!raw) return normalizeSettings({});
    return normalizeSettings(JSON.parse(raw));
  } catch {
    return normalizeSettings({});
  }
}
function saveSettings(s) {
  settings = normalizeSettings(s);
  try { localStorage.setItem(SETTINGS_KEY, JSON.stringify(settings)); } catch {}
}

let settings = loadSettings();
let refreshTimer = null;
let currentPeriod = "now";      // active data-view period
let lastSnapshot = null;        // cached snapshot so picker re-renders without re-fetching
let lastUpdateInfo = null;
let refreshInFlight = false;
let refreshQueued = false;
let fitTimer = null;
let fitRunId = 0;

const PERIOD_LABELS = {
  now:   "TODAY",  // "Now" view shows today tokens but with MTD-anchored ROI
  today: "TODAY",
  "1d":  "1D",
  "7d":  "7D",
  "14d": "14D",
  "30d": "30D",
  mtd:   "MTD",
};

// "Now" view = today's tokens in cards + original MTD-anchored ROI bar.
// Maps to backend "today" period for the per-brand data.
function effectivePeriod(p) {
  return p === "now" ? "today" : p;
}

const el = (id) => document.getElementById(id);

function fmtTokens(n) {
  if (n == null) return "—";
  if (n >= 1_000_000_000) return (n / 1_000_000_000).toFixed(2).replace(/\.?0+$/, "") + "B";
  if (n >= 1_000_000)     return (n / 1_000_000).toFixed(2).replace(/\.?0+$/, "") + "M";
  if (n >= 1_000)         return (n / 1_000).toFixed(1).replace(/\.0$/, "") + "K";
  return String(n);
}

function fmtMoney(n, digits = 2) {
  if (n == null) return "—";
  return "$" + Number(n).toLocaleString("en-US", {
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  });
}

function fmtPct(p) {
  if (p == null) return "—";
  return Math.round(p) + "%";
}

function fmtAge(iso, source = null) {
  const prefix = source ? `data: ${source} ` : "data: ";
  if (!iso) return { text: prefix + "never seen", cls: "cold" };
  const then = new Date(iso).getTime();
  const ageSec = (Date.now() - then) / 1000;
  let text, cls;
  if (ageSec < 60)        { text = prefix + "just now";                 cls = "fresh"; }
  else if (ageSec < 3600) { text = `${prefix}${Math.round(ageSec/60)}m ago`; cls = ageSec < 300 ? "fresh" : "stale"; }
  else if (ageSec < 86400){ text = `${prefix}${Math.round(ageSec/3600)}h ago`; cls = "stale"; }
  else                    { text = `${prefix}${Math.round(ageSec/86400)}d ago`; cls = "cold"; }
  return { text, cls };
}

function setFreshness(elementId, iso, source = null) {
  const e = el(elementId);
  if (!e) return;
  const { text, cls } = fmtAge(iso, source);
  e.textContent = text;
  e.classList.remove("fresh", "stale", "cold");
  e.classList.add(cls);
}

function setRing(circleId, pct) {
  const c = el(circleId);
  if (!c) return;
  const clamped = Math.max(0, Math.min(100, pct || 0));
  const offset = RING_CIRCUMFERENCE * (1 - clamped / 100);
  c.setAttribute("stroke-dashoffset", offset.toFixed(2));
}

function setBar(barId, pct) {
  const b = el(barId);
  if (!b) return;
  b.style.width = Math.max(0, Math.min(100, pct || 0)) + "%";
}

function setTick(tickId, pct) {
  const t = el(tickId);
  if (!t) return;
  t.style.left = Math.max(0, Math.min(100, pct || 0)) + "%";
}

function render(snap) {
  if (!snap) return;
  lastSnapshot = snap;
  const c = snap.claude, x = snap.codex, r = snap.roi;
  // Per-period data — "now" maps to "today" for the brand cards
  const dataKey = effectivePeriod(currentPeriod);
  const cPeriod = c?.periods?.[dataKey] || c?.periods?.today || null;
  const xPeriod = x?.periods?.[dataKey] || x?.periods?.today || null;

  // ── Single-vendor layout
  const claudeRow = document.querySelector(".pill-row.claude");
  const codexRow  = document.querySelector(".pill-row.codex");
  const claudeCard = el("cardClaude");
  const codexCard  = el("cardCodex");
  const cards      = el("brandCards");
  const empty      = el("emptyState");

  const showClaude = snap.claude_available && !!c;
  const showCodex  = snap.codex_available  && !!x;
  const singleAgent = showClaude !== showCodex;
  document.body.classList.toggle("single-agent", singleAgent);
  if (claudeRow)  claudeRow.style.display  = showClaude ? "" : "none";
  if (codexRow)   codexRow.style.display   = showCodex  ? "" : "none";
  if (claudeCard) claudeCard.style.display = showClaude ? "" : "none";
  if (codexCard)  codexCard.style.display  = showCodex  ? "" : "none";
  if (cards) {
    cards.classList.toggle("solo", singleAgent);
    cards.style.display = (showClaude || showCodex) ? "" : "none";
  }
  if (empty) {
    empty.style.display = (!showClaude && !showCodex) ? "" : "none";
  }

  // ── PILL (collapsed)
  if (showClaude) {
    setBar("pill5hClaudeBar", c.five_hour.used_percent);
    setTick("pillWkClaudeTick", c.weekly.used_percent);
    el("pill5hClaudePct").textContent = fmtPct(c.five_hour.used_percent);
    el("pillWkClaude").textContent = "Claude " + fmtPct(c.weekly.used_percent);
  }
  if (showCodex) {
    setBar("pill5hCodexBar",  x.five_hour.used_percent);
    setTick("pillWkCodexTick",  x.weekly.used_percent);
    el("pill5hCodexPct").textContent  = fmtPct(x.five_hour.used_percent);
    el("pillWkCodex").textContent  = "Codex "  + fmtPct(x.weekly.used_percent);
  }
  el("pillRoi").textContent = r.leverage > 0 ? r.leverage.toFixed(1) + "×" : "—";

  // ── PANEL (expanded)
  el("headTime").textContent = new Date(snap.updated_at).toLocaleString([], {
    month: "short", day: "numeric", hour: "numeric", minute: "2-digit",
  });
  el("headLive").textContent = "LIVE";

  // Tier labels respect user override from settings
  if (showClaude) {
    el("claudeTier").textContent = settings.claudeTier || c.tier;
    setFreshness("claudeFreshness", c.data_updated_at || c.last_event_at, c.data_source);
    setRing("claudeRing5h", c.five_hour.used_percent);
    setRing("claudeRingWk", c.weekly.used_percent);
    el("claudePct5h").textContent = fmtPct(c.five_hour.used_percent);
    el("claudePctWk").textContent = fmtPct(c.weekly.used_percent);
    el("claudeReset5h").textContent = c.five_hour.resets_label;
    el("claudeResetWk").textContent = c.weekly.resets_label;
    if (cPeriod) {
      el("claudeIn").textContent      = fmtTokens(cPeriod.tokens.input);
      el("claudeOut").textContent     = fmtTokens(cPeriod.tokens.output);
      el("claudeCacheR").textContent  = fmtTokens(cPeriod.tokens.cache_read);
      el("claudeCacheW").textContent  = fmtTokens(cPeriod.tokens.cache_write);
      el("claudeReq").textContent     = String(cPeriod.requests);
      el("claudeCost").textContent    = fmtMoney(cPeriod.api_equiv);
    }
  }
  if (showCodex) {
    el("codexTier").textContent  = settings.codexTier  || x.tier;
    setFreshness("codexFreshness",  x.data_updated_at || x.last_event_at, x.data_source);
    setRing("codexRing5h",  x.five_hour.used_percent);
    setRing("codexRingWk",  x.weekly.used_percent);
    el("codexPct5h").textContent  = fmtPct(x.five_hour.used_percent);
    el("codexPctWk").textContent  = fmtPct(x.weekly.used_percent);
    el("codexReset5h").textContent  = x.five_hour.resets_label;
    el("codexResetWk").textContent  = x.weekly.resets_label;
    if (xPeriod) {
      el("codexIn").textContent      = fmtTokens(xPeriod.tokens.input);
      el("codexOut").textContent     = fmtTokens(xPeriod.tokens.output);
      el("codexReason").textContent  = fmtTokens(xPeriod.tokens.reasoning);
      el("codexCached").textContent  = fmtTokens(xPeriod.tokens.cached_input);
      el("codexReq").textContent     = String(xPeriod.requests);
      el("codexCost").textContent    = fmtMoney(xPeriod.api_equiv);
    }
  }

  // Left cell: always shows the current period's spend (Today's $ for "now")
  const isNow = currentPeriod === "now";
  const leftKey = effectivePeriod(currentPeriod);
  const leftEquiv = r.period_api_equiv?.[leftKey] ?? r.today_api_equiv;
  el("roiToday").textContent     = fmtMoney(leftEquiv);
  const roiTodayLabel = document.querySelector('.roi-cell:first-child .l');
  if (roiTodayLabel) roiTodayLabel.textContent = (PERIOD_LABELS[currentPeriod] || "TODAY") + " · API EQUIV";

  el("roiMtd").textContent       = fmtMoney(r.mtd_api_equiv, 0);
  const daily = r.mtd_days_elapsed > 0 ? r.mtd_api_equiv / r.mtd_days_elapsed : 0;
  el("roiMtdSub").textContent    = r.mtd_days_elapsed + " days · " + fmtMoney(daily, 0) + "/day avg";
  el("roiSubs").textContent      = fmtMoney(r.subscriptions, 0);
  el("roiSubsBreakdown").textContent = fmtMoney(r.claude_monthly, 0) + " Claude + " + fmtMoney(r.codex_monthly, 0) + " Codex";

  // ROI leverage:
  //  - "Now" view → MTD spend vs MTD-prorated sub (or full monthly if mode=monthly)
  //  - other views → period spend vs period-prorated sub (or full monthly if mode=monthly)
  const leverageEquiv = isNow ? r.mtd_api_equiv : leftEquiv;
  const leverageLabel = isNow ? "MTD" : (PERIOD_LABELS[currentPeriod] || "TODAY");

  let leverageDenom = r.subscriptions; // legacy "monthly" mode
  if (settings.roiMode === "prorated") {
    const days = currentPeriod === "mtd" || isNow
      ? Math.max(1, r.mtd_days_elapsed || 1)
      : (PERIOD_DAYS[currentPeriod] || 30);
    leverageDenom = (days / 30) * r.subscriptions;
  }

  const periodLeverage = leverageDenom > 0 ? leverageEquiv / leverageDenom : 0;
  el("roiLeverage").textContent = periodLeverage.toFixed(1) + "×";
  const periodSaved = Math.max(0, leverageEquiv - leverageDenom);
  el("roiSaved").textContent = fmtMoney(periodSaved, 0) + " saved " + leverageLabel;
}

async function refresh() {
  if (refreshInFlight) {
    refreshQueued = true;
    return;
  }
  refreshInFlight = true;
  const liveEl = el("pillLive");
  try {
    const snap = await invoke("get_snapshot", { refreshMs: settings.refreshMs });
    render(snap);
    if (liveEl) liveEl.textContent = "LIVE";
    // Refit window to content (handles freshness tag wrapping etc.)
    scheduleFitWindowToContent(30);
  } catch (err) {
    console.error("[usage-widget] snapshot error:", err);
    if (liveEl) liveEl.textContent = "ERROR";
  } finally {
    refreshInFlight = false;
    if (refreshQueued) {
      refreshQueued = false;
      setTimeout(refresh, 250);
    }
  }
}

// Auto-fit HEIGHT to content. Width stays at the canonical state width so
// a mid-transition measurement can't lock the window narrow.
const WIDTH_BY_STATE = { collapsed: 272, expanded: 680, compact: 640, settings: 720 };
const SOLO_WIDTH_BY_STATE = { expanded: 520, compact: 520 };
const HEIGHT_PAD_BY_STATE = { collapsed: 4, expanded: 22, compact: 18, settings: 22 };
function widthForState(stateKey) {
  return document.body.classList.contains("single-agent")
    ? (SOLO_WIDTH_BY_STATE[stateKey] || WIDTH_BY_STATE[stateKey])
    : WIDTH_BY_STATE[stateKey];
}
function nextFrame() {
  return new Promise((resolve) => requestAnimationFrame(() => resolve()));
}
function scheduleFitWindowToContent(delay = 50) {
  clearTimeout(fitTimer);
  fitTimer = setTimeout(() => {
    void fitWindowToContent();
  }, delay);
}
async function fitWindowToContent() {
  const runId = ++fitRunId;
  let target, stateKey;
  if (document.body.classList.contains("state-expanded")) {
    target = el("panel");    stateKey = "expanded";
  } else if (document.body.classList.contains("state-compact")) {
    target = el("panel");    stateKey = "compact";
  } else if (document.body.classList.contains("state-settings")) {
    target = el("settings"); stateKey = "settings";
  } else {
    target = el("pill");     stateKey = "collapsed";
  }
  if (!target) return;
  const width = widthForState(stateKey);
  try {
    await invoke("set_window_size", {
      width,
      height: Math.max(Math.ceil(window.innerHeight || 0), 110)
    });
  } catch (e) {}
  await nextFrame();
  await nextFrame();
  if (runId !== fitRunId) return;
  void target.offsetHeight; // force layout at the final width before measuring height
  const rectHeight = target.getBoundingClientRect().height;
  const h = Math.ceil(Math.max(
    rectHeight,
    target.scrollHeight,
    document.body.scrollHeight,
    document.documentElement.scrollHeight
  ) + (HEIGHT_PAD_BY_STATE[stateKey] || 0));
  if (h < 50) return;
  try { await invoke("set_window_size", { width, height: h }); } catch (e) {}
}

// ── State toggle
async function expand() {
  document.body.classList.remove("state-collapsed", "state-compact", "state-settings");
  document.body.classList.add("state-expanded");
  try { await invoke("resize_window", { expanded: true }); } catch (e) {}
  // After layout settles, shrink to exact content height
  scheduleFitWindowToContent(50);
}
async function compact() {
  document.body.classList.remove("state-collapsed", "state-expanded", "state-settings");
  document.body.classList.add("state-compact");
  try { await invoke("resize_window", { expanded: true }); } catch (e) {}
  scheduleFitWindowToContent(50);
}
async function collapse() {
  document.body.classList.remove("state-expanded", "state-compact", "state-settings");
  document.body.classList.add("state-collapsed");
  try { await invoke("resize_window", { expanded: false }); } catch (e) {}
  scheduleFitWindowToContent(50);
}
async function showSettings() {
  document.body.classList.remove("state-collapsed", "state-expanded", "state-compact");
  document.body.classList.add("state-settings");
  try { await invoke("set_window_size", { width: WIDTH_BY_STATE.settings, height: 540 }); } catch (e) {}
  scheduleFitWindowToContent(50);
}

// ── Apply settings to DOM/timers
async function applyShellVisibility() {
  try {
    await invoke("set_shell_visibility", {
      showTray: settings.showTrayIcon,
      showTaskbar: settings.showTaskbarIcon,
    });
  } catch (err) {
    console.error("[usage-widget] shell visibility error:", err);
  }
}

function renderUpdateStatus(message, tone = "muted") {
  const status = el("updateStatus");
  if (!status) return;
  status.textContent = message;
  status.dataset.tone = tone;
}

function renderUpdateControls(info) {
  lastUpdateInfo = info;
  const download = el("btnDownloadUpdate");
  if (download) {
    download.style.display = info?.update_available && info.download_url ? "" : "none";
    download.textContent = info?.asset_name ? "Download " + info.asset_name : "Open release";
  }
  if (!info) return;
  if (info.update_available) {
    renderUpdateStatus(`Update available: ${info.latest_version}`, "ready");
  } else if (info.latest_version) {
    renderUpdateStatus(`Up to date: ${info.current_version}`, "ok");
  }
}

async function checkForUpdates(manual = false) {
  const btn = el("btnCheckUpdate");
  if (btn) btn.disabled = true;
  renderUpdateStatus("Checking GitHub Releases...", "muted");
  try {
    const info = await invoke("check_for_update");
    renderUpdateControls(info);
    if (manual && !info.update_available) {
      flashSaved("✓ Current");
    }
  } catch (err) {
    console.error("[usage-widget] update check error:", err);
    renderUpdateStatus("Update check failed", "error");
  } finally {
    if (btn) btn.disabled = false;
  }
}

function applySettings() {
  settings = normalizeSettings(settings);
  // Theme
  document.body.classList.remove("theme-dark", "theme-light", "theme-auto");
  document.body.classList.add(`theme-${settings.theme}`);
  // Glass opacity
  document.documentElement.style.setProperty("--glass-alpha", settings.glassAlpha);
  document.body.classList.toggle("compact-data-view-on", settings.compactDataView);
  // Refresh interval
  if (refreshTimer) clearInterval(refreshTimer);
  refreshTimer = setInterval(refresh, settings.refreshMs);
  // Sync controls
  const intervalEl = el("optInterval");
  if (intervalEl) intervalEl.value = String(settings.refreshMs);
  const opacityEl = el("optOpacity");
  if (opacityEl) opacityEl.value = String(settings.glassAlpha);
  const opacityVal = el("optOpacityVal");
  if (opacityVal) opacityVal.textContent = Number(settings.glassAlpha).toFixed(2);
  document.querySelectorAll(".theme-btn[data-theme]").forEach((b) => {
    b.setAttribute("data-active", b.dataset.theme === settings.theme ? "true" : "false");
  });
  document.querySelectorAll(".theme-btn[data-roi]").forEach((b) => {
    b.setAttribute("data-active", b.dataset.roi === settings.roiMode ? "true" : "false");
  });
  document.querySelectorAll(".theme-btn[data-tray]").forEach((b) => {
    b.setAttribute("data-active", String(settings.showTrayIcon) === b.dataset.tray ? "true" : "false");
  });
  document.querySelectorAll(".theme-btn[data-taskbar]").forEach((b) => {
    b.setAttribute("data-active", String(settings.showTaskbarIcon) === b.dataset.taskbar ? "true" : "false");
  });
  document.querySelectorAll(".theme-btn[data-updates]").forEach((b) => {
    b.setAttribute("data-active", String(settings.autoCheckUpdates) === b.dataset.updates ? "true" : "false");
  });
  document.querySelectorAll(".theme-btn[data-compact-data-view]").forEach((b) => {
    b.setAttribute("data-active", String(settings.compactDataView) === b.dataset.compactDataView ? "true" : "false");
  });
  const claudeTierInput = el("optClaudeTier");
  if (claudeTierInput) claudeTierInput.value = settings.claudeTier;
  const codexTierInput  = el("optCodexTier");
  if (codexTierInput)  codexTierInput.value  = settings.codexTier;
  void applyShellVisibility();
}

// Tiny visual ack that a control was registered.
function ackPulse(target) {
  if (!target) return;
  target.classList.remove("pulse-ack");
  // force reflow so the animation restarts on rapid changes
  void target.offsetWidth;
  target.classList.add("pulse-ack");
}

function flashSaved(message = "✓ Saved") {
  const ind = el("savedIndicator");
  const btn = el("btnSaveSettings");
  if (ind) {
    ind.textContent = message;
    ind.classList.add("show");
    clearTimeout(flashSaved._t);
    flashSaved._t = setTimeout(() => ind.classList.remove("show"), 1600);
  }
  if (btn) {
    btn.classList.add("saved");
    btn.textContent = "✓ Saved";
    clearTimeout(flashSaved._b);
    flashSaved._b = setTimeout(() => {
      btn.classList.remove("saved");
      btn.textContent = "Save";
    }, 1400);
  }
}

function wireSettings() {
  el("btnSettings")?.addEventListener("click", showSettings);
  el("btnSettingsBack")?.addEventListener("click", expand);

  el("optInterval")?.addEventListener("change", (e) => {
    settings.refreshMs = parseInt(e.target.value, 10) || 30000;
    saveSettings(settings);
    applySettings();
    ackPulse(e.target);
  });
  el("optOpacity")?.addEventListener("input", (e) => {
    settings.glassAlpha = parseFloat(e.target.value);
    saveSettings(settings);
    // Set on documentElement — :root-only declaration so this beats theme blocks
    document.documentElement.style.setProperty("--glass-alpha", settings.glassAlpha);
    const v = el("optOpacityVal");
    if (v) v.textContent = settings.glassAlpha.toFixed(2);
  });
  document.querySelectorAll(".theme-btn[data-theme]").forEach((btn) => {
    btn.addEventListener("click", () => {
      settings.theme = btn.dataset.theme;
      saveSettings(settings);
      applySettings();
      ackPulse(btn);
    });
  });
  document.querySelectorAll(".theme-btn[data-roi]").forEach((btn) => {
    btn.addEventListener("click", () => {
      settings.roiMode = btn.dataset.roi;
      saveSettings(settings);
      applySettings();
      if (lastSnapshot) render(lastSnapshot); // re-render ROI cells immediately
      ackPulse(btn);
    });
  });
  document.querySelectorAll(".theme-btn[data-tray]").forEach((btn) => {
    btn.addEventListener("click", () => {
      settings.showTrayIcon = btn.dataset.tray === "true";
      if (!settings.showTrayIcon && !settings.showTaskbarIcon) {
        settings.showTaskbarIcon = true;
      }
      saveSettings(settings);
      applySettings();
      ackPulse(btn);
    });
  });
  document.querySelectorAll(".theme-btn[data-taskbar]").forEach((btn) => {
    btn.addEventListener("click", () => {
      settings.showTaskbarIcon = btn.dataset.taskbar === "true";
      if (!settings.showTrayIcon && !settings.showTaskbarIcon) {
        settings.showTrayIcon = true;
      }
      saveSettings(settings);
      applySettings();
      ackPulse(btn);
    });
  });
  document.querySelectorAll(".theme-btn[data-updates]").forEach((btn) => {
    btn.addEventListener("click", () => {
      settings.autoCheckUpdates = btn.dataset.updates === "true";
      saveSettings(settings);
      applySettings();
      ackPulse(btn);
      if (settings.autoCheckUpdates) checkForUpdates(false);
    });
  });
  document.querySelectorAll(".theme-btn[data-compact-data-view]").forEach((btn) => {
    btn.addEventListener("click", () => {
      settings.compactDataView = btn.dataset.compactDataView === "true";
      saveSettings(settings);
      applySettings();
      scheduleFitWindowToContent(30);
      ackPulse(btn);
    });
  });
  el("btnCheckUpdate")?.addEventListener("click", () => checkForUpdates(true));
  el("btnDownloadUpdate")?.addEventListener("click", async () => {
    const url = lastUpdateInfo?.download_url || lastUpdateInfo?.release_url;
    if (!url) return;
    try {
      await invoke("open_update_url", { url });
    } catch (err) {
      console.error("[usage-widget] open update URL error:", err);
      renderUpdateStatus("Could not open update link", "error");
    }
  });
  el("optClaudeTier")?.addEventListener("input", (e) => {
    settings.claudeTier = e.target.value;
    saveSettings(settings);
    const t = el("claudeTier");
    if (t) t.textContent = settings.claudeTier || "MAX 5× · $100";
  });
  el("optCodexTier")?.addEventListener("input", (e) => {
    settings.codexTier = e.target.value;
    saveSettings(settings);
    const t = el("codexTier");
    if (t) t.textContent = settings.codexTier || "PRO 5× · $100";
  });
  el("btnResetDefaults")?.addEventListener("click", () => {
    settings = normalizeSettings({});
    saveSettings(settings);
    applySettings();
    flashSaved("✓ Reset");
  });
  el("btnSaveSettings")?.addEventListener("click", () => {
    saveSettings(settings);
    applySettings();
    flashSaved();
  });
}

async function manualRefresh() {
  const btn = el("btnRefresh");
  if (btn) btn.classList.add("spin");
  await refresh();
  setTimeout(() => btn?.classList.remove("spin"), 400);
}

function setPeriod(period) {
  currentPeriod = period;
  document.querySelectorAll(".dv-chip").forEach((c) => {
    c.setAttribute("data-active", c.dataset.period === period ? "true" : "false");
  });
  // Re-render from cached snapshot (no backend hit needed)
  if (lastSnapshot) render(lastSnapshot);
}

window.addEventListener("DOMContentLoaded", () => {
  el("btnExpand")?.addEventListener("click", expand);
  el("pill")?.addEventListener("dblclick", expand);
  el("btnCompact")?.addEventListener("click", compact);
  el("btnFullView")?.addEventListener("click", expand);
  el("btnCollapse")?.addEventListener("click", collapse);
  el("btnRefresh")?.addEventListener("click", manualRefresh);

  // Wire the data-view picker
  document.querySelectorAll(".dv-chip").forEach((chip) => {
    chip.addEventListener("click", () => setPeriod(chip.dataset.period));
  });
  el("btnHide")?.addEventListener("click", async () => {
    try { await invoke("hide_window"); } catch (e) {}
  });

  if (window.__TAURI__?.event) {
    window.__TAURI__.event.listen("refresh-now", () => manualRefresh());
  }

  wireSettings();
  applySettings(); // sets theme + opacity + interval timer from stored settings

  // Apply persisted plan-label overrides immediately
  const t1 = el("claudeTier");
  if (t1 && settings.claudeTier) t1.textContent = settings.claudeTier;
  const t2 = el("codexTier");
  if (t2 && settings.codexTier) t2.textContent = settings.codexTier;

  refresh();
  if (settings.autoCheckUpdates) {
    setTimeout(() => checkForUpdates(false), 1500);
  }
});
