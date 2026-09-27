use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub out_dir: String,
}

impl Default for Config {
    fn default() -> Self {
        let out = std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join("pdf");
        Self { out_dir: out.display().to_string() }
    }
}

fn dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("xps2pdf")
    }
    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join("Library")
            .join("Application Support")
            .join("xps2pdf")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
            PathBuf::from(xdg).join("xps2pdf")
        } else {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".config"))
                .unwrap_or_else(std::env::temp_dir)
                .join("xps2pdf")
        }
    }
}

fn path() -> PathBuf {
    dir().join("config.json")
}

pub fn load() -> Config {
    let Ok(text) = std::fs::read_to_string(path()) else {
        return Config::default();
    };
    serde_json::from_str(&text).unwrap_or_else(|_| Config::default())
}

pub fn save(cfg: &Config) {
    let p = path();
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(cfg) {
        let _ = std::fs::write(p, text);
    }
}
