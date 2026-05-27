use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Days, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::Write;

use crate::snapshot::UsageSnapshot;

const HISTORY_SCHEMA_VERSION: u32 = 1;
const MIN_HISTORY_INTERVAL_SECS: i64 = 15 * 60;

#[derive(Debug, Serialize)]
struct HistoryEntry<'a> {
    schema_version: u32,
    logged_at: DateTime<Utc>,
    local_date: String,
    snapshot: &'a UsageSnapshot,
}

#[derive(Debug, Deserialize)]
struct HistoryMarker {
    logged_at: DateTime<Utc>,
    local_date: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DailyUsage {
    #[serde(default)]
    pub tokens: DailyTokens,
    #[serde(default)]
    pub requests: u64,
    #[serde(default)]
    pub api_equiv: f64,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct DailyTokens {
    #[serde(default)]
    pub input: u64,
    #[serde(default)]
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub cached_input: u64,
    #[serde(default)]
    pub reasoning: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DailyLedger {
    schema_version: u32,
    #[serde(default)]
    days: BTreeMap<String, DailyLedgerDay>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DailyLedgerDay {
    #[serde(default)]
    claude: Option<DailyUsage>,
    #[serde(default)]
    codex: Option<DailyUsage>,
}

pub fn record_snapshot(snapshot: &UsageSnapshot) -> Result<bool> {
    let now = Utc::now();
    let local_now = Local::now();
    let local_date = local_now.format("%Y-%m-%d").to_string();
    let root = history_root()?;
    std::fs::create_dir_all(&root)?;

    let marker_path = root.join("last-write.json");
    if let Some(marker) = read_marker(&marker_path) {
        let same_day = marker.local_date == local_date;
        let age = now.signed_duration_since(marker.logged_at).num_seconds();
        if same_day && age >= 0 && age < MIN_HISTORY_INTERVAL_SECS {
            return Ok(false);
        }
    }

    let entry = HistoryEntry {
        schema_version: HISTORY_SCHEMA_VERSION,
        logged_at: now,
        local_date: local_date.clone(),
        snapshot,
    };
    let year = local_now.year();
    let log_path = root.join(format!("usage-snapshots-{year}.jsonl"));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    serde_json::to_writer(&mut file, &entry)?;
    file.write_all(b"\n")?;

    let marker = serde_json::json!({
        "logged_at": now,
        "local_date": local_date,
    });
    std::fs::write(marker_path, serde_json::to_string_pretty(&marker)?)?;
    Ok(true)
}

fn history_root() -> Result<std::path::PathBuf> {
    let mut root = dirs::data_local_dir().ok_or_else(|| anyhow!("no local data dir"))?;
    root.push("tally");
    root.push("history");
    Ok(root)
}

pub fn upsert_daily_usage(vendor: &str, rows: &BTreeMap<NaiveDate, DailyUsage>) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let root = history_root()?;
    std::fs::create_dir_all(&root)?;

    let mut by_year: BTreeMap<i32, Vec<(&NaiveDate, &DailyUsage)>> = BTreeMap::new();
    for (date, usage) in rows {
        by_year.entry(date.year()).or_default().push((date, usage));
    }

    for (year, rows) in by_year {
        let path = root.join(format!("daily-usage-{year}.json"));
        let mut ledger = read_daily_ledger(&path).unwrap_or_else(DailyLedger::default);
        ledger.schema_version = 1;
        let mut changed = false;
        for (date, usage) in rows {
            let day = ledger
                .days
                .entry(date.format("%Y-%m-%d").to_string())
                .or_default();
            match vendor {
                "claude" => changed |= upsert_vendor_day(&mut day.claude, usage),
                "codex" => changed |= upsert_vendor_day(&mut day.codex, usage),
                _ => return Err(anyhow!("unknown history vendor: {vendor}")),
            }
        }
        if changed {
            let body = serde_json::to_string_pretty(&ledger)?;
            if std::fs::read_to_string(&path).ok().as_deref() != Some(body.as_str()) {
                std::fs::write(path, body)?;
            }
        }
    }

    Ok(())
}

pub fn load_daily_periods(
    now: DateTime<Utc>,
) -> Result<HashMap<String, HashMap<String, DailyUsage>>> {
    let root = history_root()?;
    if !root.exists() {
        return Ok(HashMap::new());
    }
    let now_local: DateTime<Local> = now.into();
    let today = now_local.date_naive();
    let month_start = NaiveDate::from_ymd_opt(today.year(), today.month(), 1).unwrap_or(today);

    let mut out: HashMap<String, HashMap<String, DailyUsage>> = HashMap::new();
    for vendor in ["claude", "codex"] {
        out.insert(vendor.to_string(), HashMap::new());
    }

    for entry in std::fs::read_dir(root)?.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !(name.starts_with("daily-usage-") && name.ends_with(".json")) {
            continue;
        }
        let Some(ledger) = read_daily_ledger(&path) else {
            continue;
        };
        for (date_text, day) in ledger.days {
            let Ok(date) = NaiveDate::parse_from_str(&date_text, "%Y-%m-%d") else {
                continue;
            };
            let period_keys = period_keys_for_date(date, today, month_start);
            if period_keys.is_empty() {
                continue;
            }
            if let Some(usage) = day.claude {
                add_usage_to_periods(out.get_mut("claude").unwrap(), &period_keys, &usage);
            }
            if let Some(usage) = day.codex {
                add_usage_to_periods(out.get_mut("codex").unwrap(), &period_keys, &usage);
            }
        }
    }

    Ok(out)
}

fn upsert_vendor_day(slot: &mut Option<DailyUsage>, incoming: &DailyUsage) -> bool {
    match slot {
        Some(existing) => existing.keep_max(incoming),
        None => {
            *slot = Some(incoming.clone());
            true
        }
    }
}

fn period_keys_for_date(
    date: NaiveDate,
    today: NaiveDate,
    month_start: NaiveDate,
) -> Vec<&'static str> {
    let mut keys = Vec::new();
    if date == today {
        keys.push("today");
    }
    for (key, days) in [("1d", 1), ("7d", 7), ("14d", 14), ("30d", 30)] {
        let start = today.checked_sub_days(Days::new(days - 1)).unwrap_or(today);
        if date >= start && date <= today {
            keys.push(key);
        }
    }
    if date >= month_start && date <= today {
        keys.push("mtd");
    }
    keys
}

fn add_usage_to_periods(
    periods: &mut HashMap<String, DailyUsage>,
    keys: &[&str],
    usage: &DailyUsage,
) {
    for key in keys {
        periods
            .entry((*key).to_string())
            .or_default()
            .add_assign(usage);
    }
}

impl DailyUsage {
    pub fn add_assign(&mut self, other: &DailyUsage) {
        self.tokens.input += other.tokens.input;
        self.tokens.output += other.tokens.output;
        self.tokens.cache_read += other.tokens.cache_read;
        self.tokens.cache_write += other.tokens.cache_write;
        self.tokens.cached_input += other.tokens.cached_input;
        self.tokens.reasoning += other.tokens.reasoning;
        self.requests += other.requests;
        self.api_equiv += other.api_equiv;
    }

    fn keep_max(&mut self, other: &DailyUsage) -> bool {
        let before = self.clone();
        self.tokens.input = self.tokens.input.max(other.tokens.input);
        self.tokens.output = self.tokens.output.max(other.tokens.output);
        self.tokens.cache_read = self.tokens.cache_read.max(other.tokens.cache_read);
        self.tokens.cache_write = self.tokens.cache_write.max(other.tokens.cache_write);
        self.tokens.cached_input = self.tokens.cached_input.max(other.tokens.cached_input);
        self.tokens.reasoning = self.tokens.reasoning.max(other.tokens.reasoning);
        self.requests = self.requests.max(other.requests);
        self.api_equiv = self.api_equiv.max(other.api_equiv);
        self.tokens.input != before.tokens.input
            || self.tokens.output != before.tokens.output
            || self.tokens.cache_read != before.tokens.cache_read
            || self.tokens.cache_write != before.tokens.cache_write
            || self.tokens.cached_input != before.tokens.cached_input
            || self.tokens.reasoning != before.tokens.reasoning
            || self.requests != before.requests
            || (self.api_equiv - before.api_equiv).abs() > f64::EPSILON
    }
}

fn read_daily_ledger(path: &std::path::Path) -> Option<DailyLedger> {
    let body = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&body).ok()
}

fn read_marker(path: &std::path::Path) -> Option<HistoryMarker> {
    let body = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&body).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolling_30d_is_today_plus_29_prior_calendar_days() {
        let today = NaiveDate::from_ymd_opt(2026, 5, 21).unwrap();
        let month_start = NaiveDate::from_ymd_opt(2026, 5, 1).unwrap();

        let first_in_window = NaiveDate::from_ymd_opt(2026, 4, 22).unwrap();
        assert!(period_keys_for_date(first_in_window, today, month_start).contains(&"30d"));

        let outside_window = NaiveDate::from_ymd_opt(2026, 4, 21).unwrap();
        assert!(!period_keys_for_date(outside_window, today, month_start).contains(&"30d"));
    }

    #[test]
    fn daily_upsert_does_not_shrink_saved_usage() {
        let mut usage = DailyUsage {
            tokens: DailyTokens {
                input: 100,
                output: 50,
                cache_read: 20,
                cache_write: 10,
                cached_input: 30,
                reasoning: 40,
            },
            requests: 5,
            api_equiv: 12.50,
        };
        usage.keep_max(&DailyUsage {
            tokens: DailyTokens {
                input: 10,
                output: 500,
                cache_read: 2,
                cache_write: 1,
                cached_input: 300,
                reasoning: 4,
            },
            requests: 3,
            api_equiv: 8.0,
        });

        assert_eq!(usage.tokens.input, 100);
        assert_eq!(usage.tokens.output, 500);
        assert_eq!(usage.tokens.cached_input, 300);
        assert_eq!(usage.requests, 5);
        assert_eq!(usage.api_equiv, 12.50);
    }

    #[test]
    fn daily_upsert_reports_no_change_for_lower_or_equal_usage() {
        let existing = DailyUsage {
            tokens: DailyTokens {
                input: 100,
                output: 50,
                cache_read: 20,
                cache_write: 10,
                cached_input: 30,
                reasoning: 40,
            },
            requests: 5,
            api_equiv: 12.50,
        };
        let incoming = DailyUsage {
            tokens: DailyTokens {
                input: 90,
                output: 50,
                cache_read: 20,
                cache_write: 10,
                cached_input: 30,
                reasoning: 40,
            },
            requests: 5,
            api_equiv: 12.50,
        };
        let mut slot = Some(existing.clone());

        assert!(!upsert_vendor_day(&mut slot, &incoming));
        assert_eq!(slot.unwrap().tokens.input, existing.tokens.input);
    }

    #[test]
    fn daily_upsert_reports_change_for_growth_or_new_day() {
        let incoming = DailyUsage {
            requests: 1,
            api_equiv: 1.0,
            ..Default::default()
        };
        let mut empty = None;
        assert!(upsert_vendor_day(&mut empty, &incoming));

        let mut existing = Some(incoming.clone());
        let larger = DailyUsage {
            requests: 2,
            api_equiv: 3.0,
            ..Default::default()
        };
        assert!(upsert_vendor_day(&mut existing, &larger));
        assert_eq!(existing.unwrap().requests, 2);
    }
}
