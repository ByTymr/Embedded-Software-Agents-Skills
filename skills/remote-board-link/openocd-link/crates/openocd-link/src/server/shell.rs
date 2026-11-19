//! Etkileşimli kabuk: sunucuda PTY, tek kabuk.

use super::OpError;
use crate::config;
use crate::link::Link;
use crate::wire;
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde_json::json;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;

struct Active {
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
}

#[derive(Default)]
pub struct Shell {
    active: Arc<Mutex<Option<Active>>>,
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

impl Shell {
    pub fn open(
        &self,
        cfg: &config::Shell,
        link: &Arc<Link>,
        cols: u16,
        rows: u16,
        term: &str,
    ) -> Result<(), OpError> {
        let mut slot = self.active.lock().unwrap();
        if slot.is_some() {
            return Err(OpError::new("busy", "Kabuk zaten açık"));
        }
        let fail = |e: String| OpError::new("io", format!("Kabuk açılamadı: {e}"));
        let pair = native_pty_system()
            .openpty(size(cols, rows))
            .map_err(|e| fail(e.to_string()))?;
        let mut cmd = CommandBuilder::new(&cfg.program);
        cmd.args(&cfg.args);
        cmd.env("TERM", term);
        if let Some(home) = dirs::home_dir() {
            cmd.cwd(home);
        }
        let mut child = pair.slave.spawn_command(cmd).map_err(|e| fail(e.to_string()))?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(|e| fail(e.to_string()))?;
        let writer = pair.master.take_writer().map_err(|e| fail(e.to_string()))?;
        *slot = Some(Active {
            writer,
            master: pair.master,
            killer: child.clone_killer(),
        });
        drop(slot);
        let out = link.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut payload = vec![wire::SHELL_DATA];
                payload.extend_from_slice(&buf[..n]);
                if out.send(wire::SHELL, &payload).is_err() {
                    break;
                }
            }
        });
        let (active, link) = (self.active.clone(), link.clone());
        thread::spawn(move || {
            let code = child.wait().ok().map(|s| s.exit_code() as i32);
            *active.lock().unwrap() = None;
            let _ = link.send_json(&json!({"event": "shell.exit", "code": code}));
        });
        Ok(())
    }

    /// İstemcinin SHELL çerçevesi
    pub fn input(&self, payload: &[u8]) -> std::io::Result<()> {
        let Some((&kind, rest)) = payload.split_first() else {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Boş kabuk çerçevesi"));
        };
        let mut slot = self.active.lock().unwrap();
        let Some(active) = slot.as_mut() else {
            return Ok(());
        };
        match kind {
            wire::SHELL_DATA if rest.len() <= 8192 => {
                active.writer.write_all(rest)?;
                active.writer.flush()
            }
            wire::SHELL_RESIZE if rest.len() == 4 => {
                let cols = u16::from_be_bytes([rest[0], rest[1]]).clamp(1, 1000);
                let rows = u16::from_be_bytes([rest[2], rest[3]]).clamp(1, 1000);
                active.master.resize(size(cols, rows)).map_err(std::io::Error::other)
            }
            wire::SHELL_EOF => active.writer.write_all(&[4]),
            _ => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Geçersiz kabuk çerçevesi")),
        }
    }

    pub fn close(&self) {
        if let Some(mut active) = self.active.lock().unwrap().take() {
            let _ = active.killer.kill();
        }
    }
}
