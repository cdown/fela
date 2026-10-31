use anyhow::{Context, Result};
use serde::Deserialize;
use std::io::ErrorKind;
use std::path::PathBuf;

#[derive(Deserialize, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// How many images [ and ] move by.
    pub jump: usize,
    /// How many images to prepare in the direction of travel, and in the opposite one.
    pub prefetch_ahead: usize,
    pub prefetch_behind: usize,
    /// Whether to also prepare the image we would reach by repeating a jump.
    pub prefetch_jump: bool,
    /// How many images may be decoded speculatively at once.
    pub prefetch_inflight: usize,
    /// How much memory prepared images may use, in MiB.
    pub cache_mb: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            jump: 10,
            prefetch_ahead: 2,
            prefetch_behind: 1,
            prefetch_jump: true,
            prefetch_inflight: 2,
            cache_mb: 1024,
        }
    }
}

#[cfg(windows)]
fn config_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(PathBuf::from)
}

#[cfg(not(windows))]
fn config_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".config")))
}

pub fn load() -> Result<Config> {
    let Some(path) = config_dir().map(|dir| dir.join("fela").join("config.toml")) else {
        return Ok(Config::default());
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(Config::default()),
        Err(err) => Err(err).with_context(|| format!("failed to read {}", path.display())),
    }
}
