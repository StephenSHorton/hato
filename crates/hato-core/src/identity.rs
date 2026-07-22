//! Persistent iroh identity and local config for Hato.
//!
//! Each machine keeps one long-lived [`SecretKey`] so its [`EndpointId`] is stable
//! across restarts. Contacts address that id; wiping the key orphans every
//! pairing on both sides.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use iroh::{EndpointId, SecretKey};
use serde::{Deserialize, Serialize};

/// Filename for the raw 32-byte secret key.
pub const IDENTITY_FILE: &str = "identity.secret";
/// Filename for local display-name config.
pub const CONFIG_FILE: &str = "config.json";

/// Override the config root (tests / portable installs). When unset, uses the
/// platform config dir for app `"hato"`.
pub const CONFIG_DIR_ENV: &str = "HATO_CONFIG_DIR";

/// Local app config (not the contact book).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Schema version.
    pub version: u32,
    /// Friendly name shown to peers during pair / offer.
    pub display_name: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: 1,
            display_name: default_display_name(),
        }
    }
}

/// Resolve the Hato config directory, creating it if needed.
pub fn config_dir() -> anyhow::Result<PathBuf> {
    if let Ok(dir) = std::env::var(CONFIG_DIR_ENV) {
        let path = PathBuf::from(dir);
        fs::create_dir_all(&path)
            .with_context(|| format!("create config dir {}", path.display()))?;
        return Ok(path);
    }

    let base = directories::ProjectDirs::from("", "", "hato")
        .context("could not resolve a platform config directory")?
        .config_dir()
        .to_path_buf();
    fs::create_dir_all(&base).with_context(|| format!("create config dir {}", base.display()))?;
    Ok(base)
}

/// Load the secret key from disk, or generate and persist a new one.
///
/// The key file is written with mode `0600` on Unix. Never regenerate an
/// existing key — that would change this machine's [`EndpointId`].
pub fn load_or_create_secret_key() -> anyhow::Result<SecretKey> {
    let dir = config_dir()?;
    load_or_create_secret_key_in(&dir)
}

/// Like [`load_or_create_secret_key`], but under an explicit directory (tests).
pub fn load_or_create_secret_key_in(dir: &Path) -> anyhow::Result<SecretKey> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(IDENTITY_FILE);
    if path.exists() {
        let mut f =
            fs::File::open(&path).with_context(|| format!("open identity {}", path.display()))?;
        let mut buf = [0u8; 32];
        f.read_exact(&mut buf)
            .with_context(|| format!("read identity {}", path.display()))?;
        // Reject truncated/overlong files so we never silently pad keys.
        let mut extra = [0u8; 1];
        match f.read(&mut extra) {
            Ok(0) => {}
            Ok(_) => bail!("identity file {} is longer than 32 bytes", path.display()),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {}
            Err(e) => return Err(e).context("check identity length"),
        }
        return Ok(SecretKey::from_bytes(&buf));
    }

    let key = SecretKey::generate();
    write_secret_key(&path, &key)?;
    Ok(key)
}

fn write_secret_key(path: &Path, key: &SecretKey) -> anyhow::Result<()> {
    let bytes = key.to_bytes();
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create identity {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.set_permissions(fs::Permissions::from_mode(0o600))
            .context("set identity file permissions")?;
    }
    f.write_all(&bytes)
        .with_context(|| format!("write identity {}", path.display()))?;
    f.sync_all().ok();
    Ok(())
}

/// Public endpoint id for the local identity.
pub fn local_endpoint_id() -> anyhow::Result<EndpointId> {
    Ok(load_or_create_secret_key()?.public())
}

/// Load app config, creating defaults if missing.
pub fn load_or_create_config() -> anyhow::Result<AppConfig> {
    let dir = config_dir()?;
    load_or_create_config_in(&dir)
}

/// Like [`load_or_create_config`] under an explicit directory.
pub fn load_or_create_config_in(dir: &Path) -> anyhow::Result<AppConfig> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(CONFIG_FILE);
    if path.exists() {
        let raw =
            fs::read_to_string(&path).with_context(|| format!("read config {}", path.display()))?;
        let cfg: AppConfig = serde_json::from_str(&raw)
            .with_context(|| format!("parse config {}", path.display()))?;
        return Ok(cfg);
    }
    let cfg = AppConfig::default();
    save_config_in(dir, &cfg)?;
    Ok(cfg)
}

/// Persist app config to the default config dir.
pub fn save_config(cfg: &AppConfig) -> anyhow::Result<()> {
    save_config_in(&config_dir()?, cfg)
}

/// Persist app config under `dir`.
pub fn save_config_in(dir: &Path, cfg: &AppConfig) -> anyhow::Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(CONFIG_FILE);
    let raw = serde_json::to_string_pretty(cfg).context("serialize config")?;
    fs::write(&path, raw).with_context(|| format!("write config {}", path.display()))?;
    Ok(())
}

/// Set the local display name and save.
pub fn set_display_name(name: impl Into<String>) -> anyhow::Result<AppConfig> {
    let mut cfg = load_or_create_config()?;
    cfg.display_name = name.into().trim().to_string();
    if cfg.display_name.is_empty() {
        bail!("display name must not be empty");
    }
    save_config(&cfg)?;
    Ok(cfg)
}

fn default_display_name() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "hato".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("hato-id-test-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn identity_persists_across_loads() {
        let dir = temp_dir();
        let a = load_or_create_secret_key_in(&dir).unwrap();
        let b = load_or_create_secret_key_in(&dir).unwrap();
        assert_eq!(a.public(), b.public());
        assert_eq!(a.to_bytes(), b.to_bytes());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_roundtrip() {
        let dir = temp_dir();
        let mut cfg = load_or_create_config_in(&dir).unwrap();
        cfg.display_name = "Test Laptop".into();
        save_config_in(&dir, &cfg).unwrap();
        let loaded = load_or_create_config_in(&dir).unwrap();
        assert_eq!(loaded.display_name, "Test Laptop");
        let _ = fs::remove_dir_all(&dir);
    }
}
