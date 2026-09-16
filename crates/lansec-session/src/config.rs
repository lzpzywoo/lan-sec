//! Session settings shared by CLI, GUI launcher, host, and client.

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use lansec_protocol::ChromaPref;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum SessionMode {
    #[default]
    Host,
    Client,
}

impl SessionMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Host => "Host",
            Self::Client => "Client",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionConfig {
    pub mode: SessionMode,
    /// Host listen address (e.g. `0.0.0.0:44700`).
    pub bind: String,
    /// Client peer address (e.g. `192.168.1.10:44700`).
    pub connect: String,
    pub pin: String,
    /// Encode / present cadence (30..=120).
    pub target_fps: u32,
    pub target_mbps: u32,
    pub min_mbps: u32,
    pub max_mbps: u32,
    pub chroma: ChromaPref,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            mode: SessionMode::Host,
            bind: "0.0.0.0:44700".into(),
            connect: "127.0.0.1:44700".into(),
            pin: "0000".into(),
            target_fps: 60,
            target_mbps: 50,
            min_mbps: 40,
            max_mbps: 100,
            chroma: ChromaPref::Auto,
        }
    }
}

impl SessionConfig {
    pub fn for_host(bind: SocketAddr, pin: String) -> Self {
        Self {
            mode: SessionMode::Host,
            bind: bind.to_string(),
            pin,
            ..Self::default()
        }
    }

    pub fn for_client(connect: SocketAddr, pin: String) -> Self {
        Self {
            mode: SessionMode::Client,
            connect: connect.to_string(),
            pin,
            ..Self::default()
        }
    }

    pub fn clamp(&mut self) {
        self.target_fps = self.target_fps.clamp(15, 120);
        self.min_mbps = self.min_mbps.max(1);
        self.max_mbps = self.max_mbps.max(self.min_mbps);
        self.target_mbps = self.target_mbps.clamp(self.min_mbps, self.max_mbps);
        if self.pin.is_empty() {
            self.pin = "0000".into();
        }
    }

    pub fn bind_addr(&self) -> Result<SocketAddr> {
        self.bind
            .parse()
            .with_context(|| format!("invalid bind address: {}", self.bind))
    }

    pub fn connect_addr(&self) -> Result<SocketAddr> {
        self.connect
            .parse()
            .with_context(|| format!("invalid connect address: {}", self.connect))
    }

    pub fn target_bps(&self) -> u32 {
        self.target_mbps.saturating_mul(1_000_000)
    }

    pub fn min_bps(&self) -> u32 {
        self.min_mbps.saturating_mul(1_000_000)
    }

    pub fn max_bps(&self) -> u32 {
        self.max_mbps.saturating_mul(1_000_000)
    }

    pub fn config_path() -> PathBuf {
        if cfg!(target_os = "macos") {
            dirs_next_home()
                .map(|h| h.join("Library/Application Support/lansec/config.json"))
                .unwrap_or_else(|| PathBuf::from("lansec-config.json"))
        } else if cfg!(target_os = "windows") {
            std::env::var_os("APPDATA")
                .map(|a| PathBuf::from(a).join("lansec").join("config.json"))
                .unwrap_or_else(|| PathBuf::from("lansec-config.json"))
        } else {
            dirs_next_home()
                .map(|h| h.join(".config/lansec/config.json"))
                .unwrap_or_else(|| PathBuf::from("lansec-config.json"))
        }
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        match fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<SessionConfig>(&text) {
                Ok(mut cfg) => {
                    cfg.clamp();
                    cfg
                }
                Err(e) => {
                    tracing::warn!(?path, %e, "config parse failed; using defaults");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create config dir {}", parent.display()))?;
        }
        let mut cfg = self.clone();
        cfg.clamp();
        let text = serde_json::to_string_pretty(&cfg).context("serialize config")?;
        fs::write(&path, text).with_context(|| format!("write {}", path.display()))?;
        Ok(())
    }

    pub fn validate_for_start(&self) -> Result<()> {
        match self.mode {
            SessionMode::Host => {
                let _ = self.bind_addr()?;
            }
            SessionMode::Client => {
                let _ = self.connect_addr()?;
            }
        }
        if self.min_mbps > self.max_mbps {
            return Err(anyhow!("min Mbps must be ≤ max Mbps"));
        }
        Ok(())
    }
}

fn dirs_next_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_keeps_target_in_range() {
        let mut c = SessionConfig {
            target_mbps: 200,
            min_mbps: 40,
            max_mbps: 100,
            ..Default::default()
        };
        c.clamp();
        assert_eq!(c.target_mbps, 100);
        assert_eq!(c.target_fps, 60);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("lansec-cfg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // Point HOME so config_path lands under our temp dir on macOS/Linux.
        // SAFETY: test-only; not run in parallel with other HOME-dependent tests in this crate.
        unsafe { std::env::set_var("HOME", &dir) };
        let mut cfg = SessionConfig::default();
        cfg.target_fps = 30;
        cfg.target_mbps = 20;
        cfg.min_mbps = 10;
        cfg.max_mbps = 40;
        cfg.pin = "9999".into();
        cfg.save().unwrap();
        let loaded = SessionConfig::load();
        assert_eq!(loaded.target_fps, 30);
        assert_eq!(loaded.target_mbps, 20);
        assert_eq!(loaded.pin, "9999");
        let _ = fs::remove_dir_all(&dir);
    }
}
