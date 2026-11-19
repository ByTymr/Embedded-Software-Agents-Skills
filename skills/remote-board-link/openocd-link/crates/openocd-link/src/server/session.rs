//! Tek istemci oturumu: çerçeveleri dağıtır, kontrol isteklerini işler.

use super::exec;
use super::shell::Shell;
use super::{Ctx, OpError};
use crate::link::{Link, read_loop};
use crate::wire;
use openocd_link_core::log::log;
use openocd_link_core::net::Secure;
use openocd_link_core::tunnel::Tunnel;
use serde_json::{Map, Value, json};
use std::io::{self, Read};
use std::net::IpAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const MAX_REQUESTS: usize = 4;
const MAX_EXECS: usize = 2;

struct Session {
    ctx: Arc<Ctx>,
    link: Arc<Link>,
    shell: Shell,
    inflight: AtomicUsize,
}

fn protocol(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

pub fn run(ctx: &Arc<Ctx>, secure: Secure, name: &str, ip: IpAddr, sas: &str) {
    log(format!(
        "Oturum açıldı: istemci {name}, adres {ip}, sas {sas}, anahtar {}",
        ctx.key.fingerprint()
    ));
    let Ok((link, reader)) = Link::new(secure) else {
        return;
    };
    let tunnels: Vec<Arc<Tunnel>> = ctx
        .cfg
        .forward
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let tunnel = Tunnel::new();
            // run'dan ÖNCE: ilk turda karşıya GRANT gider
            tunnel.allowed_port.store(f.port, Ordering::Release);
            let (alive, out) = (link.clone(), link.clone());
            let channel = wire::tunnel_channel(i);
            tunnel.run(true, move || alive.is_alive(), move |packet| {
                out.send(channel, packet)
            });
            tunnel
        })
        .collect();
    ctx.openocd.set_notify(Some(link.clone()));
    let session = Arc::new(Session {
        ctx: ctx.clone(),
        link: link.clone(),
        shell: Shell::default(),
        inflight: AtomicUsize::new(0),
    });
    let started = Instant::now();
    let reason = read_loop(&link, reader, |channel, payload| {
        session.frame(&tunnels, channel, payload)
    });
    for tunnel in &tunnels {
        tunnel.close();
    }
    session.shell.close();
    ctx.files.abort();
    ctx.openocd.set_notify(None);
    log(format!(
        "Oturum kapandı: {} sn, neden: {reason}, gönderilen {} bayt, alınan {} bayt",
        started.elapsed().as_secs(),
        link.sent.load(Ordering::Relaxed),
        link.received.load(Ordering::Relaxed)
    ));
}

impl Session {
    fn frame(self: &Arc<Self>, tunnels: &[Arc<Tunnel>], channel: u8, payload: Vec<u8>) -> io::Result<()> {
        match channel {
            wire::CONTROL => {
                self.control(&payload);
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
                let transfer = u32::from_be_bytes(*head);
                if let Some(reason) = self.ctx.files.put_data(transfer, data) {
                    let _ = self.link.send_json(
                        &json!({"event": "file.abort", "transfer": transfer, "error": reason}),
                    );
                }
                Ok(())
            }
            wire::SHELL => {
                if self.ctx.cfg.shell.enabled {
                    self.shell.input(&payload)
                } else {
                    Ok(())
                }
            }
            _ => Err(protocol("İstemciden beklenmeyen kanal")),
        }
    }

    fn control(self: &Arc<Self>, payload: &[u8]) {
        let Ok(request) = serde_json::from_slice::<Value>(payload) else {
            self.respond(0, Err(OpError::new("bad_request", "JSON okunamadı")));
            return;
        };
        let id = request["id"].as_u64().filter(|&i| i > 0 && i <= u64::from(u32::MAX));
        let Some(id) = id.map(|i| i as u32) else {
            self.respond(0, Err(OpError::new("bad_request", "id geçersiz")));
            return;
        };
        if self.inflight.fetch_add(1, Ordering::AcqRel) >= MAX_REQUESTS {
            self.inflight.fetch_sub(1, Ordering::AcqRel);
            self.respond(id, Err(OpError::new("busy", "Çok fazla eşzamanlı istek")));
            return;
        }
        let this = self.clone();
        thread::spawn(move || {
            let op = request["op"].as_str().unwrap_or("").to_string();
            let result = if op == "file.get" {
                this.get(id, &request)
            } else {
                this.handle(id, &op, &request)
            };
            if let Some(result) = result {
                this.respond(id, result);
            }
            this.inflight.fetch_sub(1, Ordering::AcqRel);
        });
    }

    fn respond(&self, id: u32, result: Result<Value, OpError>) {
        let mut body = Map::new();
        body.insert("id".into(), id.into());
        match result {
            Ok(Value::Object(fields)) => {
                body.insert("ok".into(), true.into());
                body.extend(fields);
            }
            Ok(_) => {
                body.insert("ok".into(), true.into());
            }
            Err(e) => {
                body.insert("ok".into(), false.into());
                body.insert("code".into(), e.code.into());
                body.insert("error".into(), e.msg.into());
            }
        }
        let _ = self.link.send_json(&Value::Object(body));
    }

    fn require(&self, enabled: bool, what: &str) -> Result<(), OpError> {
        if enabled {
            Ok(())
        } else {
            Err(OpError::new("disabled", format!("{what} bu serviste kapalı")))
        }
    }

    fn handle(&self, id: u32, op: &str, req: &Value) -> Option<Result<Value, OpError>> {
        let ctx = &self.ctx;
        Some(match op {
            "info" => Ok(json!({
                "device": ctx.device,
                "services": ctx.services(),
                "features": ctx.features(),
                "files_dir": ctx.files.dir().to_string_lossy(),
                "openocd": {"configured": ctx.openocd.configured(), "running": ctx.openocd.running()},
            })),
            "openocd.start" | "openocd.stop" | "openocd.status" | "openocd.log" => {
                self.require(ctx.cfg.openocd.enabled, "OpenOCD yönetimi")
                    .and_then(|()| self.openocd(op, req))
            }
            "file.list" | "file.put" | "file.put_end" | "file.delete" => {
                self.require(ctx.cfg.files.enabled, "Dosya aktarımı")
                    .and_then(|()| self.files(op, req))
            }
            "exec" => self
                .require(ctx.cfg.exec.enabled, "exec")
                .and_then(|()| self.exec(id, req)),
            "shell.open" => self
                .require(ctx.cfg.shell.enabled, "Kabuk")
                .and_then(|()| self.shell_open(req)),
            "shell.close" => self.require(ctx.cfg.shell.enabled, "Kabuk").map(|()| {
                self.shell.close();
                json!({})
            }),
            _ => Err(OpError::new("bad_request", format!("Bilinmeyen işlem: {op}"))),
        })
    }

    fn openocd(&self, op: &str, req: &Value) -> Result<Value, OpError> {
        let ocd = &self.ctx.openocd;
        match op {
            "openocd.start" => {
                log("openocd.start");
                ocd.start().map(|pid| json!({"pid": pid}))
            }
            "openocd.stop" => {
                log("openocd.stop");
                ocd.stop().map(|code| json!({"exit_code": code}))
            }
            "openocd.status" => Ok(ocd.status()),
            _ => {
                let lines = req["lines"].as_u64().unwrap_or(100);
                if !(1..=500).contains(&lines) {
                    return Err(OpError::new("bad_request", "lines 1..500 olmalı"));
                }
                Ok(ocd.log(lines as usize))
            }
        }
    }

    fn files(&self, op: &str, req: &Value) -> Result<Value, OpError> {
        let files = &self.ctx.files;
        let name = req["name"].as_str().unwrap_or("");
        let transfer = req["transfer"].as_u64().and_then(|t| u32::try_from(t).ok());
        match op {
            "file.list" => files.list(),
            "file.put" => {
                let size = req["size"].as_u64().ok_or_else(|| OpError::new("bad_request", "size gerekli"))?;
                let sha = req["sha256"].as_str().unwrap_or("");
                let transfer = transfer.ok_or_else(|| OpError::new("bad_request", "transfer gerekli"))?;
                let out = files.put_begin(name, size, sha, transfer)?;
                log(format!("file.put {name} {size} bayt sha {}", &sha[..12.min(sha.len())]));
                Ok(out)
            }
            "file.put_end" => {
                let transfer = transfer.ok_or_else(|| OpError::new("bad_request", "transfer gerekli"))?;
                files.put_end(transfer)
            }
            _ => {
                log(format!("file.delete {name}"));
                files.delete(name).map(|()| json!({}))
            }
        }
    }

    fn get(&self, id: u32, req: &Value) -> Option<Result<Value, OpError>> {
        if let Err(e) = self.require(self.ctx.cfg.files.enabled, "Dosya aktarımı") {
            return Some(Err(e));
        }
        let name = req["name"].as_str().unwrap_or("");
        let Some(transfer) = req["transfer"].as_u64().and_then(|t| u32::try_from(t).ok()) else {
            return Some(Err(OpError::new("bad_request", "transfer gerekli")));
        };
        let files = &self.ctx.files;
        let (mut file, size, sha) = match files.get_begin(name) {
            Ok(v) => v,
            Err(e) => return Some(Err(e)),
        };
        log(format!("file.get {name} {size} bayt sha {}", &sha[..12]));
        self.respond(id, Ok(json!({"size": size, "sha256": sha})));
        let mut buf = vec![0u8; 4 + 8192];
        buf[..4].copy_from_slice(&transfer.to_be_bytes());
        let failure = loop {
            if !self.link.is_alive() {
                break None;
            }
            match file.read(&mut buf[4..]) {
                Ok(0) => break Some(None),
                Ok(n) => {
                    if self.link.send(wire::FILE, &buf[..4 + n]).is_err() {
                        break None;
                    }
                }
                Err(e) => break Some(Some(e.to_string())),
            }
        };
        files.get_end();
        let event = match failure {
            Some(None) => json!({"event": "file.end", "transfer": transfer}),
            Some(Some(error)) => json!({"event": "file.abort", "transfer": transfer, "error": error}),
            None => return None,
        };
        let _ = self.link.send_json(&event);
        None
    }

    fn exec(&self, id: u32, req: &Value) -> Result<Value, OpError> {
        let bad = |m: &str| OpError::new("bad_request", m);
        let argv: Vec<String> = req["argv"]
            .as_array()
            .ok_or_else(|| bad("argv gerekli"))?
            .iter()
            .map(|v| v.as_str().map(str::to_string).ok_or_else(|| bad("argv metin olmalı")))
            .collect::<Result<_, _>>()?;
        if !(1..=64).contains(&argv.len()) || argv[0].is_empty() {
            return Err(bad("argv 1..64 öğe olmalı"));
        }
        let max = self.ctx.cfg.exec.timeout_max_s;
        let timeout = req["timeout_s"].as_u64().unwrap_or(60);
        if !(1..=max).contains(&timeout) {
            return Err(bad("timeout_s sınır dışı"));
        }
        if self.ctx.execs.fetch_add(1, Ordering::AcqRel) >= MAX_EXECS {
            self.ctx.execs.fetch_sub(1, Ordering::AcqRel);
            return Err(OpError::new("busy", "Çok fazla eşzamanlı exec"));
        }
        log(format!("exec {} ({} öğe)", argv[0], argv.len()));
        let result = exec::run(
            &self.link,
            id,
            self.ctx.files.dir(),
            &argv,
            req["cwd"].as_str(),
            Duration::from_secs(timeout),
        );
        self.ctx.execs.fetch_sub(1, Ordering::AcqRel);
        result
    }

    fn shell_open(&self, req: &Value) -> Result<Value, OpError> {
        let dim = |key: &str| {
            req[key]
                .as_u64()
                .filter(|v| (1..=1000).contains(v))
                .map(|v| v as u16)
                .ok_or_else(|| OpError::new("bad_request", format!("{key} 1..1000 olmalı")))
        };
        let term = req["term"].as_str().unwrap_or("xterm-256color");
        if term.len() > 32 || !term.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(OpError::new("bad_request", "term geçersiz"));
        }
        log("shell.open");
        self.shell
            .open(&self.ctx.cfg.shell, &self.link, dim("cols")?, dim("rows")?, term)?;
        Ok(json!({}))
    }
}
