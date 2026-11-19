//! `openocd-link shell`: ham modda terminal, çıkışta her durumda geri alınır.

use super::control;
use crate::error::CliError;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size};
use serde_json::json;
use std::io::{Read, Write};
use std::path::Path;

/// Panik dahil her çıkışta terminal modunu geri alır
struct RawMode;

impl RawMode {
    fn enter() -> std::io::Result<Self> {
        enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
    }
}

pub fn run(dir: &Path, session: &str) -> Result<(), CliError> {
    let (cols, rows) = size().unwrap_or((80, 24));
    let term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into());
    let stream = control::open_raw(
        dir,
        session,
        json!({"op": "shell", "cols": cols, "rows": rows, "term": term}),
    )?;
    let guard = RawMode::enter().map_err(CliError::from)?;
    #[cfg(unix)]
    {
        let (dir, session) = (dir.to_path_buf(), session.to_string());
        if let Ok(mut signals) = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH]) {
            std::thread::spawn(move || {
                for _ in signals.forever() {
                    if let Ok((cols, rows)) = size() {
                        let req = json!({"op": "shell.resize", "cols": cols, "rows": rows});
                        let _ = control::call(&dir, &session, req, |_| {});
                    }
                }
            });
        }
    }
    let mut input = stream.try_clone().map_err(CliError::from)?;
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        let mut stdin = std::io::stdin();
        while let Ok(n) = stdin.read(&mut buf) {
            if n == 0 || input.write_all(&buf[..n]).is_err() {
                break;
            }
        }
    });
    let mut output = stream;
    let mut stdout = std::io::stdout();
    let mut buf = [0u8; 8192];
    while let Ok(n) = output.read(&mut buf) {
        if n == 0 || stdout.write_all(&buf[..n]).and_then(|()| stdout.flush()).is_err() {
            break;
        }
    }
    drop(guard);
    println!();
    Ok(())
}
