//! Düz dosya klasörü: yükleme (`.part` + sha256 + yeniden adlandırma), listeleme, silme.

use super::OpError;
use crate::config;
use crate::util::{hex, sha256_of, unix_secs, valid_file_name};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_LIST: usize = 500;

struct Upload {
    transfer: u32,
    name: String,
    file: File,
    part: PathBuf,
    size: u64,
    received: u64,
    hasher: Sha256,
    expected: String,
}

pub struct Files {
    pub cfg: config::Files,
    dir: PathBuf,
    upload: Mutex<Option<Upload>>,
    downloading: AtomicBool,
}

fn io_err(e: std::io::Error) -> OpError {
    OpError::new("io", e.to_string())
}

impl Files {
    /// Klasörü 0700 ile oluşturur ve artık `.part` dosyalarını siler
    pub fn new(cfg: config::Files) -> std::io::Result<Self> {
        fs::create_dir_all(&cfg.dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&cfg.dir, fs::Permissions::from_mode(0o700))?;
        }
        let dir = fs::canonicalize(&cfg.dir)?;
        for entry in fs::read_dir(&dir)?.flatten() {
            if entry.file_name().to_string_lossy().ends_with(".part") {
                let _ = fs::remove_file(entry.path());
            }
        }
        Ok(Self {
            cfg,
            dir,
            upload: Mutex::new(None),
            downloading: AtomicBool::new(false),
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn name(name: &str) -> Result<&str, OpError> {
        if valid_file_name(name) {
            Ok(name)
        } else {
            Err(OpError::new("bad_request", "Geçersiz dosya adı"))
        }
    }

    fn total(&self, except: &str) -> u64 {
        fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                (!name.ends_with(".part") && name != except)
                    .then(|| e.metadata().ok())
                    .flatten()
                    .filter(|m| m.is_file())
                    .map(|m| m.len())
            })
            .sum()
    }

    pub fn list(&self) -> Result<Value, OpError> {
        let mut files = Vec::new();
        let mut truncated = false;
        for entry in fs::read_dir(&self.dir).map_err(io_err)?.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() || name.ends_with(".part") {
                continue;
            }
            if files.len() >= MAX_LIST {
                truncated = true;
                break;
            }
            let mtime = meta.modified().map_or(0, unix_secs);
            files.push(json!({"name": name, "size": meta.len(), "mtime": mtime}));
        }
        files.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(json!({"files": files, "truncated": truncated}))
    }

    pub fn put_begin(&self, name: &str, size: u64, sha256: &str, transfer: u32) -> Result<Value, OpError> {
        let name = Self::name(name)?;
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(OpError::new("bad_request", "sha256 geçersiz"));
        }
        if size > self.cfg.max_file_mb * 1024 * 1024 {
            return Err(OpError::new("too_large", "Dosya max_file_mb sınırını aşıyor"));
        }
        if self.total(name) + size > self.cfg.max_total_mb * 1024 * 1024 {
            return Err(OpError::new("no_space", "Klasör toplam sınırı aşılır"));
        }
        let mut slot = self.upload.lock().unwrap();
        if slot.is_some() || self.downloading.load(Ordering::Acquire) {
            return Err(OpError::new("busy", "Başka bir aktarım sürüyor"));
        }
        let part = self.dir.join(format!("{name}.part"));
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&part).map_err(io_err)?;
        *slot = Some(Upload {
            transfer,
            name: name.to_string(),
            file,
            part,
            size,
            received: 0,
            hasher: Sha256::new(),
            expected: sha256.to_ascii_lowercase(),
        });
        Ok(json!({"path": self.dir.join(name).to_string_lossy()}))
    }

    /// Fazla ya da yazılamayan veri varsa iptal nedenini döner (`file.abort` olayı için)
    pub fn put_data(&self, transfer: u32, data: &[u8]) -> Option<String> {
        let mut slot = self.upload.lock().unwrap();
        let up = slot.as_mut().filter(|u| u.transfer == transfer)?;
        let reason = if up.received + data.len() as u64 > up.size {
            Some("Beklenenden fazla veri geldi".to_string())
        } else {
            up.received += data.len() as u64;
            up.hasher.update(data);
            up.file.write_all(data).err().map(|e| e.to_string())
        };
        if reason.is_some() {
            let up = slot.take().unwrap();
            let _ = fs::remove_file(&up.part);
        }
        reason
    }

    pub fn put_end(&self, transfer: u32) -> Result<Value, OpError> {
        let mut slot = self.upload.lock().unwrap();
        if slot.as_ref().is_none_or(|u| u.transfer != transfer) {
            return Err(OpError::new("bad_request", "Etkin yükleme yok"));
        }
        let up = slot.take().unwrap();
        let actual = hex(&up.hasher.finalize());
        if up.received != up.size || actual != up.expected {
            let _ = fs::remove_file(&up.part);
            return Err(OpError::new("checksum", "Boyut ya da sha256 tutmadı"));
        }
        up.file.sync_all().map_err(io_err)?;
        let target = self.dir.join(&up.name);
        fs::rename(&up.part, &target).map_err(io_err)?;
        Ok(json!({"path": target.to_string_lossy(), "size": up.size, "sha256": actual}))
    }

    /// Yarım kalan yüklemeyi siler (oturum koptu)
    pub fn abort(&self) {
        if let Some(up) = self.upload.lock().unwrap().take() {
            let _ = fs::remove_file(&up.part);
        }
        self.downloading.store(false, Ordering::Release);
    }

    /// İndirme başlar: dosyayı, boyutunu ve sha256 değerini döner
    pub fn get_begin(&self, name: &str) -> Result<(File, u64, String), OpError> {
        let name = Self::name(name)?;
        let path = self.dir.join(name);
        let file = File::open(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                OpError::new("not_found", "Dosya yok")
            } else {
                io_err(e)
            }
        })?;
        if !file.metadata().map_err(io_err)?.is_file() {
            return Err(OpError::new("not_found", "Dosya yok"));
        }
        if self.upload.lock().unwrap().is_some()
            || self.downloading.swap(true, Ordering::AcqRel)
        {
            return Err(OpError::new("busy", "Başka bir aktarım sürüyor"));
        }
        let finish = || self.downloading.store(false, Ordering::Release);
        let size = file.metadata().map_err(|e| { finish(); io_err(e) })?.len();
        let sha = sha256_of(&path).map_err(|e| { finish(); io_err(e) })?;
        Ok((file, size, sha))
    }

    pub fn get_end(&self) {
        self.downloading.store(false, Ordering::Release);
    }

    pub fn delete(&self, name: &str) -> Result<(), OpError> {
        let name = Self::name(name)?;
        fs::remove_file(self.dir.join(name)).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                OpError::new("not_found", "Dosya yok")
            } else {
                io_err(e)
            }
        })
    }
}
