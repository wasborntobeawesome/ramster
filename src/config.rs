//! User settings (~/.ramster/config.json) and on-disk locations.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::analyze::custom::{self, CustomRule};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Sampling interval of the background daemon.
    pub interval_sec: u64,
    /// Days of history to keep.
    pub retention_days: u32,
    /// Something counts as idle after this many minutes below ~1% CPU.
    pub idle_minutes: u32,
    /// macOS notification when pressure stays at warning or worse.
    pub notify: bool,
    /// Let the daemon free "safe" findings by itself under pressure.
    pub auto_free_safe: bool,
    /// Never suggest anything whose app, name or command line contains one of these.
    pub never_touch: Vec<String>,
    /// GUI apps ramster may suggest quitting when idle; empty = any not built-in excluded.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub quit_candidates: Vec<String>,
    /// Rules from ~/.ramster/rules.d/*.json (read by `load`, never written here).
    #[serde(skip)]
    pub custom_rules: Vec<CustomRule>,
    /// Problems found while reading rules.d.
    #[serde(skip)]
    pub rule_errors: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            interval_sec: 60,
            retention_days: 7,
            idle_minutes: 20,
            notify: true,
            auto_free_safe: false,
            never_touch: Vec::new(),
            quit_candidates: Vec::new(),
            custom_rules: Vec::new(),
            rule_errors: Vec::new(),
        }
    }
}

/// $RAMSTER_HOME or ~/.ramster
pub fn home() -> PathBuf {
    match std::env::var("RAMSTER_HOME") {
        Ok(h) if !h.is_empty() => PathBuf::from(h),
        _ => PathBuf::from(crate::util::home()).join(".ramster"),
    }
}

pub fn path() -> PathBuf {
    home().join("config.json")
}

/// Rule packs: ~/.ramster/rules.d/*.json
pub fn rules_dir() -> PathBuf {
    home().join("rules.d")
}

impl Config {
    pub fn load() -> Config {
        let mut c: Config = std::fs::read(path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        c.interval_sec = c.interval_sec.max(10);
        c.retention_days = c.retention_days.max(1);
        c.idle_minutes = c.idle_minutes.max(1);
        (c.custom_rules, c.rule_errors) = custom::load(&rules_dir());
        c
    }

    /// Does any field match a never_touch pattern?
    pub fn protected(&self, fields: &[&str]) -> bool {
        self.never_touch.iter().any(|pat| {
            let pat = pat.trim().to_lowercase();
            !pat.is_empty() && fields.iter().any(|f| f.to_lowercase().contains(&pat))
        })
    }
}

/// Create the config file with defaults if missing.
pub fn write_default() -> std::io::Result<()> {
    if path().exists() {
        return Ok(());
    }
    std::fs::create_dir_all(home())?;
    let mut s = serde_json::to_string_pretty(&Config::default()).unwrap_or_default();
    s.push('\n');
    std::fs::write(path(), s)
}
