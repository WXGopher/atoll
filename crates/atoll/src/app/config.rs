//! What Atoll remembers between runs: where the user last dragged a card to,
//! and whether they want the taskbar readout at all. The readout's own
//! position is not here: it is computed from the taskbar's layout every time,
//! not remembered.
//!
//! Every operation here degrades to a default rather than failing. A config file
//! that cannot be read or written is a readout that opens in its default spot
//! and forgets where it was moved to — annoying, and nothing more. Refusing to
//! start over it would be much worse.

use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Overrides [`config_dir`] outright. Set it to run Atoll against a throwaway
/// config — a test build, say — without disturbing the one the user's own Atoll
/// keeps.
pub const CONFIG_DIR_ENV: &str = "ATOLL_CONFIG_DIR";

/// `%APPDATA%\Atoll`, falling back to `%LOCALAPPDATA%\Atoll` on the machines
/// where the roaming variable is not set, and to [`CONFIG_DIR_ENV`] ahead of
/// both when it is set.
pub fn config_dir() -> io::Result<PathBuf> {
    if let Some(explicit) = std::env::var_os(CONFIG_DIR_ENV).filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(explicit));
    }
    let base = std::env::var_os("APPDATA")
        .or_else(|| std::env::var_os("LOCALAPPDATA"))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "neither APPDATA nor LOCALAPPDATA is set",
            )
        })?;
    Ok(PathBuf::from(base).join("Atoll"))
}

pub fn config_path() -> io::Result<PathBuf> {
    Ok(config_dir()?.join("config.json"))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Config {
    pub taskbar: TaskbarConfig,
    pub card: CardConfig,
    /// Unknown keys are preserved on rewrite rather than silently dropped, so
    /// a config written by any other version of Atoll — older or newer —
    /// survives a run of this one with its own settings intact.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CardConfig {
    /// Where the user last dragged a card to, in physical screen pixels. `None`
    /// until they have moved one — until then a card opens beside the taskbar
    /// readout, which is where it belongs.
    ///
    /// Remembered because somebody who moved a card moved it for a reason: that
    /// corner of the screen was in the way of what they were doing.
    pub x: Option<i32>,
    pub y: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TaskbarConfig {
    /// Off means no readout in the taskbar at all. On by default: it is the
    /// cheapest place Atoll has to put a number somebody checks all day.
    pub enabled: bool,
}

impl Default for TaskbarConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            taskbar: TaskbarConfig::default(),
            card: CardConfig::default(),
            extra: serde_json::Map::new(),
        }
    }
}

impl Config {
    /// Read the config, or hand back the defaults for anything that goes wrong —
    /// a missing file, a truncated write, a hand-edit that broke the JSON.
    pub fn load() -> Self {
        config_path()
            .ok()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    /// Persist, silently. The caller is in the middle of something the user
    /// cares about more than this file.
    pub fn save(&self) {
        let _ = self.try_save();
    }

    fn try_save(&self) -> io::Result<()> {
        let dir = config_dir()?;
        std::fs::create_dir_all(&dir)?;
        let body = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        // Write beside the target and rename over it: a crash mid-write then
        // costs the new settings rather than the old ones.
        let temporary = dir.join("config.json.tmp");
        std::fs::write(&temporary, body)?;
        let target = dir.join("config.json");
        match std::fs::rename(&temporary, &target) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = std::fs::remove_file(&temporary);
                Err(error)
            }
        }
    }

    /// Where the user last dragged a card to, if they ever have.
    pub fn card_position(&self) -> Option<(i32, i32)> {
        Some((self.card.x?, self.card.y?))
    }

    pub fn set_card_position(&mut self, x: i32, y: i32) {
        self.card.x = Some(x);
        self.card.y = Some(y);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_field_falls_back_to_its_default() {
        let config: Config = serde_json::from_str(r#"{"card":{"x":40}}"#).unwrap();
        assert!(config.taskbar.enabled);
        assert_eq!(config.card.x, Some(40));
        assert_eq!(config.card.y, None);
        // Only half a position is no position at all.
        assert_eq!(config.card_position(), None);
    }

    #[test]
    fn keys_from_a_newer_atoll_survive_a_rewrite() {
        let config: Config =
            serde_json::from_str(r#"{"taskbar":{"enabled":false},"soundEnabled":true}"#).unwrap();
        assert!(!config.taskbar.enabled);

        let rewritten = serde_json::to_value(&config).unwrap();
        assert_eq!(rewritten["soundEnabled"], serde_json::json!(true));
    }

    #[test]
    fn nonsense_reads_as_the_defaults() {
        assert!(serde_json::from_str::<Config>("not json").is_err());
        // `load` swallows exactly that, which is the behaviour that matters.
        assert_eq!(Config::default().taskbar, TaskbarConfig::default());
        assert_eq!(Config::default().card, CardConfig::default());
    }

    /// The readout is on unless the user has turned it off.
    #[test]
    fn the_taskbar_readout_defaults_to_on() {
        assert!(Config::default().taskbar.enabled);

        // A config an older Atoll wrote may have no taskbar section at all,
        // or a taskbar section with the offset keys the readout used to
        // remember. Both still load.
        let old: Config = serde_json::from_str(r#"{"card":{"x":10,"y":10}}"#).unwrap();
        assert!(old.taskbar.enabled);
        let dragged: Config =
            serde_json::from_str(r#"{"taskbar":{"enabled":false,"x":40,"y":9}}"#).unwrap();
        assert!(!dragged.taskbar.enabled);
    }

    #[test]
    fn a_round_trip_keeps_a_dragged_card_where_it_was_left() {
        let mut config = Config::default();
        assert_eq!(config.card_position(), None, "unplaced until it is moved");

        config.set_card_position(1200, 40);
        let raw = serde_json::to_string(&config).unwrap();
        let back: Config = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.card_position(), Some((1200, 40)));
    }
}
