//! État propre à l'interface, absent du stockage presage : dernière lecture, non lus,
//! statut de distribution. Ne contient que des identifiants et des horodatages, jamais de texte.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default, Clone)]
pub struct ThreadMeta {
    pub last_ts: u64,
    pub last_read: u64,
    pub unread: u32,
}

#[derive(Serialize, Deserialize, Default)]
pub struct Sidecar {
    pub threads: HashMap<String, ThreadMeta>,
    /// Horodatage du message envoyé → 1 distribué, 2 lu.
    pub status: BTreeMap<u64, u8>,
    #[serde(skip)]
    path: PathBuf,
    #[serde(skip)]
    dirty: bool,
}

const MAX_STATUS: usize = 5000;

impl Sidecar {
    pub fn load(path: PathBuf) -> Self {
        let mut sidecar: Sidecar = std::fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        sidecar.path = path;
        sidecar
    }

    pub fn meta(&mut self, key: &str) -> &mut ThreadMeta {
        self.dirty = true;
        self.threads.entry(key.to_string()).or_default()
    }

    pub fn set_status(&mut self, ts: u64, level: u8) {
        let entry = self.status.entry(ts).or_insert(0);
        if *entry < level {
            *entry = level;
            self.dirty = true;
        }
        while self.status.len() > MAX_STATUS {
            self.status.pop_first();
        }
    }

    pub fn save_if_dirty(&mut self) {
        if !self.dirty || self.path.as_os_str().is_empty() {
            return;
        }
        if let Ok(json) = serde_json::to_vec(self) {
            let tmp = self.path.with_extension("tmp");
            if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, &self.path).is_ok() {
                self.dirty = false;
            }
        }
    }
}
