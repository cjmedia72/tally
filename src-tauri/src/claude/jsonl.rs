use anyhow::Result;
use chrono::{DateTime, Datelike, Duration, Local, NaiveDate, TimeZone, Utc};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::sync::{Mutex, OnceLock};
use walkdir::WalkDir;

use super::roots::{cowork_session_roots, discover_cowork_session_ids};
use super::types::{ClaudeStats, PeriodStats, TokenWindow};

#[derive(Debug, Deserialize)]
struct ClaudeLine {
    #[serde(default)]
    timestamp: Option<DateTime<Utc>>,
    #[serde(default)]
    message: Option<ClaudeMessage>,
    #[serde(default)]
    entrypoint: Option<String>,
    #[serde(default, rename = "sessionId")]
    session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClaudeMessage {
    #[serde(default)]
    usage: Option<ClaudeUsage>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileSignature {
    files: u64,
    bytes: u64,
    newest_mtime_ns: u128,
    local_year: i32,
    local_ordinal: u32,
}

impl FileSignature {
    fn new(local_now: chrono::DateTime<Local>) -> Self {
        Self {
            files: 0,
            bytes: 0,
            newest_mtime_ns: 0,
            local_year: local_now.year(),
            local_ordinal: local_now.ordinal(),
        }
    }

    fn observe_file(&mut self, len: u64, modified: std::time::SystemTime) {
        if let Ok(delta) = modified.duration_since(std::time::UNIX_EPOCH) {
            self.newest_mtime_ns = self.newest_mtime_ns.max(delta.as_nanos());
        }
        self.files += 1;
        self.bytes = self.bytes.saturating_add(len);
    }
}

fn stats_cache() -> &'static Mutex<Option<(FileSignature, ClaudeStats)>> {
    static CACHE: OnceLock<Mutex<Option<(FileSignature, ClaudeStats)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

pub(crate) fn collect_token_stats() -> Result<ClaudeStats> {
    let mut stats = ClaudeStats::default();
    let mut daily_rows: BTreeMap<NaiveDate, crate::history::DailyUsage> = BTreeMap::new();
    let mut projects_dir = match dirs::home_dir() {
        Some(d) => d,
        None => return Ok(stats),
    };
    projects_dir.push(".claude");
    projects_dir.push("projects");

    let cowork_ids = discover_cowork_session_ids();

    let mut walk_roots: Vec<std::path::PathBuf> = Vec::new();
    if projects_dir.exists() {
        walk_roots.push(projects_dir.clone());
    }
    for cw_root in cowork_session_roots() {
        for entry in WalkDir::new(&cw_root)
            .max_depth(10)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let p = entry.path();
            if p.is_dir()
                && p.ends_with("projects")
                && p.parent()
                    .map(|pp| pp.ends_with(".claude"))
                    .unwrap_or(false)
            {
                walk_roots.push(p.to_path_buf());
            }
        }
    }
    if walk_roots.is_empty() {
        return Ok(stats);
    }

    let now = Utc::now();
    let mut all_message_times: Vec<DateTime<Utc>> = Vec::new();
    let cutoff_30d = now - Duration::days(30);
    let cutoff_14d = now - Duration::days(14);
    let cutoff_7d = now - Duration::days(7);
    let cutoff_1d = now - Duration::days(1);
    let now_local = Local::now();
    let today_start = Local
        .from_local_datetime(&now_local.date_naive().and_hms_opt(0, 0, 0).unwrap())
        .single()
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(now);
    let mtd_start = Local
        .from_local_datetime(
            &chrono::NaiveDate::from_ymd_opt(now_local.year(), now_local.month(), 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        )
        .single()
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or(now);

    let mut jsonl_files: Vec<(std::path::PathBuf, std::fs::Metadata)> = Vec::new();
    let mut signature = FileSignature::new(now_local);

    for entry in walk_roots
        .iter()
        .flat_map(|r| WalkDir::new(r).into_iter())
        .filter_map(|e| e.ok())
    {
        let path = entry.path().to_path_buf();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if let Ok(mtime) = meta.modified() {
            let mt: DateTime<Utc> = mtime.into();
            if mt < cutoff_30d {
                continue;
            }
            signature.observe_file(meta.len(), mtime);
        } else {
            signature.observe_file(meta.len(), std::time::UNIX_EPOCH);
        }
        jsonl_files.push((path, meta));
    }

    if let Some((cached_sig, cached_stats)) = stats_cache().lock().unwrap().as_ref() {
        if *cached_sig == signature {
            return Ok(cached_stats.clone());
        }
    }

    for (path, _meta) in jsonl_files {
        let is_cowork_path = path.components().any(|c| {
            c.as_os_str()
                .to_string_lossy()
                .contains("local-agent-mode-sessions")
        });
        let path_stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let is_cowork_session = is_cowork_path || cowork_ids.contains(path_stem);
        let file = match File::open(path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let reader = BufReader::new(file);
        for line in reader.lines().map_while(|r| r.ok()) {
            let parsed: ClaudeLine = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let is_cowork_event = is_cowork_session
                || parsed
                    .session_id
                    .as_deref()
                    .map(|sid| cowork_ids.contains(sid))
                    .unwrap_or(false);
            let ts = match parsed.timestamp {
                Some(t) => t,
                None => continue,
            };
            let msg = match parsed.message {
                Some(m) => m,
                None => continue,
            };
            let model = msg.model.unwrap_or_default();
            let usage = match msg.usage {
                Some(u) => u,
                None => continue,
            };
            all_message_times.push(ts);
            let msg_cost = crate::pricing::claude_message_cost(
                &model,
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_input_tokens,
                usage.cache_creation_input_tokens,
            );
            let local_ts: DateTime<Local> = ts.into();
            let daily = daily_rows.entry(local_ts.date_naive()).or_default();
            daily.tokens.input += usage.input_tokens;
            daily.tokens.output += usage.output_tokens;
            daily.tokens.cache_read += usage.cache_read_input_tokens;
            daily.tokens.cache_write += usage.cache_creation_input_tokens;
            daily.requests += 1;
            daily.api_equiv += msg_cost;

            let accrue = |p: &mut PeriodStats| {
                add(&mut p.tokens, &usage);
                p.requests += 1;
                p.cost += msg_cost;
            };
            if ts >= today_start {
                accrue(&mut stats.today);
            }
            if ts >= cutoff_1d {
                accrue(&mut stats.d1);
            }
            if ts >= cutoff_7d {
                accrue(&mut stats.d7);
            }
            if ts >= cutoff_14d {
                accrue(&mut stats.d14);
            }
            if ts >= cutoff_30d {
                accrue(&mut stats.d30);
            }
            if ts >= mtd_start {
                accrue(&mut stats.mtd);
            }

            if ts >= cutoff_7d {
                let ep = parsed.entrypoint.unwrap_or_else(|| "unknown".to_string());
                *stats.cost_by_entrypoint_7d.entry(ep).or_insert(0.0) += msg_cost;
            }
            if is_cowork_event {
                if ts >= cutoff_7d {
                    stats.cowork_cost_7d += msg_cost;
                }
                if ts >= today_start {
                    stats.cowork_cost_today += msg_cost;
                }
                if ts >= mtd_start {
                    stats.cowork_cost_mtd += msg_cost;
                }
            }
        }
    }

    if !all_message_times.is_empty() {
        all_message_times.sort();
        stats.last_event_at = Some(*all_message_times.last().unwrap());
    }

    if let Err(e) = crate::history::upsert_daily_usage("claude", &daily_rows) {
        eprintln!("[tally] claude daily history backfill failed: {e}");
    }

    *stats_cache().lock().unwrap() = Some((signature, stats.clone()));
    Ok(stats)
}

fn add(w: &mut TokenWindow, u: &ClaudeUsage) {
    w.input += u.input_tokens;
    w.output += u.output_tokens;
    w.cache_read += u.cache_read_input_tokens;
    w.cache_write += u.cache_creation_input_tokens;
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn file_signature_changes_when_jsonl_tree_changes() {
        let day = Local.with_ymd_and_hms(2026, 5, 27, 8, 0, 0).unwrap();
        let mut a = FileSignature::new(day);
        a.observe_file(
            100,
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(10),
        );

        let mut same = FileSignature::new(day);
        same.observe_file(
            100,
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(10),
        );
        assert_eq!(a, same);

        let mut size_changed = FileSignature::new(day);
        size_changed.observe_file(
            101,
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(10),
        );
        assert_ne!(a, size_changed);

        let mut mtime_changed = FileSignature::new(day);
        mtime_changed.observe_file(
            100,
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(11),
        );
        assert_ne!(a, mtime_changed);
    }

    #[test]
    fn file_signature_changes_on_local_day_rollover() {
        let before_midnight = Local.with_ymd_and_hms(2026, 5, 27, 23, 59, 0).unwrap();
        let after_midnight = Local.with_ymd_and_hms(2026, 5, 28, 0, 1, 0).unwrap();
        let mut a = FileSignature::new(before_midnight);
        let mut b = FileSignature::new(after_midnight);
        let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(10);
        a.observe_file(100, mtime);
        b.observe_file(100, mtime);

        assert_ne!(a, b);
    }
}
