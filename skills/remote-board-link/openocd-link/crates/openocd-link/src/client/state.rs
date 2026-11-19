//! Oturum durum dosyası: `<state-dir>/<oturum>.json`, 0600. Anahtar içermez.

use crate::error::CliError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct State {
    pub pid: u32,
    pub control_port: u16,
    pub token: String,
    pub device: String,
    pub peer: String,
    pub via: String,
    pub sas: String,
    pub fingerprint: String,
    pub started_at: String,
    pub services: Value,
    pub features: Value,
    pub files_dir: String,
}

pub fn default_dir() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(std::env::temp_dir)
        .join("openocd-link")
}

pub fn path(dir: &Path, session: &str) -> PathBuf {
    dir.join(format!("{session}.json"))
}

impl State {
    /// Geçici dosyaya yazıp yeniden adlandırır
    pub fn write(&self, dir: &Path, session: &str) -> std::io::Result<()> {
        std::fs::create_dir_all(dir)?;
        let target = path(dir, session);
        let temp = dir.join(format!("{session}.json.tmp"));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(serde_json::to_string(self)?.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(temp, target)
    }

    pub fn read(dir: &Path, session: &str) -> Result<Self, CliError> {
        let none = || CliError::new(10, "Bağlı oturum yok: önce openocd-link connect");
        let text = std::fs::read_to_string(path(dir, session)).map_err(|_| none())?;
        serde_json::from_str(&text).map_err(|_| none())
    }

    pub fn remove(dir: &Path, session: &str) {
        let _ = std::fs::remove_file(path(dir, session));
    }
}
