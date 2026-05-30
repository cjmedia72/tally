use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

const RELEASES_API: &str = "https://api.github.com/repos/EcomCJ/Tally/releases/latest";

#[derive(Debug, Serialize)]
pub struct UpdateInfo {
    pub current_version: String,
    pub latest_version: Option<String>,
    pub update_available: bool,
    pub release_url: Option<String>,
    pub download_url: Option<String>,
    pub asset_name: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    body: Option<String>,
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

pub fn check(current_version: &str) -> Result<UpdateInfo> {
    let release: GitHubRelease = ureq::get(RELEASES_API)
        .set("User-Agent", "TALLY-Ai-Usage-Monitor")
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| anyhow!("GitHub release check failed: {e}"))?
        .into_json()
        .map_err(|e| anyhow!("GitHub release response parse failed: {e}"))?;

    let latest = clean_version(&release.tag_name);
    let current = clean_version(current_version);
    let update_available = compare_versions(&latest, &current).is_gt();
    let asset = pick_windows_asset(&release.assets);

    Ok(UpdateInfo {
        current_version: current_version.to_string(),
        latest_version: Some(release.tag_name),
        update_available,
        release_url: Some(release.html_url.clone()),
        download_url: asset
            .map(|a| a.browser_download_url.clone())
            .or(Some(release.html_url)),
        asset_name: asset.map(|a| a.name.clone()),
        notes: release.body,
    })
}

fn pick_windows_asset(assets: &[GitHubAsset]) -> Option<&GitHubAsset> {
    assets
        .iter()
        .find(|asset| {
            let name = asset.name.to_ascii_lowercase();
            name.ends_with(".exe") && name.contains("setup")
        })
        .or_else(|| {
            assets.iter().find(|asset| {
                let name = asset.name.to_ascii_lowercase();
                name.ends_with(".msi")
            })
        })
}

fn clean_version(value: &str) -> Vec<u64> {
    value
        .trim()
        .trim_start_matches('v')
        .split(['.', '-', '+'])
        .map(|part| {
            part.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
        })
        .take_while(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u64>().ok())
        .collect()
}

fn compare_versions(a: &[u64], b: &[u64]) -> std::cmp::Ordering {
    let len = a.len().max(b.len());
    for index in 0..len {
        let left = *a.get(index).unwrap_or(&0);
        let right = *b.get(index).unwrap_or(&0);
        match left.cmp(&right) {
            std::cmp::Ordering::Equal => {}
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}
