//! İstemci tarafı oturum: istek/yanıt eşleştirme, olaylar ve veri kanalları.

use crate::error::CliError;
use crate::link::Link;
use crate::wire;
use openocd_link_core::tunnel::Tunnel;
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type ExecWatchers = Mutex<HashMap<u32, Sender<(u8, Vec<u8>)>>>;

pub enum FileMsg {
    Data(Vec<u8>),
    End,
    Abort(String),
}

pub enum ShellMsg {
    Data(Vec<u8>),
    Exit,
}

pub struct Remote {
    pub link: Arc<Link>,
    pub hello: Value,
    next_id: AtomicU32,
    next_transfer: AtomicU32,
    pending: Mutex<HashMap<u32, Sender<Value>>>,
    files: Mutex<HashMap<u32, Sender<FileMsg>>>,
    execs: ExecWatchers,
    shell: Mutex<Option<Sender<ShellMsg>>>,
}

fn protocol(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

impl Remote {
    pub fn new(link: Arc<Link>, hello: Value) -> Arc<Self> {
        Arc::new(Self {
            link,
            hello,
            next_id: AtomicU32::new(1),
            next_transfer: AtomicU32::new(1),
            pending: Mutex::default(),
            files: Mutex::default(),
            execs: Mutex::default(),
            shell: Mutex::default(),
        })
    }

    pub fn alloc_id(&self) -> u32 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn alloc_transfer(&self) -> u32 {
        self.next_transfer.fetch_add(1, Ordering::Relaxed)
    }

    /// İsteği gönderir; yanıt dönen kanala gelir
    pub fn send_request(&self, id: u32, op: &str, fields: Value) -> Result<Receiver<Value>, CliError> {
        let mut body = match fields {
            Value::Object(map) => map,
            _ => Map::new(),
        };
        body.insert("id".into(), id.into());
        body.insert("op".into(), op.into());
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id, tx);
        if let Err(e) = self.link.send_json(&Value::Object(body)) {
            self.pending.lock().unwrap().remove(&id);
            return Err(CliError::new(9, format!("Bağlantı koptu: {e}")));
        }
        Ok(rx)
    }

    pub fn await_response(&self, id: u32, rx: &Receiver<Value>, timeout: Duration) -> Result<Value, CliError> {
        let response = rx.recv_timeout(timeout).map_err(|_| {
            self.pending.lock().unwrap().remove(&id);
            CliError::new(1, "Uzak servis yanıt vermedi")
        })?;
        if response["ok"] == true {
            Ok(response)
        } else {
            let msg = response["error"].as_str().unwrap_or("Uzak hata");
            Err(CliError::new(1, msg))
        }
    }

    pub fn request(&self, op: &str, fields: Value, timeout: Duration) -> Result<Value, CliError> {
        let id = self.alloc_id();
        let rx = self.send_request(id, op, fields)?;
        self.await_response(id, &rx, timeout)
    }

    pub fn watch_file(&self, transfer: u32) -> Receiver<FileMsg> {
        let (tx, rx) = channel();
        self.files.lock().unwrap().insert(transfer, tx);
        rx
    }

    pub fn unwatch_file(&self, transfer: u32) {
        self.files.lock().unwrap().remove(&transfer);
    }

    pub fn watch_exec(&self, id: u32) -> Receiver<(u8, Vec<u8>)> {
        let (tx, rx) = channel();
        self.execs.lock().unwrap().insert(id, tx);
        rx
    }

    pub fn unwatch_exec(&self, id: u32) {
        self.execs.lock().unwrap().remove(&id);
    }

    pub fn watch_shell(&self) -> Receiver<ShellMsg> {
        let (tx, rx) = channel();
        *self.shell.lock().unwrap() = Some(tx);
        rx
    }

    pub fn unwatch_shell(&self) {
        *self.shell.lock().unwrap() = None;
    }

    pub fn send_file(&self, transfer: u32, data: &[u8]) -> io::Result<()> {
        let mut payload = transfer.to_be_bytes().to_vec();
        payload.extend_from_slice(data);
        self.link.send(wire::FILE, &payload)
    }

    pub fn send_shell(&self, kind: u8, data: &[u8]) -> io::Result<()> {
        let mut payload = vec![kind];
        payload.extend_from_slice(data);
        self.link.send(wire::SHELL, &payload)
    }

    /// Okuma döngüsünün çerçeve işleyicisi
    pub fn frame(&self, tunnels: &[Arc<Tunnel>], channel: u8, payload: Vec<u8>) -> io::Result<()> {
        match channel {
            wire::CONTROL => {
                let value: Value = serde_json::from_slice(&payload).map_err(|_| protocol("JSON okunamadı"))?;
                self.control(value);
                Ok(())
            }
            0x01..=0x08 => tunnels
                .get(usize::from(channel) - 1)
                .ok_or_else(|| protocol("Tanımsız tünel kanalı"))?
                .receive(&payload),
            wire::FILE => {
                let (head, data) = payload
                    .split_first_chunk::<4>()
                    .ok_or_else(|| protocol("Kısa dosya çerçevesi"))?;
                if let Some(tx) = self.files.lock().unwrap().get(&u32::from_be_bytes(*head)) {
                    let _ = tx.send(FileMsg::Data(data.to_vec()));
                }
                Ok(())
            }
            wire::EXEC_OUT => {
                let (head, rest) = payload
                    .split_first_chunk::<4>()
                    .ok_or_else(|| protocol("Kısa exec çerçevesi"))?;
                let (&kind, data) = rest.split_first().ok_or_else(|| protocol("Kısa exec çerçevesi"))?;
                if let Some(tx) = self.execs.lock().unwrap().get(&u32::from_be_bytes(*head)) {
                    let _ = tx.send((kind, data.to_vec()));
                }
                Ok(())
            }
            wire::SHELL => {
                if let Some((&wire::SHELL_DATA, data)) = payload.split_first()
                    && let Some(tx) = self.shell.lock().unwrap().as_ref()
                {
                    let _ = tx.send(ShellMsg::Data(data.to_vec()));
                }
                Ok(())
            }
            _ => Err(protocol("Servisten beklenmeyen kanal")),
        }
    }

    fn control(&self, value: Value) {
        if let Some(event) = value["event"].as_str() {
            let transfer = value["transfer"].as_u64().and_then(|t| u32::try_from(t).ok());
            match (event, transfer) {
                ("file.end", Some(t)) => self.file_msg(t, FileMsg::End),
                ("file.abort", Some(t)) => {
                    let why = value["error"].as_str().unwrap_or("Aktarım iptal edildi");
                    self.file_msg(t, FileMsg::Abort(why.to_string()));
                }
                ("shell.exit", _) => {
                    if let Some(tx) = self.shell.lock().unwrap().as_ref() {
                        let _ = tx.send(ShellMsg::Exit);
                    }
                }
                _ => {}
            }
            return;
        }
        let id = value["id"].as_u64().and_then(|i| u32::try_from(i).ok());
        if let Some(tx) = id.and_then(|i| self.pending.lock().unwrap().remove(&i)) {
            let _ = tx.send(value);
        }
    }

    fn file_msg(&self, transfer: u32, msg: FileMsg) {
        if let Some(tx) = self.files.lock().unwrap().get(&transfer) {
            let _ = tx.send(msg);
        }
    }

    pub fn info(&self) -> Value {
        json!({
            "device": self.hello["device"],
            "services": self.hello["services"],
            "features": self.hello["features"],
            "files_dir": self.hello["files_dir"],
        })
    }
}
