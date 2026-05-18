use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Local, Utc};
use serde::{Deserialize, Serialize};
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

fn read_marker(path: &std::path::Path) -> Option<HistoryMarker> {
    let body = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&body).ok()
}
