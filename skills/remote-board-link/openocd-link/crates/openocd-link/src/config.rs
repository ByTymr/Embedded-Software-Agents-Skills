//! Servis yapılandırması (TOML) ve doğrulaması.

use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub key_file: PathBuf,
    #[serde(default)]
    pub udp_port: u16,
    #[serde(default)]
    pub device_name: String,
    #[serde(default = "default_poll")]
    pub poll_secs: u64,
    /// İstemcinin sabit adresleri (ip:port): servis DHT'yi beklemeden bunlara bağlanmayı dener
    #[serde(default)]
    pub connect_to: Vec<String>,
    #[serde(default)]
    pub forward: Vec<Forward>,
    #[serde(default)]
    pub files: Files,
    #[serde(default)]
    pub openocd: OpenOcd,
    #[serde(default)]
    pub exec: Exec,
    #[serde(default)]
    pub shell: Shell,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Forward {
    pub name: String,
    pub port: u16,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Files {
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "default_dir")]
    pub dir: PathBuf,
    #[serde(default = "default_file_mb")]
    pub max_file_mb: u64,
    #[serde(default = "default_total_mb")]
    pub max_total_mb: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenOcd {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_openocd")]
    pub executable: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_tcl")]
    pub tcl_port: u16,
    #[serde(default = "default_gdb")]
    pub gdb_port: u16,
    #[serde(default)]
    pub autostart: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exec {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_timeout_max")]
    pub timeout_max_s: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shell {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_shell")]
    pub program: PathBuf,
    #[serde(default = "default_shell_args")]
    pub args: Vec<String>,
}

fn yes() -> bool {
    true
}
fn default_poll() -> u64 {
    4
}
fn default_dir() -> PathBuf {
    "/var/lib/openocd-link/files".into()
}
fn default_file_mb() -> u64 {
    64
}
fn default_total_mb() -> u64 {
    1024
}
fn default_openocd() -> PathBuf {
    "/usr/bin/openocd".into()
}
fn default_tcl() -> u16 {
    6666
}
fn default_gdb() -> u16 {
    3333
}
fn default_timeout_max() -> u64 {
    600
}
fn default_shell() -> PathBuf {
    "/bin/bash".into()
}
fn default_shell_args() -> Vec<String> {
    vec!["-l".into()]
}

impl Default for Files {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: default_dir(),
            max_file_mb: default_file_mb(),
            max_total_mb: default_total_mb(),
        }
    }
}
impl Default for OpenOcd {
    fn default() -> Self {
        Self {
            enabled: false,
            executable: default_openocd(),
            args: Vec::new(),
            tcl_port: default_tcl(),
            gdb_port: default_gdb(),
            autostart: false,
        }
    }
}
impl Default for Exec {
    fn default() -> Self {
        Self {
            enabled: false,
            timeout_max_s: default_timeout_max(),
        }
    }
}
impl Default for Shell {
    fn default() -> Self {
        Self {
            enabled: false,
            program: default_shell(),
            args: default_shell_args(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("Yapılandırma okunamadı ({}): {e}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let config: Self = toml::from_str(text).map_err(|e| format!("Yapılandırma hatalı: {e}"))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(2..=60).contains(&self.poll_secs) {
            return Err("poll_secs 2..60 olmalı".into());
        }
        if self.connect_to.len() > 8 {
            return Err("connect_to en fazla 8 adres içermeli".into());
        }
        for a in &self.connect_to {
            a.parse::<std::net::SocketAddr>()
                .map_err(|_| format!("connect_to geçersiz (ip:port olmalı): {a}"))?;
        }
        if !(1..=8).contains(&self.forward.len()) {
            return Err("forward 1..8 kayıt içermeli".into());
        }
        let (mut names, mut ports) = (HashSet::new(), HashSet::new());
        for f in &self.forward {
            let ok = (1..=16).contains(&f.name.len())
                && f.name
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
            if !ok {
                return Err(format!("forward.name geçersiz: {}", f.name));
            }
            if f.port == 0 {
                return Err(format!("forward.port 1..65535 olmalı ({})", f.name));
            }
            if !names.insert(f.name.clone()) {
                return Err(format!("forward.name yinelenmiş: {}", f.name));
            }
            if !ports.insert(f.port) {
                return Err(format!("forward.port yinelenmiş: {}", f.port));
            }
        }
        let files = &self.files;
        if !(1..=1024).contains(&files.max_file_mb) {
            return Err("files.max_file_mb 1..1024 olmalı".into());
        }
        if !(files.max_file_mb..=65_536).contains(&files.max_total_mb) {
            return Err("files.max_total_mb max_file_mb..65536 olmalı".into());
        }
        let ocd = &self.openocd;
        if ocd.enabled {
            if !ocd.executable.is_absolute() {
                return Err("openocd.executable mutlak yol olmalı".into());
            }
            if ocd.tcl_port == 0 || ocd.gdb_port == 0 || ocd.tcl_port == ocd.gdb_port {
                return Err("openocd.tcl_port ve gdb_port farklı, 1..65535 olmalı".into());
            }
            for arg in &ocd.args {
                let reserved = ["tcl_port", "gdb_port", "telnet_port", "bindto"];
                if arg.starts_with("-c") || reserved.iter().any(|r| arg.contains(r)) {
                    return Err(format!(
                        "openocd.args içinde servisin eklediği ayar olamaz: {arg}"
                    ));
                }
            }
        }
        if !(1..=3600).contains(&self.exec.timeout_max_s) {
            return Err("exec.timeout_max_s 1..3600 olmalı".into());
        }
        if self.shell.enabled && !self.shell.program.is_absolute() {
            return Err("shell.program mutlak yol olmalı".into());
        }
        Ok(())
    }

    pub fn warnings(&self) -> Vec<&'static str> {
        if self.exec.enabled || self.shell.enabled {
            vec!["UYARI: anahtarı bilen herkes bu cihazda komut çalıştırabilir"]
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = include_str!("../../../packaging/linux/openocd-link.toml");

    fn base() -> String {
        "key_file = \"/k\"\n[[forward]]\nname = \"tcl\"\nport = 6666\n".to_string()
    }

    #[test]
    fn example_is_valid() {
        let c = Config::parse(EXAMPLE).unwrap();
        assert_eq!(c.forward.len(), 2);
        assert!(c.openocd.enabled && !c.exec.enabled && !c.shell.enabled);
    }

    #[test]
    fn minimal_uses_defaults() {
        let c = Config::parse(&base()).unwrap();
        assert_eq!(c.poll_secs, 4);
        assert_eq!(c.files.max_file_mb, 64);
        assert!(c.files.enabled);
    }

    #[test]
    fn every_rule_rejects() {
        let cases = [
            ("poll_secs = 1\n", "poll_secs"),
            ("poll_secs = 61\n", "poll_secs"),
            ("bilinmeyen = 1\n", "bilinmeyen"),
            ("[files]\nmax_file_mb = 0\n", "max_file_mb"),
            ("[files]\nmax_file_mb = 1025\n", "max_file_mb"),
            ("[files]\nmax_file_mb = 10\nmax_total_mb = 5\n", "max_total_mb"),
            ("[exec]\ntimeout_max_s = 0\n", "timeout_max_s"),
            ("[exec]\ntimeout_max_s = 3601\n", "timeout_max_s"),
            ("[shell]\nenabled = true\nprogram = \"bash\"\n", "shell.program"),
            ("[openocd]\nenabled = true\nexecutable = \"openocd\"\n", "executable"),
            ("[openocd]\nenabled = true\nargs = [\"-c\", \"init\"]\n", "args"),
            ("[openocd]\nenabled = true\nargs = [\"-f\", \"x\", \"gdb_port 1\"]\n", "args"),
            ("[openocd]\nenabled = true\nargs = [\"bindto 0.0.0.0\"]\n", "args"),
        ];
        for (extra, field) in cases {
            let text = if extra.starts_with('[') {
                format!("{}{extra}", base())
            } else {
                format!("{extra}{}", base())
            };
            let err = Config::parse(&text).unwrap_err();
            assert!(err.contains(field), "{extra:?} -> {err}");
        }
    }

    #[test]
    fn forward_rules() {
        let key = "key_file = \"/k\"\n";
        assert!(Config::parse(key).unwrap_err().contains("forward"));
        let dup_name = format!("{key}[[forward]]\nname=\"a\"\nport=1\n[[forward]]\nname=\"a\"\nport=2\n");
        assert!(Config::parse(&dup_name).unwrap_err().contains("yinelenmiş"));
        let dup_port = format!("{key}[[forward]]\nname=\"a\"\nport=1\n[[forward]]\nname=\"b\"\nport=1\n");
        assert!(Config::parse(&dup_port).unwrap_err().contains("yinelenmiş"));
        let bad = format!("{key}[[forward]]\nname=\"A\"\nport=1\n");
        assert!(Config::parse(&bad).unwrap_err().contains("forward.name"));
        let zero = format!("{key}[[forward]]\nname=\"a\"\nport=0\n");
        assert!(Config::parse(&zero).unwrap_err().contains("forward.port"));
        let mut many = key.to_string();
        for i in 1..=9 {
            many += &format!("[[forward]]\nname=\"s{i}\"\nport={i}\n");
        }
        assert!(Config::parse(&many).unwrap_err().contains("1..8"));
    }
}
