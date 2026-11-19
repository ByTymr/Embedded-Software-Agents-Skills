//! Yerel kontrol soketi: daemon tarafı (komutları uzak servise çevirir) ve komut tarafı.

use super::session::{FileMsg, Remote, ShellMsg};
use super::state::State;
use crate::error::CliError;
use crate::util::{hex, sha256_of, valid_file_name};
use crate::wire;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::thread;
use std::time::Duration;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const PROGRESS_EVERY: u64 = 256 * 1024;

pub struct Daemon {
    pub remote: Arc<Remote>,
    pub token: String,
    pub stop: Arc<AtomicBool>,
    pub state: State,
}

type Failure = (String, String);

fn fail(code: &str, msg: impl Into<String>) -> Failure {
    (code.to_string(), msg.into())
}

fn from_cli(e: CliError) -> Failure {
    fail("remote", e.msg)
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn line(stream: &mut TcpStream, value: &Value) -> std::io::Result<()> {
    let mut text = serde_json::to_vec(value)?;
    text.push(b'\n');
    stream.write_all(&text)
}

pub fn listen(daemon: Arc<Daemon>, listener: TcpListener) {
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let daemon = daemon.clone();
            thread::spawn(move || connection(&daemon, stream));
        }
    });
}

fn connection(daemon: &Arc<Daemon>, mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut text = String::new();
    let Ok(clone) = stream.try_clone() else { return };
    if BufReader::new(clone.take(65_536)).read_line(&mut text).is_err() {
        return;
    }
    let _ = stream.set_read_timeout(None);
    let Ok(request) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    // Yanlış jeton: yanıtsız kapanır
    if !same(request["token"].as_str().unwrap_or(""), &daemon.token) {
        return;
    }
    let result = dispatch(daemon, &mut stream, &request);
    let last = match result {
        Ok(Value::Object(mut map)) => {
            map.insert("ok".into(), true.into());
            Value::Object(map)
        }
        Ok(_) => json!({"ok": true}),
        Err((code, error)) => json!({"ok": false, "code": code, "error": error}),
    };
    if request["op"] != "shell" {
        let _ = line(&mut stream, &last);
    }
}

fn dispatch(d: &Arc<Daemon>, stream: &mut TcpStream, req: &Value) -> Result<Value, Failure> {
    let op = req["op"].as_str().unwrap_or("");
    match op {
        "status" => {
            let mut value = serde_json::to_value(&d.state).unwrap_or_default();
            value["token"] = Value::Null;
            value["connected"] = d.remote.link.is_alive().into();
            Ok(value)
        }
        "shutdown" => {
            d.stop.store(true, Ordering::Release);
            d.remote.link.close();
            Ok(json!({}))
        }
        "info" => Ok(d.remote.info()),
        "file.list" | "file.delete" | "openocd.start" | "openocd.stop" | "openocd.status"
        | "openocd.log" => {
            let mut fields = req.clone();
            if let Some(map) = fields.as_object_mut() {
                map.remove("token");
                map.remove("op");
            }
            let mut response = d.remote.request(op, fields, REQUEST_TIMEOUT).map_err(from_cli)?;
            if let Some(map) = response.as_object_mut() {
                map.remove("id");
                map.remove("ok");
            }
            Ok(response)
        }
        "put" => put(d, stream, req),
        "get" => get(d, stream, req),
        "exec" => exec(d, stream, req),
        "shell" => shell(d, stream, req),
        "shell.resize" => {
            let n = |k: &str| req[k].as_u64().unwrap_or(0).clamp(1, 1000) as u16;
            let mut data = n("cols").to_be_bytes().to_vec();
            data.extend(n("rows").to_be_bytes());
            d.remote.send_shell(wire::SHELL_RESIZE, &data).map_err(|e| fail("io", e.to_string()))?;
            Ok(json!({}))
        }
        _ => Err(fail("bad_request", format!("Bilinmeyen işlem: {op}"))),
    }
}

fn path_of(req: &Value, key: &str) -> Result<PathBuf, Failure> {
    let path = PathBuf::from(req[key].as_str().unwrap_or(""));
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(fail("bad_request", format!("{key} mutlak yol olmalı")))
    }
}

fn put(d: &Arc<Daemon>, stream: &mut TcpStream, req: &Value) -> Result<Value, Failure> {
    let path = path_of(req, "path")?;
    let name = match req["name"].as_str() {
        Some(n) => n.to_string(),
        None => path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
    };
    if !valid_file_name(&name) {
        return Err(fail("bad_request", format!("Geçersiz uzak dosya adı: {name}")));
    }
    let io = |e: std::io::Error| fail("io", e.to_string());
    let mut file = std::fs::File::open(&path).map_err(io)?;
    let size = file.metadata().map_err(io)?.len();
    let sha = sha256_of(&path).map_err(io)?;
    let transfer = d.remote.alloc_transfer();
    let events = d.remote.watch_file(transfer);
    let finish = |r: Result<Value, Failure>| {
        d.remote.unwatch_file(transfer);
        r
    };
    let fields = json!({"name": name, "size": size, "sha256": sha, "transfer": transfer});
    if let Err(e) = d.remote.request("file.put", fields, REQUEST_TIMEOUT) {
        return finish(Err(from_cli(e)));
    }
    let mut buf = [0u8; 8192];
    let mut sent = 0u64;
    let mut reported = 0u64;
    loop {
        if let Ok(FileMsg::Abort(why)) = events.try_recv() {
            return finish(Err(fail("remote", why)));
        }
        let n = file.read(&mut buf).map_err(io)?;
        if n == 0 {
            break;
        }
        if let Err(e) = d.remote.send_file(transfer, &buf[..n]) {
            return finish(Err(fail("io", e.to_string())));
        }
        sent += n as u64;
        if sent - reported >= PROGRESS_EVERY {
            reported = sent;
            line(stream, &json!({"progress": sent, "total": size})).map_err(io)?;
        }
    }
    line(stream, &json!({"progress": sent, "total": size})).map_err(io)?;
    let result = d
        .remote
        .request("file.put_end", json!({"transfer": transfer}), REQUEST_TIMEOUT)
        .map_err(from_cli)
        .map(|mut r| {
            if let Some(m) = r.as_object_mut() {
                m.remove("id");
                m.remove("ok");
            }
            r
        });
    finish(result)
}

fn get(d: &Arc<Daemon>, stream: &mut TcpStream, req: &Value) -> Result<Value, Failure> {
    let out = path_of(req, "out")?;
    let name = req["name"].as_str().unwrap_or("");
    if !valid_file_name(name) {
        return Err(fail("bad_request", "Geçersiz uzak dosya adı"));
    }
    if out.exists() && req["force"] != true {
        return Err(fail("exists", format!("{} zaten var (--force ile üzerine yazılır)", out.display())));
    }
    let io = |e: std::io::Error| fail("io", e.to_string());
    let part = PathBuf::from(format!("{}.part", out.display()));
    let transfer = d.remote.alloc_transfer();
    let events = d.remote.watch_file(transfer);
    let result = (|| {
        let head = d
            .remote
            .request("file.get", json!({"name": name, "transfer": transfer}), REQUEST_TIMEOUT)
            .map_err(from_cli)?;
        let (size, want) = (head["size"].as_u64().unwrap_or(0), head["sha256"].as_str().unwrap_or("").to_string());
        let mut file = OpenOptions::new().write(true).create(true).truncate(true).open(&part).map_err(io)?;
        let (mut hasher, mut got, mut reported) = (Sha256::new(), 0u64, 0u64);
        loop {
            match events.recv_timeout(Duration::from_secs(15)) {
                Ok(FileMsg::Data(data)) => {
                    got += data.len() as u64;
                    hasher.update(&data);
                    file.write_all(&data).map_err(io)?;
                    if got - reported >= PROGRESS_EVERY {
                        reported = got;
                        line(stream, &json!({"progress": got, "total": size})).map_err(io)?;
                    }
                }
                Ok(FileMsg::End) => break,
                Ok(FileMsg::Abort(why)) => return Err(fail("remote", why)),
                Err(RecvTimeoutError::Timeout) => return Err(fail("timeout", "Aktarım zaman aşımına uğradı")),
                Err(RecvTimeoutError::Disconnected) => return Err(fail("io", "Oturum kapandı")),
            }
        }
        file.sync_all().map_err(io)?;
        let actual = hex(&hasher.finalize());
        if got != size || actual != want {
            return Err(fail("checksum", "Boyut ya da sha256 tutmadı"));
        }
        std::fs::rename(&part, &out).map_err(io)?;
        line(stream, &json!({"progress": got, "total": size})).map_err(io)?;
        Ok(json!({"path": out.to_string_lossy(), "size": size, "sha256": actual}))
    })();
    d.remote.unwatch_file(transfer);
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result
}

fn exec(d: &Arc<Daemon>, stream: &mut TcpStream, req: &Value) -> Result<Value, Failure> {
    let id = d.remote.alloc_id();
    let output = d.remote.watch_exec(id);
    let timeout = req["timeout_s"].as_u64().unwrap_or(60);
    let mut fields = json!({"argv": req["argv"], "timeout_s": timeout});
    if let Some(cwd) = req["cwd"].as_str() {
        fields["cwd"] = cwd.into();
    }
    let drain = |stream: &mut TcpStream| {
        while let Ok((kind, data)) = output.try_recv() {
            let key = if kind == 1 { "stdout" } else { "stderr" };
            if line(stream, &json!({key: B64.encode(data)})).is_err() {
                return false;
            }
        }
        true
    };
    let result = (|| {
        let rx = d.remote.send_request(id, "exec", fields).map_err(from_cli)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(timeout + 30);
        loop {
            if !drain(stream) {
                return Err(fail("io", "Yerel bağlantı koptu"));
            }
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(response) => {
                    drain(stream);
                    return if response["ok"] == true {
                        Ok(json!({
                            "exit_code": response["exit_code"], "signal": response["signal"],
                            "timed_out": response["timed_out"], "truncated": response["truncated"],
                        }))
                    } else {
                        Err(fail(
                            response["code"].as_str().unwrap_or("remote"),
                            response["error"].as_str().unwrap_or("Uzak hata"),
                        ))
                    };
                }
                Err(RecvTimeoutError::Timeout) if std::time::Instant::now() < deadline => {}
                Err(_) => return Err(fail("timeout", "Uzak servis yanıt vermedi")),
            }
        }
    })();
    d.remote.unwatch_exec(id);
    result
}

fn shell(d: &Arc<Daemon>, stream: &mut TcpStream, req: &Value) -> Result<Value, Failure> {
    let rx = d.remote.watch_shell();
    let mut fields = json!({"cols": req["cols"], "rows": req["rows"]});
    if let Some(term) = req["term"].as_str() {
        fields["term"] = term.into();
    }
    if let Err(e) = d.remote.request("shell.open", fields, REQUEST_TIMEOUT) {
        d.remote.unwatch_shell();
        let _ = line(stream, &json!({"ok": false, "code": "remote", "error": e.msg}));
        return Ok(json!({}));
    }
    let io = |_| fail("io", "Yerel bağlantı koptu");
    line(stream, &json!({"ok": true})).map_err(io)?;
    let mut input = stream.try_clone().map_err(io)?;
    let remote = d.remote.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = input.read(&mut buf) {
            if n == 0 || remote.send_shell(wire::SHELL_DATA, &buf[..n]).is_err() {
                break;
            }
        }
        flag.store(true, Ordering::Release);
    });
    while !stop.load(Ordering::Acquire) && d.remote.link.is_alive() {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(ShellMsg::Data(data)) => {
                if stream.write_all(&data).is_err() {
                    break;
                }
            }
            Ok(ShellMsg::Exit) => break,
            Err(_) => {}
        }
    }
    d.remote.unwatch_shell();
    let _ = d.remote.request("shell.close", json!({}), Duration::from_secs(5));
    let _ = stream.shutdown(std::net::Shutdown::Both);
    Ok(json!({}))
}

// ───────────── komut tarafı ─────────────

fn read_line_raw(stream: &mut TcpStream) -> std::io::Result<Option<Value>> {
    let mut bytes = Vec::new();
    let mut one = [0u8; 1];
    loop {
        if stream.read(&mut one)? == 0 {
            return Ok(None);
        }
        if one[0] == b'\n' {
            return Ok(serde_json::from_slice(&bytes).ok());
        }
        bytes.push(one[0]);
        if bytes.len() > 4 * 1024 * 1024 {
            return Ok(None);
        }
    }
}

fn open(dir: &Path, session: &str, mut req: Value) -> Result<TcpStream, CliError> {
    let state = State::read(dir, session)?;
    let none = || CliError::new(10, "Bağlı oturum yok: önce openocd-link connect");
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], state.control_port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).map_err(|_| none())?;
    req["token"] = state.token.into();
    line(&mut stream, &req)?;
    Ok(stream)
}

/// Komutu daemon'a gönderir; ilerleme ve çıktı satırlarını `on_line`'a verir, son satırı döner
pub fn call(
    dir: &Path,
    session: &str,
    req: Value,
    mut on_line: impl FnMut(&Value),
) -> Result<Value, CliError> {
    let mut stream = open(dir, session, req)?;
    loop {
        match read_line_raw(&mut stream)? {
            None => return Err(CliError::new(10, "Bağlı oturum yok: önce openocd-link connect")),
            Some(value) if value.get("ok").is_some() => {
                if value["ok"] == true {
                    return Ok(value);
                }
                return Err(CliError::new(1, value["error"].as_str().unwrap_or("Hata")));
            }
            Some(value) => on_line(&value),
        }
    }
}

/// Kabuk için: ilk satırı okur, ardından akışı ham bayt olarak döner
pub fn open_raw(dir: &Path, session: &str, req: Value) -> Result<TcpStream, CliError> {
    let mut stream = open(dir, session, req)?;
    match read_line_raw(&mut stream)? {
        Some(v) if v["ok"] == true => Ok(stream),
        Some(v) => Err(CliError::new(1, v["error"].as_str().unwrap_or("Hata"))),
        None => Err(CliError::new(10, "Bağlı oturum yok: önce openocd-link connect")),
    }
}
