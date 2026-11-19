//! İnsan okuyabilir metin ya da `--json` çıktısı.

use serde_json::Value;

pub fn print_json(value: &Value) {
    println!("{}", serde_json::to_string(value).unwrap_or_default());
}

/// `connect` olayları: insan çıktısında Türkçe satırlar
pub fn event(json: bool, value: &Value) {
    if json {
        print_json(value);
        return;
    }
    match value["event"].as_str() {
        Some("searching") => {
            println!("Servis aranıyor (anahtar {})…", value["fingerprint"].as_str().unwrap_or(""));
        }
        Some("ready") => {
            println!(
                "Bağlandı: {} ({}), doğrulama kodu {}",
                value["device"].as_str().unwrap_or(""),
                value["peer"].as_str().unwrap_or(""),
                value["sas"].as_str().unwrap_or("")
            );
            for s in value["services"].as_array().into_iter().flatten() {
                println!(
                    "  {}: 127.0.0.1:{} → uzak {}",
                    s["name"].as_str().unwrap_or(""),
                    s["local_port"],
                    s["remote_port"]
                );
            }
        }
        Some("closed") => println!("Bağlantı kapandı: {}", value["reason"].as_str().unwrap_or("")),
        Some("retry") => println!("{}; yeniden deneniyor…", value["reason"].as_str().unwrap_or("")),
        _ => {}
    }
}

pub fn error(json: bool, code: i32, msg: &str) {
    if json {
        print_json(&serde_json::json!({"ok": false, "code": code, "error": msg}));
    } else {
        eprintln!("Hata: {msg}");
    }
}

/// Komut sonucu
pub fn result(json: bool, command: &str, value: &Value) {
    if json {
        print_json(value);
        return;
    }
    match command {
        "ls" => {
            for f in value["files"].as_array().into_iter().flatten() {
                println!("{:>10}  {}", f["size"], f["name"].as_str().unwrap_or(""));
            }
        }
        "openocd log" => {
            for l in value["lines"].as_array().into_iter().flatten() {
                println!("{}", l.as_str().unwrap_or(""));
            }
        }
        "put" | "get" => println!(
            "{} ({} bayt, sha256 {})",
            value["path"].as_str().unwrap_or(""),
            value["size"],
            &value["sha256"].as_str().unwrap_or("")[..12.min(value["sha256"].as_str().unwrap_or("").len())]
        ),
        "status" => {
            println!(
                "{}: {} ({}), doğrulama kodu {}",
                if value["connected"] == true { "bağlı" } else { "bağlı değil" },
                value["device"].as_str().unwrap_or(""),
                value["peer"].as_str().unwrap_or(""),
                value["sas"].as_str().unwrap_or("")
            );
            for s in value["services"].as_array().into_iter().flatten() {
                println!(
                    "  {}: 127.0.0.1:{} → uzak {}",
                    s["name"].as_str().unwrap_or(""),
                    s["local_port"],
                    s["remote_port"]
                );
            }
        }
        _ => println!("{}", serde_json::to_string_pretty(value).unwrap_or_default()),
    }
}
