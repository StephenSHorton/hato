//! Local contact book — machines we've paired with.
//!
//! Contacts are keyed by a stable slug `id` and address peers by iroh
//! [`EndpointId`]. After pairing, `hato send --to <id>` dials that id directly.

use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};

use crate::identity::config_dir;

/// Contact book filename under the config dir.
pub const CONTACTS_FILE: &str = "contacts.json";

/// One paired peer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Contact {
    /// Stable local slug used by `send --to` (e.g. `alice`).
    pub id: String,
    /// Friendly display name from pairing (or renamed later).
    pub name: String,
    /// Peer's iroh endpoint id (hex).
    pub endpoint_id: String,
    /// Optional last-known endpoint address string (faster first dial).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_addr: Option<String>,
    /// Unix seconds when we paired.
    pub paired_at: u64,
    /// Unix seconds of last successful transfer, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<u64>,
}

impl Contact {
    /// Parse the stored endpoint id.
    pub fn endpoint_id(&self) -> anyhow::Result<EndpointId> {
        EndpointId::from_str(&self.endpoint_id)
            .map_err(|e| anyhow::anyhow!("contact {}: bad endpoint id: {e}", self.id))
    }
}

/// On-disk contact book.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContactBook {
    pub version: u32,
    pub contacts: Vec<Contact>,
}

impl ContactBook {
    /// Empty book at schema v1.
    pub fn new() -> Self {
        Self {
            version: 1,
            contacts: Vec::new(),
        }
    }

    /// Load from the default config dir (creating an empty book if missing).
    pub fn load() -> anyhow::Result<Self> {
        Self::load_from(&config_dir()?)
    }

    /// Load from `dir/contacts.json`.
    pub fn load_from(dir: &Path) -> anyhow::Result<Self> {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let path = dir.join(CONTACTS_FILE);
        if !path.exists() {
            let book = Self::new();
            book.save_to(dir)?;
            return Ok(book);
        }
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("read contacts {}", path.display()))?;
        let book: Self = serde_json::from_str(&raw)
            .with_context(|| format!("parse contacts {}", path.display()))?;
        Ok(book)
    }

    /// Save to the default config dir.
    pub fn save(&self) -> anyhow::Result<()> {
        self.save_to(&config_dir()?)
    }

    /// Save to `dir/contacts.json`.
    pub fn save_to(&self, dir: &Path) -> anyhow::Result<()> {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let path = dir.join(CONTACTS_FILE);
        let raw = serde_json::to_string_pretty(self).context("serialize contacts")?;
        // Atomic-ish replace: write temp then rename.
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, &raw).with_context(|| format!("write {}", tmp.display()))?;
        fs::rename(&tmp, &path).with_context(|| format!("rename into {}", path.display()))?;
        Ok(())
    }

    /// Path helpers for tests / CLI diagnostics.
    pub fn path() -> anyhow::Result<PathBuf> {
        Ok(config_dir()?.join(CONTACTS_FILE))
    }

    /// Resolve by exact id, then case-insensitive name, then id prefix.
    pub fn resolve(&self, query: &str) -> anyhow::Result<&Contact> {
        let q = query.trim();
        if q.is_empty() {
            bail!("empty contact name");
        }
        if let Some(c) = self.contacts.iter().find(|c| c.id == q) {
            return Ok(c);
        }
        let lower = q.to_ascii_lowercase();
        let by_name: Vec<_> = self
            .contacts
            .iter()
            .filter(|c| c.name.to_ascii_lowercase() == lower)
            .collect();
        if by_name.len() == 1 {
            return Ok(by_name[0]);
        }
        if by_name.len() > 1 {
            bail!("ambiguous contact name {q:?}; use the id instead");
        }
        let by_prefix: Vec<_> = self
            .contacts
            .iter()
            .filter(|c| c.id.starts_with(q) || c.name.to_ascii_lowercase().starts_with(&lower))
            .collect();
        match by_prefix.as_slice() {
            [one] => Ok(one),
            [] => bail!("no contact matching {q:?}; run `hato contacts list`"),
            _ => bail!("ambiguous contact {q:?}; be more specific or use the id"),
        }
    }

    /// Mutable resolve (same rules as [`Self::resolve`]).
    pub fn resolve_mut(&mut self, query: &str) -> anyhow::Result<&mut Contact> {
        let id = self.resolve(query)?.id.clone();
        Ok(self
            .contacts
            .iter_mut()
            .find(|c| c.id == id)
            .expect("id just resolved"))
    }

    /// Whether we already trust this endpoint id.
    pub fn contains_endpoint(&self, id: &EndpointId) -> bool {
        let s = id.to_string();
        self.contacts.iter().any(|c| c.endpoint_id == s)
    }

    /// Look up by endpoint id.
    pub fn by_endpoint(&self, id: &EndpointId) -> Option<&Contact> {
        let s = id.to_string();
        self.contacts.iter().find(|c| c.endpoint_id == s)
    }

    /// Insert or update a contact after pairing. Returns the contact id used.
    pub fn upsert_paired(
        &mut self,
        display_name: &str,
        endpoint_id: EndpointId,
        endpoint_addr: Option<String>,
    ) -> String {
        let ep = endpoint_id.to_string();
        if let Some(existing) = self.contacts.iter_mut().find(|c| c.endpoint_id == ep) {
            existing.name = display_name.trim().to_string();
            if let Some(addr) = endpoint_addr {
                existing.endpoint_addr = Some(addr);
            }
            existing.last_seen = Some(now_secs());
            return existing.id.clone();
        }

        let base = slugify(display_name);
        let id = unique_id(&base, &self.contacts);
        self.contacts.push(Contact {
            id: id.clone(),
            name: display_name.trim().to_string(),
            endpoint_id: ep,
            endpoint_addr,
            paired_at: now_secs(),
            last_seen: Some(now_secs()),
        });
        id
    }

    /// Rename a contact's display name (id unchanged unless `also_id` is true).
    pub fn rename(&mut self, query: &str, new_name: &str) -> anyhow::Result<&Contact> {
        let name = new_name.trim();
        if name.is_empty() {
            bail!("name must not be empty");
        }
        let c = self.resolve_mut(query)?;
        c.name = name.to_string();
        Ok(c)
    }

    /// Remove a contact. Returns the removed record.
    pub fn remove(&mut self, query: &str) -> anyhow::Result<Contact> {
        let id = self.resolve(query)?.id.clone();
        let idx = self
            .contacts
            .iter()
            .position(|c| c.id == id)
            .expect("resolved");
        Ok(self.contacts.remove(idx))
    }

    /// Touch last_seen for a known endpoint.
    pub fn touch(&mut self, endpoint_id: &EndpointId) -> bool {
        let s = endpoint_id.to_string();
        if let Some(c) = self.contacts.iter_mut().find(|c| c.endpoint_id == s) {
            c.last_seen = Some(now_secs());
            true
        } else {
            false
        }
    }
}

/// Unix timestamp in seconds.
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Turn a display name into a stable-ish slug.
pub fn slugify(name: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "contact".into()
    } else {
        out
    }
}

fn unique_id(base: &str, existing: &[Contact]) -> String {
    if !existing.iter().any(|c| c.id == base) {
        return base.to_string();
    }
    for n in 2..10_000 {
        let candidate = format!("{base}-{n}");
        if !existing.iter().any(|c| c.id == candidate) {
            return candidate;
        }
    }
    format!("{base}-{}", now_secs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir() -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("hato-contacts-test-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Alice's PC"), "alice-s-pc");
        assert_eq!(slugify("  "), "contact");
        assert_eq!(slugify("MacBook Pro"), "macbook-pro");
    }

    #[test]
    fn upsert_and_resolve() {
        let dir = temp_dir();
        let mut book = ContactBook::load_from(&dir).unwrap();
        let id_a = SecretKey::generate().public();
        let id_b = SecretKey::generate().public();
        let slug = book.upsert_paired("Alice's PC", id_a, None);
        assert_eq!(slug, "alice-s-pc");
        book.upsert_paired("Bob", id_b, None);
        book.save_to(&dir).unwrap();

        let book = ContactBook::load_from(&dir).unwrap();
        assert_eq!(
            book.resolve("alice-s-pc").unwrap().endpoint_id().unwrap(),
            id_a
        );
        assert_eq!(book.resolve("Bob").unwrap().endpoint_id().unwrap(), id_b);
        assert!(book.contains_endpoint(&id_a));
        assert!(!book.contains_endpoint(&SecretKey::generate().public()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unique_ids_on_collision() {
        let mut book = ContactBook::new();
        let a = SecretKey::generate().public();
        let b = SecretKey::generate().public();
        assert_eq!(book.upsert_paired("Alice", a, None), "alice");
        assert_eq!(book.upsert_paired("Alice", b, None), "alice-2");
    }

    #[test]
    fn remove_and_rename() {
        let mut book = ContactBook::new();
        let id = SecretKey::generate().public();
        book.upsert_paired("Alice", id, None);
        book.rename("alice", "Alicia").unwrap();
        assert_eq!(book.resolve("alice").unwrap().name, "Alicia");
        let removed = book.remove("alice").unwrap();
        assert_eq!(removed.name, "Alicia");
        assert!(book.resolve("alice").is_err());
    }
}
