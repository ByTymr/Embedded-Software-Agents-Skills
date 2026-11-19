//! `openocd-link serve`: kart takılı cihazda çalışan servis.

pub mod exec;
pub mod files;
pub mod openocd;
pub mod session;
pub mod shell;

use crate::auth::{self, AuthError, Limiter};
use crate::config::Config;
use crate::key::Key;
use crate::meet::{self, Meet};
use crate::util::clean_name;
use crate::wire;
use files::Files;
use openocd::OpenOcd;
use openocd_link_core::log::log;
use openocd_link_core::net;
use openocd_link_core::punch::{Bridged, Node};
use openocd_link_core::rendezvous::Role;
use serde_json::{Value, json};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

#[derive(Debug)]
pub struct OpError {
    pub code: &'static str,
    pub msg: String,
}

impl OpError {
    pub fn new(code: &'static str, msg: impl Into<String>) -> Self {
        Self {
            code,
            msg: msg.into(),
        }
    }
}

pub struct Ctx {
    pub cfg: Config,
    pub key: Key,
    pub device: String,
    pub files: Files,
    pub openocd: Arc<OpenOcd>,
    pub limiter: Arc<Limiter>,
    pub busy: AtomicBool,
    pub execs: AtomicUsize,
    pub meet: Mutex<Option<Meet>>,
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let name = ok
        .then(|| buf.split(|&b| b == 0).next().map(|b| String::from_utf8_lossy(b).into_owned()))
        .flatten()
        .unwrap_or_default();
    if name.is_empty() { "openocd-link".into() } else { name }
}

impl Ctx {
    pub fn new(cfg: Config, key: Key) -> io::Result<Arc<Self>> {
        let device = if cfg.device_name.is_empty() {
            hostname()
        } else {
            cfg.device_name.clone()
        };
        let files = Files::new(cfg.files.clone())?;
        let openocd = OpenOcd::new(cfg.openocd.clone());
        Ok(Arc::new(Self {
            cfg,
            key,
            device,
            files,
            openocd,
            limiter: Arc::default(),
            busy: AtomicBool::new(false),
            execs: AtomicUsize::new(0),
            meet: Mutex::new(None),
        }))
    }

    pub fn services(&self) -> Value {
        self.cfg
            .forward
            .iter()
            .map(|f| json!({"name": f.name, "port": f.port}))
            .collect()
    }

    pub fn features(&self) -> Value {
        json!({
            "files": self.cfg.files.enabled,
            "openocd": self.cfg.openocd.enabled,
            "exec": self.cfg.exec.enabled,
            "shell": self.cfg.shell.enabled,
        })
    }

    fn hello(&self) -> Value {
        json!({
            "proto": wire::PROTO,
            "status": "ok",
            "device": self.device,
            "services": self.services(),
            "features": self.features(),
            "files_dir": self.files.dir().to_string_lossy(),
        })
    }
}

/// Bu süre boyunca DHT'ye kayıt yazılamaz ve açık oturum da yoksa servis kendini yeniden başlattırır
const STALL_RESTART: Duration = Duration::from_secs(15 * 60);

/// `stop` true olana kadar bağlantı kabul eder. Oturum bitince asla kapanmaz.
/// `false` dönerse servis erişilemez kaldığı için çıkmıştır; systemd yeniden başlatır.
pub fn serve(ctx: Arc<Ctx>, node: Arc<Node>, dht: bool, stop: Arc<AtomicBool>) -> bool {
    for warning in ctx.cfg.warnings() {
        log(warning);
    }
    log(format!("Anahtar parmak izi: {}", ctx.key.fingerprint()));
    let (tx, rx) = sync_channel(4);
    node.accept(Some(tx));
    // Servisin başlattığı bağlantılar da aynı kabul yolundan geçer
    let (rtx, rrx) = sync_channel::<io::Result<Bridged>>(4);
    let rctx = ctx.clone();
    thread::spawn(move || {
        for bridged in rrx.into_iter().flatten() {
            let ctx = rctx.clone();
            thread::spawn(move || accept(&ctx, bridged));
        }
    });
    if dht {
        let poll = Duration::from_secs(ctx.cfg.poll_secs);
        let meet = meet::start(node.clone(), &ctx.key, Role::Host, poll, None, stop.clone());
        *ctx.meet.lock().unwrap() = Some(meet);
    }
    // Sabit istemci adresleri: oturum yokken sürekli bağlanmayı dene
    let fixed: Vec<std::net::SocketAddr> =
        ctx.cfg.connect_to.iter().filter_map(|a| a.parse().ok()).collect();
    if !fixed.is_empty() {
        log(format!("Sabit adrese bağlanılacak: {}", openocd_link_core::addr::candidates_text(&fixed)));
        let (ctx, node, stop, rtx) = (ctx.clone(), node.clone(), stop.clone(), rtx.clone());
        thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                if ctx.busy.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_secs(1));
                    continue;
                }
                let cancel = Arc::new(AtomicBool::new(false));
                node.dial_back(fixed.clone(), rtx.clone(), cancel.clone());
                // Oturum açılana ya da 60 sn dolana kadar bekle, sonra yenile
                for _ in 0..60 {
                    if stop.load(Ordering::Acquire) || ctx.busy.load(Ordering::Acquire) {
                        break;
                    }
                    thread::sleep(Duration::from_secs(1));
                }
                cancel.store(true, Ordering::Release);
            }
        });
    }
    if ctx.cfg.openocd.enabled && ctx.cfg.openocd.autostart {
        match ctx.openocd.start() {
            Ok(pid) => log(format!("OpenOCD başlatıldı (pid {pid})")),
            Err(e) => log(format!("OpenOCD başlatılamadı: {}", e.msg)),
        }
    }
    let mut healthy = true;
    while !stop.load(Ordering::Acquire) {
        let stalled = ctx.meet.lock().unwrap().as_ref().is_some_and(|m| m.since_publish() >= STALL_RESTART);
        if stalled && ctx.cfg.connect_to.is_empty() && !ctx.busy.load(Ordering::Acquire) {
            log("15 dakikadır DHT'ye kayıt yazılamadı; servis yeniden başlatılıyor");
            healthy = false;
            break;
        }
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(bridged) => {
                let ctx = ctx.clone();
                thread::spawn(move || accept(&ctx, bridged));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    node.accept(None);
    ctx.openocd.shutdown();
    healthy
}

fn reject(secure: &mut net::Secure, status: &str) {
    let _ = wire::write_hello(secure, &json!({"proto": wire::PROTO, "status": status}));
}

fn accept(ctx: &Arc<Ctx>, (socket, addr): Bridged) {
    let ip = addr.ip().to_canonical();
    let Some(permit) = ctx.limiter.admit(ip) else {
        return;
    };
    let mut secure = match auth::authenticate(socket, false, &ctx.key.pake_secret()) {
        Ok(secure) => {
            permit.finish(true);
            secure
        }
        Err(AuthError::NotOurs) => {
            permit.ignore();
            return;
        }
        Err(AuthError::Failed(e)) => {
            if permit.finish(false) {
                log("Çok fazla başarısız deneme: yeni bağlantılar 60 sn durduruldu");
            }
            log(format!("Kimlik doğrulama başarısız ({ip}): {e}"));
            return;
        }
    };
    let Ok(hello) = wire::read_hello(&mut secure) else {
        return;
    };
    if hello["proto"] != wire::PROTO {
        reject(&mut secure, "unsupported");
        return;
    }
    let name = clean_name(hello["name"].as_str().unwrap_or(""));
    if ctx.busy.swap(true, Ordering::AcqRel) {
        reject(&mut secure, "busy");
        return;
    }
    if wire::write_hello(&mut secure, &ctx.hello()).is_ok() {
        if let Some(meet) = ctx.meet.lock().unwrap().as_ref() {
            meet.settle();
        }
        let sas = net::sas(&secure.hash);
        session::run(ctx, secure, &name, ip, &sas);
    }
    ctx.busy.store(false, Ordering::Release);
    if let Some(meet) = ctx.meet.lock().unwrap().as_ref() {
        meet.reset();
    }
}
