use anyhow::{Context, Result};
use serde::Deserialize;
use std::io::ErrorKind;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::PathBuf;

#[derive(Deserialize, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// How many images [ and ] move by.
    pub jump: usize,
    /// Milliseconds between steps while holding a next or previous image key.
    pub navigation_repeat_ms: NonZeroU32,
    /// How many bytes to allot in the direction of travel per byte in the opposite direction.
    pub prefetch_bias: NonZeroUsize,
    /// Whether to also prepare the image we would reach by repeating a jump.
    pub prefetch_jump: bool,
    /// How many images may be decoded speculatively at once.
    pub prefetch_inflight: usize,
    /// How much memory prepared images may use, in MiB.
    pub cache_mb: usize,
    /// Start in fullscreen by default.
    pub fullscreen: bool,
    /// The key handler and its arguments.
    pub handler: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            jump: 5,
            navigation_repeat_ms: NonZeroU32::new(50).expect("BUG: zero repeat interval"),
            prefetch_bias: NonZeroUsize::new(3).expect("BUG: zero default bias"),
            prefetch_jump: true,
            prefetch_inflight: 4,
            cache_mb: 512,
            fullscreen: false,
            handler: Vec::new(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_navigation_repeat_interval() {
        assert_eq!(Config::default().navigation_repeat_ms.get(), 50);
        let config: Config = toml::from_str("navigation_repeat_ms = 120").unwrap();
        assert_eq!(config.navigation_repeat_ms.get(), 120);
        assert!(toml::from_str::<Config>("navigation_repeat_ms = 0").is_err());
    }

    #[test]
    fn test_prefetch_bias_must_be_positive() {
        let config: Config = toml::from_str("prefetch_bias = 1").unwrap();
        assert_eq!(config.prefetch_bias.get(), 1);
        assert!(toml::from_str::<Config>("prefetch_bias = 0").is_err());
        assert!(toml::from_str::<Config>("prefetch_bais = 1").is_err());
    }
}
