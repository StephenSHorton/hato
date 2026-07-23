//! Toru-style auto-update: GitHub Releases → download NSIS installer → `/S` → quit.
//!
//! We deliberately re-run the real per-user NSIS installer (not an in-place exe
//! swap) so registry / uninstall / shortcuts stay coherent. See Toru's
//! `internal/update` package for the reference design.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::AppHandle;
#[cfg(windows)]
use tauri::Emitter;

const REPO: &str = "StephenSHorton/hato";
#[cfg(windows)]
const EVENT_INSTALLING: &str = "update:installing";

/// Shared install guard + app handle for the updater.
pub struct UpdateState {
    installing: AtomicBool,
    /// Used on Windows to emit `update:installing` and exit after launching NSIS.
    #[cfg_attr(not(windows), allow(dead_code))]
    app: AppHandle,
}

impl UpdateState {
    pub fn new(app: AppHandle) -> Self {
        Self {
            installing: AtomicBool::new(false),
            app,
        }
    }
}

/// What the frontend needs when an update is available.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub version: String,
    pub notes: String,
    pub asset_url: String,
    pub asset_name: String,
    pub sha256: String,
    pub published_at: String,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    published_at: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

/// Running version: `"dev"` in debug builds (never auto-updates); release builds
/// use the stamped Cargo package version.
pub fn current_version() -> String {
    if cfg!(debug_assertions) {
        "dev".into()
    } else {
        env!("CARGO_PKG_VERSION").into()
    }
}

/// Fire-and-forget startup auto-update (errors are logged, never fatal).
pub fn spawn_auto_update(state: Arc<UpdateState>) {
    tauri::async_runtime::spawn(async move {
        match check_for_update_inner().await {
            Ok(Some(info)) => {
                eprintln!("hato: auto-updating to v{}", info.version);
                if let Err(e) = download_and_install_inner(&state, info).await {
                    eprintln!("hato: auto-update install failed: {e}");
                }
            }
            Ok(None) => {}
            Err(e) => eprintln!("hato: auto-update check failed: {e}"),
        }
    });
}

#[tauri::command]
pub fn get_version() -> String {
    current_version()
}

#[tauri::command]
pub async fn check_for_update() -> Result<Option<UpdateInfo>, String> {
    check_for_update_inner().await.map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn download_and_install(
    state: tauri::State<'_, Arc<UpdateState>>,
    info: UpdateInfo,
) -> Result<(), String> {
    download_and_install_inner(&state, info)
        .await
        .map_err(|e| e.to_string())
}

async fn check_for_update_inner() -> anyhow::Result<Option<UpdateInfo>> {
    let current = current_version();
    if current.is_empty() || current == "dev" {
        return Ok(None);
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(format!("hato/{current} (+https://github.com/{REPO})"))
        .build()?;

    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;

    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("github releases: {status}: {}", body.trim());
    }

    let rel: GhRelease = resp.json().await?;
    if !is_newer(&rel.tag_name, &current) {
        return Ok(None);
    }

    let mut installer: Option<GhAsset> = None;
    let mut sums_url: Option<String> = None;
    for a in rel.assets {
        let n = a.name.to_ascii_lowercase();
        if n.ends_with("-installer.exe") && n.contains("windows") {
            installer = Some(a);
        } else if n == "sha256sums" {
            sums_url = Some(a.browser_download_url);
        }
    }
    let installer = installer.ok_or_else(|| {
        anyhow::anyhow!("release {} has no windows installer asset", rel.tag_name)
    })?;

    let mut sha256 = String::new();
    if let Some(sums) = sums_url {
        if let Ok(sum) = fetch_checksum(&client, &sums, &installer.name).await {
            sha256 = sum;
        }
    }

    Ok(Some(UpdateInfo {
        version: rel.tag_name.trim_start_matches('v').to_string(),
        notes: rel.body,
        asset_url: installer.browser_download_url,
        asset_name: installer.name,
        sha256,
        published_at: rel.published_at,
    }))
}

async fn fetch_checksum(client: &reqwest::Client, url: &str, name: &str) -> anyhow::Result<String> {
    let text = client.get(url).send().await?.text().await?;
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() == 2 {
            let file = fields[1].trim_start_matches('*');
            if Path::new(file).file_name().and_then(|s| s.to_str()) == Some(name) {
                return Ok(fields[0].to_ascii_lowercase());
            }
        }
    }
    anyhow::bail!("no checksum for {name}");
}

async fn download_and_install_inner(state: &UpdateState, info: UpdateInfo) -> anyhow::Result<()> {
    if state
        .installing
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Ok(()); // already in flight
    }

    let result = download_and_launch(state, &info).await;
    if result.is_err() {
        state.installing.store(false, Ordering::SeqCst);
    }
    result
}

async fn download_and_launch(state: &UpdateState, info: &UpdateInfo) -> anyhow::Result<()> {
    let dir = std::env::temp_dir().join("hato-update");
    std::fs::create_dir_all(&dir)?;
    let dst: PathBuf = dir.join(&info.asset_name);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .user_agent(format!(
            "hato/{} (+https://github.com/{REPO})",
            current_version()
        ))
        .build()?;

    let resp = client.get(&info.asset_url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("download {}: {}", info.asset_url, resp.status());
    }
    let bytes = resp.bytes().await?;
    std::fs::write(&dst, &bytes)?;

    if !info.sha256.is_empty() {
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let got = format!("{:x}", hasher.finalize());
        if !got.eq_ignore_ascii_case(&info.sha256) {
            let _ = std::fs::remove_file(&dst);
            anyhow::bail!("update checksum mismatch: got {got}, want {}", info.sha256);
        }
    }

    // Detached silent install so the installer outlives our process.
    // Auto-update is Windows/NSIS only (same as Toru).
    #[cfg(not(windows))]
    {
        let _ = (&state.app, &dst);
        anyhow::bail!("auto-update is only supported on Windows");
    }

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut cmd = std::process::Command::new(&dst);
        cmd.arg("/S")
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        cmd.spawn()
            .map_err(|e| anyhow::anyhow!("launch installer: {e}"))?;

        let _ = state.app.emit(EVENT_INSTALLING, &info.version);
        // Give the webview a beat to paint "Updating…", then unlock Hato.exe.
        let app = state.app.clone();
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(400)).await;
            app.exit(0);
        });
        Ok(())
    }
}

fn is_newer(tag: &str, current: &str) -> bool {
    if current.is_empty() || current == "dev" {
        return false;
    }
    cmp_semver(tag.trim_start_matches('v'), current.trim_start_matches('v')) > 0
}

fn cmp_semver(a: &str, b: &str) -> i32 {
    let (na, prea) = parse_ver(a);
    let (nb, preb) = parse_ver(b);
    for i in 0..3 {
        if na[i] != nb[i] {
            return if na[i] > nb[i] { 1 } else { -1 };
        }
    }
    match (prea.is_empty(), preb.is_empty()) {
        (true, true) => 0,
        (true, false) => 1, // final > prerelease
        (false, true) => -1,
        (false, false) => prea.cmp(&preb) as i32,
    }
}

fn parse_ver(v: &str) -> ([u32; 3], String) {
    let v = v.split('+').next().unwrap_or(v);
    let (num, pre) = match v.split_once('-') {
        Some((n, p)) => (n, p.to_string()),
        None => (v, String::new()),
    };
    let mut out = [0u32; 3];
    for (i, p) in num.split('.').take(3).enumerate() {
        out[i] = p.trim().parse().unwrap_or(0);
    }
    (out, pre)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_newer() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.2.0"));
        assert!(!is_newer("v1.0.0", "dev"));
        assert!(is_newer("v1.0.0", "1.0.0-beta"));
    }
}
