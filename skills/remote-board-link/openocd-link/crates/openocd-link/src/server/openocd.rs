//! OpenOCD sürecini yönetir. Kendiliğinden yeniden başlatılmaz.

use super::OpError;
use crate::config;
use crate::link::Link;
use crate::util::now_rfc3339;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const LOG_LINES: usize = 2000;
const LOG_BYTES: usize = 50_000;

#[derive(Default)]
struct State {
    child: Option<Child>,
    pid: Option<u32>,
    running: bool,
    exit_code: Option<i32>,
    since: Option<String>,
    stopping: bool,
}

pub struct OpenOcd {
    cfg: config::OpenOcd,
    state: Arc<Mutex<State>>,
    log: Arc<Mutex<VecDeque<String>>>,
    notify: Arc<Mutex<Option<Arc<Link>>>>,
}

fn port_open(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

fn pump(stream: impl Read + Send + 'static, log: Arc<Mutex<VecDeque<String>>>) {
    thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            let mut log = log.lock().unwrap();
            if log.len() == LOG_LINES {
                log.pop_front();
            }
            log.push_back(line);
        }
    });
}

impl OpenOcd {
    pub fn new(cfg: config::OpenOcd) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            state: Arc::default(),
            log: Arc::default(),
            notify: Arc::default(),
        })
    }

    pub fn configured(&self) -> bool {
        self.cfg.enabled
    }

    pub fn running(&self) -> bool {
        self.state.lock().unwrap().running
    }

    pub fn set_notify(&self, link: Option<Arc<Link>>) {
        *self.notify.lock().unwrap() = link;
    }

    /// Servisin `-c` öğeleri `init`'ten önce uygulanır, bu yüzden başa gelir
    pub fn argv(&self) -> Vec<String> {
        let mut args = vec![
            "-c".into(),
            "bindto 127.0.0.1".into(),
            "-c".into(),
            format!("tcl_port {}", self.cfg.tcl_port),
            "-c".into(),
            format!("gdb_port {}", self.cfg.gdb_port),
            "-c".into(),
            "telnet_port disabled".into(),
        ];
        args.extend(self.cfg.args.iter().cloned());
        args
    }

    fn last_lines(&self, n: usize) -> String {
        let log = self.log.lock().unwrap();
        let skip = log.len().saturating_sub(n);
        log.iter().skip(skip).cloned().collect::<Vec<_>>().join("\n")
    }

    pub fn start(&self) -> Result<u32, OpError> {
        if self.running() {
            return Err(OpError::new("already_running", "OpenOCD zaten çalışıyor"));
        }
        if port_open(self.cfg.tcl_port) {
            return Err(OpError::new(
                "already_running",
                "Port başka bir OpenOCD tarafından kullanılıyor",
            ));
        }
        let mut child = Command::new(&self.cfg.executable)
            .args(self.argv())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| OpError::new("start_failed", format!("OpenOCD başlatılamadı: {e}")))?;
        self.log.lock().unwrap().clear();
        if let Some(out) = child.stdout.take() {
            pump(out, self.log.clone());
        }
        if let Some(err) = child.stderr.take() {
            pump(err, self.log.clone());
        }
        let pid = child.id();
        *self.state.lock().unwrap() = State {
            child: Some(child),
            pid: Some(pid),
            running: true,
            exit_code: None,
            since: Some(now_rfc3339()),
            stopping: false,
        };
        self.monitor();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if port_open(self.cfg.tcl_port) {
                return Ok(pid);
            }
            if !self.running() {
                break;
            }
            if Instant::now() >= deadline {
                self.stop_quiet();
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        Err(OpError::new(
            "start_failed",
            format!(
                "OpenOCD Tcl portu 10 sn içinde açılmadı. Son satırlar:\n{}",
                self.last_lines(20)
            ),
        ))
    }

    /// Tek biçerdöver: süreç kendiliğinden biterse `openocd.exit` olayı gider
    fn monitor(&self) {
        let (state, notify) = (self.state.clone(), self.notify.clone());
        thread::spawn(move || {
            loop {
                let mut s = state.lock().unwrap();
                let Some(child) = s.child.as_mut() else { return };
                if let Ok(Some(status)) = child.try_wait() {
                    s.child = None;
                    s.running = false;
                    s.exit_code = status.code();
                    let (code, quiet) = (s.exit_code, s.stopping);
                    drop(s);
                    if !quiet && let Some(link) = notify.lock().unwrap().as_ref() {
                        let _ = link.send_json(&json!({"event": "openocd.exit", "code": code}));
                    }
                    return;
                }
                drop(s);
                thread::sleep(Duration::from_millis(100));
            }
        });
    }

    fn stop_quiet(&self) -> Option<i32> {
        let pid = {
            let mut s = self.state.lock().unwrap();
            if !s.running {
                return s.exit_code;
            }
            s.stopping = true;
            s.pid
        };
        if let Some(pid) = pid {
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !self.running() {
                return self.state.lock().unwrap().exit_code;
            }
            thread::sleep(Duration::from_millis(50));
        }
        if let Some(child) = self.state.lock().unwrap().child.as_mut() {
            let _ = child.kill();
        }
        while self.running() {
            thread::sleep(Duration::from_millis(20));
        }
        self.state.lock().unwrap().exit_code
    }

    pub fn stop(&self) -> Result<Option<i32>, OpError> {
        if !self.running() {
            return Err(OpError::new("not_running", "OpenOCD çalışmıyor"));
        }
        Ok(self.stop_quiet())
    }

    pub fn shutdown(&self) {
        self.stop_quiet();
    }

    pub fn status(&self) -> Value {
        let s = self.state.lock().unwrap();
        json!({"running": s.running, "pid": s.pid, "exit_code": s.exit_code, "since": s.since})
    }

    pub fn log(&self, lines: usize) -> Value {
        let log = self.log.lock().unwrap();
        let mut picked: Vec<&String> = Vec::new();
        let mut bytes = 0;
        for line in log.iter().rev().take(lines) {
            if bytes + line.len() + 1 > LOG_BYTES {
                break;
            }
            bytes += line.len() + 1;
            picked.push(line);
        }
        picked.reverse();
        json!({"lines": picked, "truncated": picked.len() < lines.min(log.len())})
    }
}
