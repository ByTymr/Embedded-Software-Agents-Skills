//! Aynı makinede servis + istemci: internet ve DHT kapalı, QUIC loopback üzerinden.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use openocd_link::client::{self, ConnectParams, control};
use openocd_link::config::Config;
use openocd_link::error::CliError;
use openocd_link::key::Key;
use openocd_link::server::{self, Ctx};
use openocd_link_core::punch;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ol-{name}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(dir).unwrap()
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Gelen baytları yankılar; yarı kapatmada yazma yönünü kapatır
fn echo_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 || stream.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                let _ = stream.shutdown(Shutdown::Write);
            });
        }
    });
    port
}

struct Server {
    key: Key,
    stop: Arc<AtomicBool>,
    target: SocketAddr,
    ctx: Arc<Ctx>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

fn start_server(extra: &str) -> Server {
    start_server_with("", extra)
}

/// `top`: [[forward]] öncesine giden üst düzey alanlar; `extra`: sonuna eklenen bölümler
fn start_server_with(top: &str, extra: &str) -> Server {
    let dir = temp("srv");
    let echo = echo_server();
    let key = Key::generate();
    let text = format!(
        "key_file = \"/yok\"\n{extra_top}\n[[forward]]\nname = \"tcl\"\nport = {echo}\n\
         [files]\ndir = \"{files}\"\n{extra}",
        extra_top = top,
        files = dir.join("files").display(),
    );
    let ctx = Ctx::new(Config::parse(&text).unwrap(), key.clone()).unwrap();
    let loopback = IpAddr::from([127, 0, 0, 1]);
    let node = punch::start(0, Some(vec![loopback]));
    let until = Instant::now() + Duration::from_secs(10);
    let target = loop {
        if let Some(list) = node.candidates() {
            break list.into_iter().find(|a| a.ip() == loopback).unwrap();
        }
        assert!(Instant::now() < until, "aday listesi gelmedi");
        thread::sleep(Duration::from_millis(50));
    };
    let stop = Arc::new(AtomicBool::new(false));
    let (c, s) = (ctx.clone(), stop.clone());
    thread::spawn(move || server::serve(c, node, false, s));
    Server { key, stop, target, ctx }
}

struct Client {
    state: PathBuf,
    session: String,
    local: u16,
    stop: Arc<AtomicBool>,
    events: Receiver<Value>,
    handle: Option<JoinHandle<Result<(), CliError>>>,
}

impl Client {
    fn call(&self, req: Value) -> Result<Value, CliError> {
        control::call(&self.state, &self.session, req, |_| {})
    }

    fn ready(&self) -> Value {
        loop {
            let event = self.events.recv_timeout(Duration::from_secs(30)).expect("olay gelmedi");
            if event["event"] == "ready" {
                return event;
            }
        }
    }

    /// Bağlantının kendiliğinden bitmesini bekler (hata beklenen durumlar)
    fn wait(mut self) -> Result<(), CliError> {
        self.handle.take().unwrap().join().unwrap()
    }

    fn finish(mut self) -> Result<(), CliError> {
        self.stop.store(true, Ordering::Release);
        self.handle.take().unwrap().join().unwrap()
    }
}

fn spawn_client(target: SocketAddr, key: Key, session: &str, local: u16) -> Client {
    let mut client = spawn_client_with(target, key, session, vec![("tcl".into(), local)]);
    client.local = local;
    client
}

fn spawn_client_with(target: SocketAddr, key: Key, session: &str, maps: Vec<(String, u16)>) -> Client {
    let state = temp("cli");
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, events) = channel();
    let params = ConnectParams {
        key,
        session: session.into(),
        state_dir: state.clone(),
        maps,
        timeout: Duration::from_secs(20),
        peers: vec![target],
        dht: false,
        name: "test istemcisi".into(),
        node: Some(punch::start(0, Some(vec![]))),
        port: 0,
        stop: stop.clone(),
    };
    let handle = thread::spawn(move || client::connect(params, &mut |e| {
        let _ = tx.send(e.clone());
    }));
    Client { state, session: session.into(), local: 0, stop, events, handle: Some(handle) }
}

fn connected(server: &Server) -> Client {
    let client = spawn_client(server.target, server.key.clone(), "lab", free_port());
    client.ready();
    client
}

fn tunnel_round_trip(port: u16, data: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    stream.write_all(data).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let mut out = Vec::new();
    stream.read_to_end(&mut out).unwrap();
    out
}

fn random(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn sha(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    openocd_link::util::hex(&Sha256::digest(data))
}

#[test]
fn tunnel_carries_bytes_both_ways_and_keeps_half_close() {
    let server = start_server("");
    let client = connected(&server);
    let payload = random(100_000, 7);
    // Yarı kapatma: istemci yazmayı kapatır, yankı verisi yine de tamamen gelir
    assert_eq!(tunnel_round_trip(client.local, &payload), payload);
    assert_eq!(tunnel_round_trip(client.local, b""), b"");
    client.finish().unwrap();
}

#[test]
fn status_info_and_state_file() {
    let server = start_server("");
    let client = connected(&server);
    let status = client.call(json!({"op": "status"})).unwrap();
    assert_eq!(status["connected"], true);
    assert_eq!(status["services"][0]["name"], "tcl");
    let path = client.state.join("lab.json");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("OL1-") && !text.contains(&server.key.text()));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
    assert_eq!(client.call(json!({"op": "info"})).unwrap()["features"]["files"], true);
    let state = client.state.clone();
    client.call(json!({"op": "shutdown"})).unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while path.exists() && Instant::now() < until {
        thread::sleep(Duration::from_millis(50));
    }
    assert!(!path.exists(), "durum dosyası silinmedi");
    assert_eq!(client::control::call(&state, "lab", json!({"op": "status"}), |_| {}).unwrap_err().code, 10);
    client.finish().unwrap();
}

#[test]
fn wrong_token_closes_silently() {
    let server = start_server("");
    let client = connected(&server);
    let port = state_port(&client);
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    stream.write_all(b"{\"token\":\"yanlis\",\"op\":\"status\"}\n").unwrap();
    let mut out = Vec::new();
    let _ = stream.read_to_end(&mut out);
    assert!(out.is_empty(), "yanlış jetonla yanıt geldi: {out:?}");
    client.finish().unwrap();
}

fn state_port(client: &Client) -> u16 {
    let text = std::fs::read_to_string(client.state.join("lab.json")).unwrap();
    serde_json::from_str::<Value>(&text).unwrap()["control_port"].as_u64().unwrap() as u16
}

#[test]
fn wrong_key_exits_with_code_5() {
    let server = start_server("");
    let client = spawn_client(server.target, Key::generate(), "lab", free_port());
    let err = client.wait().unwrap_err();
    assert_eq!(err.code, 5, "{err}");
    assert!(err.msg.contains("anahtar yanlış"));
}

#[test]
fn second_client_gets_busy() {
    let server = start_server("");
    let first = connected(&server);
    let second = spawn_client(server.target, server.key.clone(), "ikinci", free_port());
    assert_eq!(second.wait().unwrap_err().code, 6);
    first.finish().unwrap();
    // Oturum bitince yeni istemci kabul edilir. Servis kopan oturumu en geç okuma zaman aşımında
    // (15 sn) fark eder; o zamana kadar busy dönebilir.
    let until = Instant::now() + Duration::from_secs(25);
    loop {
        let again = spawn_client(server.target, server.key.clone(), "ucuncu", free_port());
        let ready = loop {
            match again.events.recv_timeout(Duration::from_secs(20)) {
                Ok(e) if e["event"] == "ready" => break true,
                Ok(e) if e["event"] == "closed" => break false,
                Ok(_) => {}
                Err(_) => break false,
            }
        };
        if ready {
            again.finish().unwrap();
            return;
        }
        assert!(Instant::now() < until, "yeni oturum kurulamadı");
        thread::sleep(Duration::from_millis(300));
    }
}

#[test]
fn map_arguments_are_validated_before_connecting() {
    use openocd_link::client::parse_maps;
    let maps = |v: &[&str]| parse_maps(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!(maps(&["tcl=1", "tcl=2"]).unwrap_err().code, 2);
    assert_eq!(maps(&["tcl=1", "gdb=1"]).unwrap_err().code, 2);
    assert_eq!(maps(&["tcl"]).unwrap_err().code, 2);
    assert_eq!(maps(&["tcl=0"]).unwrap_err().code, 2);
    assert_eq!(maps(&["tcl=16666"]).unwrap(), vec![("tcl".to_string(), 16666)]);
}

#[test]
fn unknown_service_name_is_a_usage_error() {
    let server = start_server("");
    let client = spawn_client_with(server.target, server.key.clone(), "lab", vec![("yok".into(), free_port())]);
    assert_eq!(client.wait().unwrap_err().code, 2);
}

// ───────────── dosya aktarımı ─────────────

fn put(client: &Client, path: &Path, name: &str) -> Result<Value, CliError> {
    client.call(json!({"op": "put", "path": path.to_string_lossy(), "name": name}))
}

fn get(client: &Client, name: &str, out: &Path, force: bool) -> Result<Value, CliError> {
    client.call(json!({"op": "get", "name": name, "out": out.to_string_lossy(), "force": force}))
}

#[test]
fn files_round_trip_with_equal_sha256() {
    let server = start_server("");
    let client = connected(&server);
    let work = temp("work");
    for (i, len) in [0usize, 1, 8192, 8193, 5 * 1024 * 1024].into_iter().enumerate() {
        let data = random(len, i as u64 + 3);
        let local = work.join(format!("in{i}"));
        std::fs::write(&local, &data).unwrap();
        let name = format!("f{i}.bin");
        let up = put(&client, &local, &name).unwrap();
        assert_eq!(up["sha256"], sha(&data), "{len}");
        let stored = server.ctx.files.dir().join(&name);
        assert_eq!(std::fs::read(&stored).unwrap(), data);
        let back = work.join(format!("out{i}"));
        let down = get(&client, &name, &back, false).unwrap();
        assert_eq!(down["sha256"], sha(&data));
        assert_eq!(std::fs::read(&back).unwrap(), data);
    }
    let listing = client.call(json!({"op": "file.list"})).unwrap();
    assert_eq!(listing["files"].as_array().unwrap().len(), 5);
    assert!(std::fs::read_dir(server.ctx.files.dir()).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".part")));
    // Var olan hedef --force olmadan istek göndermeden reddedilir
    let err = get(&client, "f1.bin", &work.join("out1"), false).unwrap_err();
    assert!(err.msg.contains("zaten var"));
    assert!(get(&client, "f1.bin", &work.join("out1"), true).is_ok());
    client.call(json!({"op": "file.delete", "name": "f1.bin"})).unwrap();
    assert!(client.call(json!({"op": "file.delete", "name": "f1.bin"})).is_err());
    client.finish().unwrap();
}

#[test]
fn invalid_names_and_limits_are_rejected() {
    let server = start_server("max_file_mb = 1\nmax_total_mb = 2\n");
    let client = connected(&server);
    let work = temp("work");
    let local = work.join("x");
    std::fs::write(&local, b"x").unwrap();
    for bad in ["../x", ".gizli", "a/b", "x.part", &"a".repeat(129)] {
        assert!(put(&client, &local, bad).is_err(), "{bad}");
        assert!(client.call(json!({"op": "file.delete", "name": bad})).is_err(), "{bad}");
    }
    let big = work.join("big");
    std::fs::write(&big, random(1024 * 1024 + 1, 1)).unwrap();
    assert!(put(&client, &big, "big.bin").unwrap_err().msg.contains("max_file_mb"));
    let one = work.join("one");
    std::fs::write(&one, random(1024 * 1024, 2)).unwrap();
    put(&client, &one, "a.bin").unwrap();
    put(&client, &one, "b.bin").unwrap();
    assert!(put(&client, &one, "c.bin").unwrap_err().msg.contains("toplam"));
    client.finish().unwrap();
}

#[test]
fn tunnel_keeps_flowing_during_a_transfer_and_session_loss_leaves_no_part() {
    let server = start_server("");
    let client = connected(&server);
    let work = temp("work");
    let local = work.join("big");
    std::fs::write(&local, random(8 * 1024 * 1024, 5)).unwrap();
    let (state, session, big) = (client.state.clone(), client.session.clone(), local.clone());
    let upload = thread::spawn(move || {
        control::call(&state, &session, json!({"op": "put", "path": big.to_string_lossy(), "name": "big.bin"}), |_| {})
    });
    let mut rounds = 0;
    while !upload.is_finished() {
        assert_eq!(tunnel_round_trip(client.local, b"ping"), b"ping");
        rounds += 1;
    }
    upload.join().unwrap().unwrap();
    assert!(rounds >= 1);
    // Aktarım ortasında oturum kesilir
    let (state, session, big) = (client.state.clone(), client.session.clone(), local);
    let cut = thread::spawn(move || {
        control::call(&state, &session, json!({"op": "put", "path": big.to_string_lossy(), "name": "cut.bin"}), |_| {})
    });
    thread::sleep(Duration::from_millis(30));
    client.stop.store(true, Ordering::Release);
    let _ = cut.join().unwrap();
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let parts = std::fs::read_dir(server.ctx.files.dir()).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().ends_with(".part")).count();
        if parts == 0 {
            break;
        }
        assert!(Instant::now() < until, ".part dosyası kaldı");
        thread::sleep(Duration::from_millis(100));
    }
    let _ = client.finish();
}

// ───────────── OpenOCD, exec, kabuk ─────────────

#[cfg(unix)]
fn fake_openocd(dir: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("openocd");
    std::fs::write(&path, format!("#!/usr/bin/env python3\n{body}")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

const LISTENER: &str = r#"
import socket, sys, time
args = sys.argv[1:]
port = 0
for i, a in enumerate(args):
    if a.startswith("tcl_port "):
        port = int(a.split()[1])
print("args:", " ".join(args), flush=True)
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", port))
s.listen()
print("hazir", flush=True)
while True:
    time.sleep(1)
"#;

#[cfg(unix)]
#[test]
fn openocd_start_status_log_stop() {
    let dir = temp("ocd");
    let exe = fake_openocd(&dir, LISTENER);
    let port = free_port();
    let extra = format!(
        "[openocd]\nenabled = true\nexecutable = \"{}\"\nargs = [\"-f\", \"x.cfg\"]\ntcl_port = {port}\ngdb_port = {}\n",
        exe.display(),
        free_port()
    );
    let server = start_server(&extra);
    let client = connected(&server);
    let started = client.call(json!({"op": "openocd.start"})).unwrap();
    assert!(started["pid"].as_u64().unwrap() > 0);
    assert_eq!(client.call(json!({"op": "openocd.start"})).unwrap_err().msg, "OpenOCD zaten çalışıyor");
    let status = client.call(json!({"op": "openocd.status"})).unwrap();
    assert_eq!(status["running"], true);
    let log = client.call(json!({"op": "openocd.log", "lines": 10})).unwrap();
    let text = log["lines"].as_array().unwrap().iter().map(|l| l.as_str().unwrap()).collect::<Vec<_>>().join("\n");
    // Servisin -c öğeleri önce gelir, yapılandırma args'ı sonra
    let expect = format!("-c bindto 127.0.0.1 -c tcl_port {port} -c gdb_port");
    assert!(text.contains(&expect), "{text}");
    assert!(text.contains("telnet_port disabled -f x.cfg"), "{text}");
    let pid = started["pid"].as_u64().unwrap();
    client.call(json!({"op": "openocd.stop"})).unwrap();
    assert_eq!(client.call(json!({"op": "openocd.status"})).unwrap()["running"], false);
    assert!(!Path::new(&format!("/proc/{pid}")).exists() || std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| s.contains(") Z")));
    assert!(client.call(json!({"op": "openocd.stop"})).is_err());
    client.finish().unwrap();
}

#[cfg(unix)]
#[test]
fn openocd_start_failure_includes_last_lines_and_port_clash_is_detected() {
    let dir = temp("ocd");
    let exe = fake_openocd(&dir, "import sys\nprint('Error: no device found', flush=True)\nsys.exit(1)\n");
    let busy = TcpListener::bind("127.0.0.1:0").unwrap();
    let extra = format!(
        "[openocd]\nenabled = true\nexecutable = \"{}\"\ntcl_port = {}\ngdb_port = {}\n",
        exe.display(),
        free_port(),
        free_port()
    );
    let server = start_server(&extra);
    let client = connected(&server);
    let err = client.call(json!({"op": "openocd.start"})).unwrap_err();
    assert!(err.msg.contains("no device found"), "{}", err.msg);
    client.finish().unwrap();
    // Port başka bir süreçte: already_running
    let extra = format!(
        "[openocd]\nenabled = true\nexecutable = \"{}\"\ntcl_port = {}\ngdb_port = {}\n",
        exe.display(),
        busy.local_addr().unwrap().port(),
        free_port()
    );
    let server = start_server(&extra);
    let client = connected(&server);
    let err = client.call(json!({"op": "openocd.start"})).unwrap_err();
    assert!(err.msg.contains("başka bir OpenOCD"), "{}", err.msg);
    client.finish().unwrap();
}

#[cfg(unix)]
#[test]
fn disabled_features_are_refused() {
    let server = start_server("");
    let client = connected(&server);
    for req in [
        json!({"op": "exec", "argv": ["true"]}),
        json!({"op": "shell", "cols": 80, "rows": 24}),
        json!({"op": "openocd.start"}),
    ] {
        let err = client.call(req).unwrap_err();
        assert!(err.msg.contains("kapalı"), "{}", err.msg);
    }
    client.finish().unwrap();
}

#[cfg(unix)]
fn run_exec(client: &Client, req: Value) -> Result<(Value, String, String), CliError> {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let last = control::call(&client.state, &client.session, req, |line| {
        for (key, sink) in [("stdout", &mut out), ("stderr", &mut err)] {
            if let Some(d) = line[key].as_str() {
                sink.extend(B64.decode(d).unwrap());
            }
        }
    })?;
    Ok((last, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap()))
}

#[cfg(unix)]
#[test]
fn exec_runs_commands_with_limits() {
    let server = start_server("[exec]\nenabled = true\n");
    let client = connected(&server);
    let (last, out, err) = run_exec(&client, json!({"op": "exec", "argv": ["sh", "-c", "echo a; echo b >&2; exit 3"]})).unwrap();
    assert_eq!((out.as_str(), err.as_str(), &last["exit_code"]), ("a\n", "b\n", &json!(3)));
    let started = Instant::now();
    let (last, ..) = run_exec(&client, json!({"op": "exec", "argv": ["sleep", "5"], "timeout_s": 1})).unwrap();
    assert_eq!(last["timed_out"], true);
    assert!(started.elapsed() < Duration::from_secs(4));
    let err = run_exec(&client, json!({"op": "exec", "argv": ["pwd"], "cwd": "../"})).unwrap_err();
    assert!(err.msg.contains("cwd"), "{}", err.msg);
    let (last, out, _) = run_exec(&client, json!({"op": "exec", "argv": ["sh", "-c", "head -c 2097152 /dev/zero"]})).unwrap();
    assert_eq!(last["truncated"], true);
    assert_eq!(out.len(), 1024 * 1024);
    assert!(run_exec(&client, json!({"op": "exec", "argv": ["true"], "timeout_s": 601})).is_err());
    client.finish().unwrap();
}

#[cfg(unix)]
#[test]
fn shell_runs_commands() {
    let server = start_server("[shell]\nenabled = true\nprogram = \"/bin/sh\"\nargs = []\n");
    let client = connected(&server);
    let mut stream = control::open_raw(&client.state, "lab", json!({"op": "shell", "cols": 80, "rows": 24})).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    stream.write_all(b"echo merhaba$((1+2))\nexit\n").unwrap();
    let mut out = Vec::new();
    let _ = stream.read_to_end(&mut out);
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("merhaba3"), "{text}");
    client.finish().unwrap();
}

// ───────────── gerçek servis süreci ─────────────

#[cfg(unix)]
fn udp_free_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

#[cfg(unix)]
fn spawn_service(config: &Path) -> std::process::Child {
    std::process::Command::new(env!("CARGO_BIN_EXE_openocd-link"))
        .args(["serve", "--no-dht", "--config"])
        .arg(config)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap()
}

#[cfg(unix)]
#[test]
fn service_death_ends_client_with_code_9_and_restart_allows_a_new_session() {
    let dir = temp("proc");
    let key = Key::generate();
    key.save_new(&dir.join("key"), false).unwrap();
    let port = udp_free_port();
    let config = dir.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "key_file = \"{}\"\nudp_port = {port}\n[[forward]]\nname = \"tcl\"\nport = {}\n[files]\ndir = \"{}\"\n",
            dir.join("key").display(),
            echo_server(),
            dir.join("files").display()
        ),
    )
    .unwrap();
    let target: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let connect = || {
        let client = spawn_client(target, key.clone(), "lab", free_port());
        client.ready();
        client
    };
    let mut service = spawn_service(&config);
    thread::sleep(Duration::from_secs(1));
    let client = connect();
    assert_eq!(tunnel_round_trip(client.local, b"canli"), b"canli");
    let killed = Instant::now();
    service.kill().unwrap();
    service.wait().unwrap();
    let err = client.wait().unwrap_err();
    assert_eq!(err.code, 9, "{err}");
    assert!(killed.elapsed() < Duration::from_secs(20), "{:?}", killed.elapsed());
    // Servis yeniden başlayınca yeni oturum kurulur
    let mut service = spawn_service(&config);
    thread::sleep(Duration::from_secs(1));
    let client = connect();
    assert_eq!(tunnel_round_trip(client.local, b"yeniden"), b"yeniden");
    client.finish().unwrap();
    service.kill().unwrap();
    service.wait().unwrap();
}

// ───────────── servis istemciye bağlanır (connect_to) ─────────────

#[test]
fn service_dials_a_fixed_client_address() {
    // İstemci sabit portta bekler, adres vermez, DHT kapalı: yolu servis açar
    let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let server = start_server_with(&format!("connect_to = [\"127.0.0.1:{port}\"]"), "");
    let state = temp("cli");
    let (tx, events) = channel();
    let stop = Arc::new(AtomicBool::new(false));
    let local = free_port();
    let params = ConnectParams {
        key: server.key.clone(),
        session: "ters".into(),
        state_dir: state,
        maps: vec![("tcl".into(), local)],
        timeout: Duration::from_secs(30),
        peers: vec![],
        dht: false,
        name: "ters".into(),
        node: None,
        port,
        stop: stop.clone(),
    };
    let handle = thread::spawn(move || client::connect(params, &mut |e| {
        let _ = tx.send(e.clone());
    }));
    let ready = loop {
        match events.recv_timeout(Duration::from_secs(40)) {
            Ok(e) if e["event"] == "ready" => break true,
            Ok(e) if e["event"] == "closed" => break false,
            Ok(_) => {}
            Err(_) => break false,
        }
    };
    assert!(ready, "servis sabit adrese bağlanamadı");
    assert_eq!(tunnel_round_trip(local, b"ters-yon"), b"ters-yon");
    stop.store(true, Ordering::Release);
    handle.join().unwrap().unwrap();
}
