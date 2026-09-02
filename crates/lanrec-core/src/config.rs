//! Persisted user settings.
//!
//! Currently just the names the user gives their adapters. "Ethernet 2" says
//! nothing about which cable that is; "Zum MacBook" does.
//!
//! Keyed on MAC address, not on the Windows friendly name or the interface
//! index. Both of those change: the index is reassigned when adapters are added
//! or removed, and the friendly name is itself user-editable in Windows. The MAC
//! stays with the hardware, which is what the label is actually about.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Labels {
    #[serde(default)]
    labels: BTreeMap<String, String>,
}

impl Labels {
    /// Read the store, falling back to an empty one.
    ///
    /// A missing file is the normal first-run case. A corrupt file is not worth
    /// failing the whole app over either -- the worst outcome is that adapters
    /// show their Windows names again, and the next save repairs it.
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Write the store atomically.
    ///
    /// Straight into place would leave a truncated file if the process died
    /// mid-write, and a truncated file loses every label at once.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)
                .with_context(|| format!("Konfigurationsordner {} anlegen", dir.display()))?;
        }

        let json = serde_json::to_string_pretty(self).context("Labels serialisieren")?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, json).with_context(|| format!("{} schreiben", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("{} ersetzen", path.display()))?;
        Ok(())
    }

    pub fn get(&self, mac: &str) -> Option<&str> {
        self.labels.get(&normalize(mac)).map(String::as_str)
    }

    /// Set a label, or remove it when the text is blank.
    pub fn set(&mut self, mac: &str, label: &str) {
        let label = label.trim();
        if label.is_empty() {
            self.labels.remove(&normalize(mac));
        } else {
            self.labels.insert(normalize(mac), label.to_string());
        }
    }

    pub fn len(&self) -> usize {
        self.labels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.labels.is_empty()
    }
}

/// MAC addresses are compared case-insensitively; store one spelling.
fn normalize(mac: &str) -> String {
    mac.trim().to_ascii_uppercase()
}

/// Where the settings live: `%APPDATA%\lanrec\labels.json`.
///
/// Deliberately not Tauri's per-identifier config dir, so the headless CLI and
/// the app read the same file.
pub fn labels_path() -> Result<PathBuf> {
    let base = std::env::var("APPDATA").context("APPDATA ist nicht gesetzt")?;
    Ok(PathBuf::from(base).join("lanrec").join("labels.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("lanrec-test-{name}-{}.json", std::process::id()));
        p
    }

    #[test]
    fn round_trips_through_a_file() {
        let path = temp("roundtrip");
        let mut l = Labels::default();
        l.set("AA:BB:CC:DD:EE:FF", "Zum MacBook");
        l.save(&path).unwrap();

        let back = Labels::load(&path);
        assert_eq!(back.get("AA:BB:CC:DD:EE:FF"), Some("Zum MacBook"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn mac_case_does_not_matter() {
        let mut l = Labels::default();
        l.set("aa:bb:cc:dd:ee:ff", "Zum MacBook");
        assert_eq!(l.get("AA:BB:CC:DD:EE:FF"), Some("Zum MacBook"));
    }

    #[test]
    fn blank_label_clears_the_entry() {
        let mut l = Labels::default();
        l.set("AA:BB:CC:DD:EE:FF", "Zum MacBook");
        l.set("AA:BB:CC:DD:EE:FF", "   ");
        assert!(l.is_empty());
    }

    #[test]
    fn label_is_trimmed() {
        let mut l = Labels::default();
        l.set("AA:BB:CC:DD:EE:FF", "  Zum MacBook  ");
        assert_eq!(l.get("AA:BB:CC:DD:EE:FF"), Some("Zum MacBook"));
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let l = Labels::load(&temp("does-not-exist"));
        assert!(l.is_empty());
    }

    #[test]
    fn corrupt_file_degrades_to_empty_instead_of_failing() {
        let path = temp("corrupt");
        fs::write(&path, "{ this is not json").unwrap();
        assert!(Labels::load(&path).is_empty());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let path = temp("atomic");
        let mut l = Labels::default();
        l.set("AA:BB:CC:DD:EE:FF", "Zum MacBook");
        l.save(&path).unwrap();
        assert!(!path.with_extension("json.tmp").exists());
        let _ = fs::remove_file(&path);
    }
}
