//! `openocd-link connect`: oturum daemon'u ve yerel komutlar.

pub mod control;
pub mod session;
pub mod state;
pub mod terminal;

use crate::auth::{self, AuthError};
use crate::error::CliError;
use crate::key::Key;
use crate::link::{Link, read_loop};
use crate::meet::{self, ClientDial};
use crate::util::{clean_name, hex, now_rfc3339};
use crate::wire;
use control::Daemon;
use openocd_link_core::net;
use openocd_link_core::punch::{self, Bridged, Node};
use openocd_link_core::rendezvous::Role;
use openocd_link_core::tunnel::Tunnel;
use serde_json::{Value, json};
use session::Remote;
use state::State;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, channel, sync_channel};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct ConnectParams {
    pub key: Key,
    pub session: String,
    pub state_dir: PathBuf,
    pub maps: Vec<(String, u16)>,
    pub timeout: Duration,
    pub peers: Vec<SocketAddr>,
    pub dht: bool,
    pub name: String,
    /// Testlerde hazır düğüm verilir
    pub node: Option<Arc<Node>>,
    /// Sabit yerel UDP portu (0: rastgele)
    pub port: u16,
    /// Dışarıdan durdurma (testler)
    pub stop: Arc<AtomicBool>,
}

fn emit(event: &Value, f: &mut dyn FnMut(&Value)) {
    f(event);
}

/// "ad=port" değerlerini ayrıştırır; yinelenen ad ve yerel port bağlanmadan önce reddedilir
pub fn parse_maps(items: &[String]) -> Result<Vec<(String, u16)>, CliError> {
    let mut maps: Vec<(String, u16)> = Vec::new();
    for item in items {
        let usage = || CliError::new(2, format!("--map ad=port biçiminde olmalı: {item}"));
        let (name, port) = item.split_once('=').ok_or_else(usage)?;
        let port: u16 = port.parse().ok().filter(|p| *p != 0).ok_or_else(usage)?;
        if maps.iter().any(|(n, _)| n == name) {
            return Err(CliError::new(2, format!("--map içinde {name} birden fazla")));
        }
        if maps.iter().any(|(_, p)| *p == port) {
            return Err(CliError::new(2, format!("Yerel port {port} birden fazla kullanılıyor")));
        }
        maps.push((name.to_string(), port));
    }
    Ok(maps)
}

fn wait_peer(
    rx: &std::sync::mpsc::Receiver<io::Result<Bridged>>,
    timeout: Duration,
    stop: &AtomicBool,
) -> Result<Bridged, CliError> {
    let deadline = Instant::now() + timeout;
    loop {
        if stop.load(Ordering::Acquire) {
            return Err(CliError::new(0, "Durduruldu"));
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(CliError::new(4, "Servis bulunamadı (zaman aşımı)"));
        }
        match rx.recv_timeout(left.min(Duration::from_millis(200))) {
            Ok(Ok(bridged)) => return Ok(bridged),
            Ok(Err(_)) | Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return Err(CliError::new(4, "Servis bulunamadı"));
            }
        }
    }
}

fn setup_tunnels(
    link: &Arc<Link>,
    services: &[Value],
    maps: &[(String, u16)],
) -> Result<(Vec<Arc<Tunnel>>, Vec<Value>), CliError> {
    for (name, _) in maps {
        if !services.iter().any(|s| s["name"] == name.as_str()) {
            return Err(CliError::new(2, format!("Serviste {name} adlı yönlendirme yok")));
        }
    }
    let mut plan = Vec::new();
    for s in services {
        let name = s["name"].as_str().unwrap_or("");
        let remote = s["port"].as_u64().unwrap_or(0) as u16;
        let local = maps.iter().find(|(n, _)| n == name).map_or(remote, |(_, p)| *p);
        if plan.iter().any(|(_, _, l)| *l == local) {
            return Err(CliError::new(2, format!("Yerel port {local} birden fazla kullanılıyor")));
        }
        plan.push((name.to_string(), remote, local));
    }
    let mut tunnels = Vec::new();
    let mut summary = Vec::new();
    for (i, (name, remote, local)) in plan.into_iter().enumerate() {
        let tunnel = Tunnel::new();
        let (alive, out) = (link.clone(), link.clone());
        let channel = wire::tunnel_channel(i);
        tunnel.run(false, move || alive.is_alive(), move |packet| out.send(channel, packet));
        // Sıra önemli: yerel bağlantı kabul edilince remote_port okunur
        tunnel.remote_port.store(remote, Ordering::Release);
        tunnel.local_port.store(local, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if tunnel.local_port.load(Ordering::Acquire) == 0 {
                let status = tunnel.status.lock().unwrap().clone();
                return Err(CliError::new(8, status));
            }
            if tunnel.status.lock().unwrap().contains("dinleniyor") || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        summary.push(json!({"name": name, "remote_port": remote, "local_port": local}));
        tunnels.push(tunnel);
    }
    Ok((tunnels, summary))
}

/// Oturum bitene kadar çalışır. `on_event`: searching / ready / closed olayları.
pub fn connect(p: ConnectParams, on_event: &mut dyn FnMut(&Value)) -> Result<(), CliError> {
    if let Ok(existing) = State::read(&p.state_dir, &p.session) {
        let addr = SocketAddr::from(([127, 0, 0, 1], existing.control_port));
        if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok() {
            return Err(CliError::new(2, format!("{} oturumu zaten bağlı", p.session)));
        }
    }
    emit(&json!({"event": "searching", "fingerprint": p.key.fingerprint()}), on_event);
    let node = p
        .node
        .clone()
        .unwrap_or_else(|| punch::start(p.port, (!p.dht).then(Vec::new)));
    if p.peers.is_empty() && p.dht {
        let until = Instant::now() + Duration::from_secs(10);
        while node.candidates().is_none() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let (tx, rx) = sync_channel(1);
    let cancel = Arc::new(AtomicBool::new(false));
    let meet_stop = Arc::new(AtomicBool::new(false));
    // Servis simetrik NAT arkasındaysa bağlantıyı o açar; ilk kurulan yol kazanır
    node.accept_back(Some((tx.clone(), cancel.clone())));
    if !p.peers.is_empty() {
        node.dial(p.peers.clone(), tx.clone(), cancel.clone());
    }
    if p.dht {
        let dial = ClientDial { tx: tx.clone(), cancel: cancel.clone() };
        meet::start(node.clone(), &p.key, Role::Client, Duration::from_secs(3), Some(dial), meet_stop.clone());
    }
    let found = wait_peer(&rx, p.timeout, &p.stop);
    cancel.store(true, Ordering::Release);
    meet_stop.store(true, Ordering::Release);
    node.accept_back(None);
    let (socket, peer) = found?;

    let mut secure = match auth::authenticate(socket, true, &p.key.pake_secret()) {
        Ok(secure) => secure,
        Err(AuthError::NotOurs) => return Err(CliError::new(5, "Kimlik doğrulama başarısız")),
        Err(AuthError::Failed(e)) => {
            let msg = match e.kind() {
                io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset => {
                    "Servis bağlantıyı kabul etmedi: anahtar yanlış ya da sürümler uyumsuz".to_string()
                }
                _ => format!("Kimlik doğrulama başarısız: {e}"),
            };
            return Err(CliError::new(5, msg));
        }
    };
    let sas = net::sas(&secure.hash);
    let io_err = |e: io::Error| CliError::new(5, format!("Servis bağlantıyı kabul etmedi: {e}"));
    wire::write_hello(&mut secure, &json!({"proto": wire::PROTO, "name": clean_name(&p.name)})).map_err(io_err)?;
    let hello = wire::read_hello(&mut secure).map_err(io_err)?;
    match hello["status"].as_str() {
        Some("ok") => {}
        Some("busy") => return Err(CliError::new(6, "Servis meşgul: başka bir oturum açık")),
        _ => return Err(CliError::new(7, "Protokol sürümü uyumsuz")),
    }
    let services = hello["services"].as_array().cloned().unwrap_or_default();
    if services.len() > wire::MAX_SERVICES {
        return Err(CliError::new(7, "Servis çok fazla yönlendirme bildirdi"));
    }

    let (link, reader) = Link::new(secure).map_err(CliError::from)?;
    let remote = Remote::new(link.clone(), hello.clone());
    let (tunnels, summary) = match setup_tunnels(&link, &services, &p.maps) {
        Ok(v) => v,
        Err(e) => {
            link.close();
            return Err(e);
        }
    };
    let (reason_tx, reason_rx) = channel();
    {
        let (link, remote, tunnels) = (link.clone(), remote.clone(), tunnels.clone());
        std::thread::spawn(move || {
            let reason = read_loop(&link, reader, |c, payload| remote.frame(&tunnels, c, payload));
            let _ = reason_tx.send(reason);
        });
    }

    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(CliError::from)?;
    let mut token = [0u8; 32];
    getrandom::fill(&mut token).map_err(|e| CliError::new(1, e.to_string()))?;
    let state = State {
        pid: std::process::id(),
        control_port: listener.local_addr().map_err(CliError::from)?.port(),
        token: hex(&token),
        device: hello["device"].as_str().unwrap_or("").to_string(),
        peer: openocd_link_core::addr::show_addr(peer),
        via: "quic".into(),
        sas: sas.clone(),
        fingerprint: p.key.fingerprint(),
        started_at: now_rfc3339(),
        services: Value::Array(summary.clone()),
        features: hello["features"].clone(),
        files_dir: hello["files_dir"].as_str().unwrap_or("").to_string(),
    };
    state.write(&p.state_dir, &p.session).map_err(CliError::from)?;
    let stop = p.stop.clone();
    let daemon = Arc::new(Daemon {
        remote: remote.clone(),
        token: state.token.clone(),
        stop: stop.clone(),
        state: State { token: String::new(), ..state.clone() },
    });
    control::listen(daemon, listener);
    emit(
        &json!({
            "event": "ready", "device": state.device, "peer": state.peer, "via": "quic",
            "sas": sas, "services": summary, "features": state.features, "files_dir": state.files_dir,
        }),
        on_event,
    );

    let outcome = loop {
        if stop.load(Ordering::Acquire) {
            link.close();
            for t in &tunnels {
                t.close();
            }
            break Ok(());
        }
        match reason_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(reason) => {
                for t in &tunnels {
                    t.close();
                }
                break if stop.load(Ordering::Acquire) { Ok(()) } else { Err(CliError::new(9, reason)) };
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break Err(CliError::new(9, "Bağlantı koptu")),
        }
    };
    State::remove(&p.state_dir, &p.session);
    let (code, reason) = match &outcome {
        Ok(()) => (0, "Kapatıldı".to_string()),
        Err(e) => (e.code, e.msg.clone()),
    };
    emit(&json!({"event": "closed", "code": code, "reason": reason}), on_event);
    outcome
}

/// `disconnect`: kapatma ister; 5 sn içinde bitmezse SIGTERM
pub fn disconnect(dir: &std::path::Path, session: &str) -> Result<Value, CliError> {
    let state = State::read(dir, session)?;
    control::call(dir, session, json!({"op": "shutdown"}), |_| {})?;
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        if !state::path(dir, session).exists() {
            return Ok(json!({"disconnected": true}));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    #[cfg(unix)]
    unsafe {
        libc::kill(state.pid as i32, libc::SIGTERM);
    }
    State::remove(dir, session);
    Ok(json!({"disconnected": true, "forced": true}))
}
