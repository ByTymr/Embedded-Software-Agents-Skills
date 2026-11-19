use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use clap::{Parser, Subcommand};
use openocd_link::client::{self, ConnectParams, control, state};
use openocd_link::error::CliError;
use openocd_link::key::Key;
use openocd_link::{output, util};
use serde_json::{Value, json};
use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "openocd-link", version, about = "Uzak kart servisi")]
struct Cli {
    /// Oturum adı: [a-z0-9_-], 1..32 karakter
    #[arg(long, default_value = "default", global = true)]
    session: String,
    /// Makine okuyabilir çıktı
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Bağlantı anahtarı üretir ve bir kez yazdırır
    Keygen {
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        force: bool,
    },
    /// Anahtarı stdin'den okuyup kaydeder
    ImportKey {
        #[arg(long)]
        out: PathBuf,
        #[arg(long)]
        force: bool,
    },
    /// Kartın takılı olduğu cihazda servisi çalıştırır
    Serve {
        #[arg(long)]
        config: PathBuf,
        #[arg(long, hide = true)]
        no_dht: bool,
    },
    /// Servisin bağlanacağı istemci adresini ayarlar (Pi'de, root): ip[:port], port yoksa 47124
    SetClient {
        addr: String,
        #[arg(long, default_value = "/etc/openocd-link/client.addr")]
        file: PathBuf,
    },
    /// Servise bağlanır ve oturum daemon'u olarak çalışır
    Connect {
        #[arg(long)]
        key_file: PathBuf,
        #[arg(long = "map")]
        maps: Vec<String>,
        /// Tek denemenin süresi (sn); dolunca yeni bir UDP ucuyla yeniden denenir
        #[arg(long, default_value_t = 120)]
        timeout: u64,
        #[arg(long = "peer", hide = true)]
        peers: Vec<std::net::SocketAddr>,
        #[arg(long, hide = true)]
        no_dht: bool,
        #[arg(long)]
        name: Option<String>,
        /// Tek deneme: servis bulunamazsa ya da bağlantı koparsa yeniden denemeden çık
        #[arg(long)]
        once: bool,
        /// Sabit yerel UDP portu (UDP yönlendirmesi için)
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
    /// DHT'de servisin kaydını arar ve durumunu yazar (tanılama)
    #[command(hide = true)]
    Probe {
        #[arg(long)]
        key_file: PathBuf,
        /// Aynı süreçte kaç kez okunacağı (3 sn arayla)
        #[arg(long, default_value_t = 1)]
        count: u32,
    },
    Status,
    Disconnect,
    Info,
    Ls,
    Put {
        local: PathBuf,
        #[arg(long)]
        name: Option<String>,
    },
    Get {
        name: String,
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long)]
        force: bool,
    },
    Rm {
        name: String,
    },
    /// Uzaktaki OpenOCD sürecini yönetir
    Openocd {
        #[command(subcommand)]
        action: Ocd,
    },
    Exec {
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long, default_value_t = 60)]
        timeout: u64,
        #[arg(last = true, required = true)]
        argv: Vec<String>,
    },
    /// Uzak cihazda etkileşimli kabuk
    Shell,
}

#[derive(Subcommand)]
enum Ocd {
    Start,
    Stop,
    Status,
    Log {
        #[arg(long, default_value_t = 100)]
        lines: u64,
    },
}

fn usage(msg: impl Into<String>) -> CliError {
    CliError::new(2, msg)
}

fn read_key_stdin() -> Result<String, CliError> {
    if std::io::stdin().is_terminal() {
        rpassword::prompt_password("Anahtar: ").map_err(CliError::from)
    } else {
        let mut text = String::new();
        std::io::stdin().read_to_string(&mut text)?;
        Ok(text)
    }
}

fn fingerprint_line(json: bool, key: &Key, out: &std::path::Path) {
    let value = json!({"ok": true, "fingerprint": key.fingerprint(), "path": out.to_string_lossy()});
    if json {
        output::print_json(&value);
    } else {
        println!("Anahtar kaydedildi: {} (parmak izi {})", out.display(), key.fingerprint());
    }
}

#[cfg(unix)]
fn serve(config: PathBuf, no_dht: bool) -> Result<i32, CliError> {
    use openocd_link::config::Config;
    use openocd_link::server;
    let mut cfg = Config::load(&config).map_err(usage)?;
    // set-client ile yazılan adres: yapılandırmadaki connect_to'ya eklenir (kurulum bu dosyayı ezmez)
    let extra = config.with_file_name("client.addr");
    if let Ok(text) = std::fs::read_to_string(&extra) {
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            if line.parse::<std::net::SocketAddr>().is_ok() && !cfg.connect_to.iter().any(|a| a == line) {
                cfg.connect_to.push(line.to_string());
            }
        }
    }
    let key = Key::load(&cfg.key_file).map_err(usage)?;
    let port = cfg.udp_port;
    let ctx = server::Ctx::new(cfg, key).map_err(|e| usage(format!("Dosya klasörü hazırlanamadı: {e}")))?;
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(signal, stop.clone()).map_err(CliError::from)?;
    }
    let node = openocd_link_core::punch::start(port, None);
    Ok(if server::serve(ctx, node, !no_dht, stop) { 0 } else { 1 })
}

#[cfg(not(unix))]
fn serve(_: PathBuf, _: bool) -> Result<i32, CliError> {
    Err(usage("Servis yalnız Linux/systemd üzerinde çalışır"))
}

fn run(cli: &Cli) -> Result<i32, CliError> {
    if !util::valid_session(&cli.session) {
        return Err(usage("Oturum adı [a-z0-9_-] ve 1..32 karakter olmalı"));
    }
    let dir = cli.state_dir.clone().unwrap_or_else(state::default_dir);
    let json = cli.json;
    let session = cli.session.as_str();
    let call = |req: Value| control::call(&dir, session, req, |_| {});
    match &cli.command {
        Command::Keygen { out, force } => {
            let key = Key::generate();
            key.save_new(out, *force).map_err(usage)?;
            if json {
                output::print_json(&json!({"ok": true, "key": key.text(), "fingerprint": key.fingerprint()}));
            } else {
                println!("{}", key.text());
                eprintln!("Anahtar {} dosyasına yazıldı (parmak izi {}). Bir daha gösterilmez.", out.display(), key.fingerprint());
            }
        }
        Command::ImportKey { out, force } => {
            let key = Key::parse(&read_key_stdin()?).map_err(|e| CliError::new(3, e))?;
            key.save_new(out, *force).map_err(usage)?;
            fingerprint_line(json, &key, out);
        }
        Command::SetClient { addr, file } => {
            let text = if addr.parse::<std::net::IpAddr>().is_ok() {
                format!("{addr}:47124")
            } else {
                addr.clone()
            };
            let parsed: std::net::SocketAddr =
                text.parse().map_err(|_| usage(format!("Adres ip:port olmalı: {addr}")))?;
            std::fs::write(file, format!("{parsed}\n"))
                .map_err(|e| CliError::new(1, format!("{} yazılamadı (sudo gerekir): {e}", file.display())))?;
            let restarted = std::process::Command::new("systemctl")
                .args(["restart", "openocd-link"])
                .status()
                .is_ok_and(|s| s.success());
            println!("İstemci adresi: {parsed}");
            println!("{}", if restarted { "Servis yeniden başlatıldı." } else { "Servisi yeniden başlatın: sudo systemctl restart openocd-link" });
        }
        Command::Serve { config, no_dht } => return serve(config.clone(), *no_dht),
        Command::Connect { key_file, maps, timeout, peers, no_dht, name, once, port } => {
            let key = Key::load_lenient(key_file).map_err(|e| CliError::new(3, e))?;
            let maps = client::parse_maps(maps)?;
            let stop = Arc::new(AtomicBool::new(false));
            #[cfg(unix)]
            for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
                signal_hook::flag::register(signal, stop.clone()).map_err(CliError::from)?;
            }
            // Kalıcı bağlantı: servis bulunamazsa, meşgulse ya da bağlantı koparsa yeniden dener.
            // Her deneme yeni bir UDP ucuyla başlar. Durdurmak için Ctrl+C ya da `disconnect`.
            loop {
                let params = ConnectParams {
                    key: key.clone(),
                    session: cli.session.clone(),
                    state_dir: dir.clone(),
                    maps: maps.clone(),
                    timeout: Duration::from_secs((*timeout).max(1)),
                    peers: peers.clone(),
                    dht: !no_dht,
                    name: name.clone().unwrap_or_else(|| std::env::var("HOSTNAME").unwrap_or_else(|_| "istemci".into())),
                    node: None,
                    port: *port,
                    stop: stop.clone(),
                };
                let mut ready = false;
                let result = client::connect(params, &mut |event| {
                    ready |= event["event"] == "ready";
                    output::event(json, event);
                });
                let Err(e) = result else { break };
                // 4: bulunamadı, 6: meşgul, 9: bağlantı koptu. Diğerleri (anahtar, sürüm, port) kalıcıdır.
                let transient = matches!(e.code, 4 | 6 | 9);
                if *once || !transient || stop.load(std::sync::atomic::Ordering::Acquire) {
                    return Err(e);
                }
                if !ready {
                    output::event(json, &json!({"event": "retry", "code": e.code, "reason": e.msg}));
                }
                std::thread::sleep(Duration::from_secs(3));
            }
        }
        Command::Probe { key_file, count } => {
            let key = Key::load_lenient(key_file).map_err(|e| CliError::new(3, e))?;
            let dht = openocd_link_core::rendezvous::shared().ok_or_else(|| CliError::new(1, "DHT'ye katılınamadı"))?;
            let keys = key.rendezvous();
            let role = openocd_link_core::rendezvous::Role::Host;
            for i in 0..(*count).max(1) {
                if i > 0 {
                    std::thread::sleep(Duration::from_secs(3));
                }
                println!("{}", openocd_link_core::rendezvous::diagnose(dht, &keys, role));
            }
        }
        Command::Status => output::result(json, "status", &call(json!({"op": "status"}))?),
        Command::Disconnect => output::result(json, "disconnect", &client::disconnect(&dir, session)?),
        Command::Info => output::result(json, "info", &call(json!({"op": "info"}))?),
        Command::Ls => output::result(json, "ls", &call(json!({"op": "file.list"}))?),
        Command::Put { local, name } => {
            let path = std::fs::canonicalize(local).map_err(|e| CliError::new(1, format!("{}: {e}", local.display())))?;
            let mut req = json!({"op": "put", "path": path.to_string_lossy()});
            if let Some(name) = name {
                req["name"] = name.as_str().into();
            }
            let value = control::call(&dir, session, req, |p| progress(json, p))?;
            output::result(json, "put", &value);
        }
        Command::Get { name, out, force } => {
            let out = match out {
                Some(p) => std::path::absolute(p)?,
                None => std::env::current_dir()?.join(name),
            };
            let req = json!({"op": "get", "name": name, "out": out.to_string_lossy(), "force": force});
            let value = control::call(&dir, session, req, |p| progress(json, p))?;
            output::result(json, "get", &value);
        }
        Command::Rm { name } => output::result(json, "rm", &call(json!({"op": "file.delete", "name": name}))?),
        Command::Openocd { action } => {
            let (req, label) = match action {
                Ocd::Start => (json!({"op": "openocd.start"}), "openocd start"),
                Ocd::Stop => (json!({"op": "openocd.stop"}), "openocd stop"),
                Ocd::Status => (json!({"op": "openocd.status"}), "openocd status"),
                Ocd::Log { lines } => (json!({"op": "openocd.log", "lines": lines}), "openocd log"),
            };
            output::result(json, label, &call(req)?);
        }
        Command::Exec { cwd, timeout, argv } => return exec(&dir, session, json, cwd.as_deref(), *timeout, argv),
        Command::Shell => client::terminal::run(&dir, session)?,
    }
    Ok(0)
}

fn progress(json: bool, line: &Value) {
    if !json && let (Some(done), Some(total)) = (line["progress"].as_u64(), line["total"].as_u64()) {
        eprint!("\r{done}/{total} bayt");
        if done == total {
            eprintln!();
        }
    }
}

fn exec(dir: &std::path::Path, session: &str, json: bool, cwd: Option<&str>, timeout: u64, argv: &[String]) -> Result<i32, CliError> {
    let mut req = json!({"op": "exec", "argv": argv, "timeout_s": timeout});
    if let Some(cwd) = cwd {
        req["cwd"] = cwd.into();
    }
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let last = control::call(dir, session, req, |line| {
        for (key, sink) in [("stdout", &mut out), ("stderr", &mut err)] {
            if let Some(data) = line[key].as_str().and_then(|d| B64.decode(d).ok()) {
                if !json {
                    if key == "stdout" {
                        let _ = std::io::stdout().write_all(&data).and_then(|()| std::io::stdout().flush());
                    } else {
                        let _ = std::io::stderr().write_all(&data);
                    }
                }
                sink.extend(data);
            }
        }
    })?;
    let code = last["exit_code"].as_i64().map(|c| c as i32);
    if json {
        output::print_json(&json!({
            "ok": true, "exit_code": last["exit_code"], "signal": last["signal"],
            "timed_out": last["timed_out"], "truncated": last["truncated"],
            "stdout": String::from_utf8_lossy(&out), "stderr": String::from_utf8_lossy(&err),
        }));
    }
    Ok(code.unwrap_or(if last["timed_out"] == true { 124 } else { 1 }))
}

fn main() {
    openocd_link_core::log::set_logger(|line| eprintln!("{line}"));
    let cli = Cli::parse();
    let code = match run(&cli) {
        Ok(code) => code,
        Err(e) => {
            output::error(cli.json, e.code, &e.msg);
            e.code
        }
    };
    std::process::exit(code);
}
