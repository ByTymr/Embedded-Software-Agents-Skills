//! Komut çalıştırma: kabuk yok, canlı çıktı, süre sınırı, süreç grubu öldürme.

use super::OpError;
use crate::link::Link;
use crate::wire;
use serde_json::{Value, json};
use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const STREAM_LIMIT: usize = 1024 * 1024;
const CHUNK: usize = 8192;

/// `cwd` dosya klasörüne göre göreli olmalı ve kanonik hâli klasörün dışına çıkmamalı
pub fn resolve_cwd(base: &Path, cwd: Option<&str>) -> Result<std::path::PathBuf, OpError> {
    let Some(cwd) = cwd.filter(|c| !c.is_empty()) else {
        return Ok(base.to_path_buf());
    };
    if Path::new(cwd).is_absolute() {
        return Err(OpError::new("bad_request", "cwd göreli olmalı"));
    }
    let full = std::fs::canonicalize(base.join(cwd))
        .map_err(|_| OpError::new("bad_request", "cwd bulunamadı"))?;
    if !full.starts_with(base) {
        return Err(OpError::new("bad_request", "cwd dosya klasörünün dışına çıkıyor"));
    }
    Ok(full)
}

fn forward(mut stream: impl Read, link: Arc<Link>, id: u32, kind: u8, truncated: Arc<AtomicBool>) {
    let mut sent = 0usize;
    let mut buf = [0u8; CHUNK];
    loop {
        let n = match stream.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let room = STREAM_LIMIT.saturating_sub(sent).min(n);
        if room < n {
            truncated.store(true, Ordering::Release);
        }
        if room > 0 {
            let mut payload = id.to_be_bytes().to_vec();
            payload.push(kind);
            payload.extend_from_slice(&buf[..room]);
            sent += room;
            if link.send(wire::EXEC_OUT, &payload).is_err() {
                return;
            }
        }
    }
}

fn kill_group(pgid: i32) {
    unsafe { libc::kill(-pgid, libc::SIGKILL) };
}

pub fn run(
    link: &Arc<Link>,
    id: u32,
    base: &Path,
    argv: &[String],
    cwd: Option<&str>,
    timeout: Duration,
) -> Result<Value, OpError> {
    let dir = resolve_cwd(base, cwd)?;
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| OpError::new("io", format!("Komut başlatılamadı: {e}")))?;
    let pgid = child.id() as i32;
    let truncated = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = [
        child.stdout.take().map(|s| (Box::new(s) as Box<dyn Read + Send>, 1u8)),
        child.stderr.take().map(|s| (Box::new(s) as Box<dyn Read + Send>, 2u8)),
    ]
    .into_iter()
    .flatten()
    .map(|(stream, kind)| {
        let (link, truncated) = (link.clone(), truncated.clone());
        thread::spawn(move || forward(stream, link, id, kind, truncated))
    })
    .collect();
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if Instant::now() >= deadline {
            timed_out = true;
            kill_group(pgid);
            break child.wait().ok();
        }
        if !link.is_alive() {
            kill_group(pgid);
            break child.wait().ok();
        }
        thread::sleep(Duration::from_millis(20));
    };
    // Arka planda kalan torunlar boruları açık tutmasın
    kill_group(pgid);
    for reader in readers {
        let _ = reader.join();
    }
    Ok(json!({
        "exit_code": status.and_then(|s| s.code()),
        "signal": status.and_then(|s| s.signal()),
        "timed_out": timed_out,
        "truncated": truncated.load(Ordering::Acquire),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cwd_must_stay_inside() {
        let base = std::fs::canonicalize(std::env::temp_dir()).unwrap().join(format!("ol-exec-{}", std::process::id()));
        std::fs::create_dir_all(base.join("alt")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        assert_eq!(resolve_cwd(&base, None).unwrap(), base);
        assert_eq!(resolve_cwd(&base, Some("alt")).unwrap(), base.join("alt"));
        for bad in ["../", "..", "/etc", "yok"] {
            assert_eq!(resolve_cwd(&base, Some(bad)).unwrap_err().code, "bad_request", "{bad}");
        }
        std::fs::remove_dir_all(base).ok();
    }
}
