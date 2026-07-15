//! Persisted UI state — sort order, theme, and diff mode — so diffski opens the
//! same way every time. Stored as a tiny `key=value` file (no serde dependency)
//! at `$XDG_CONFIG_HOME/diffski/config` (falling back to `~/.config/...`).

use crate::app::{DiffMode, SortField};
use std::path::PathBuf;

#[derive(Clone)]
pub struct Config {
    pub sort_field: SortField,
    pub sort_desc: bool,
    pub theme: Option<String>,
    pub diff_mode: DiffMode,
    /// Left (file-list) pane width as a percentage of the terminal.
    pub split_pct: u16,
    /// Follow-latest: jump the view to files as they change on disk.
    pub follow: bool,
    /// Check for and install updates in the background on startup.
    pub auto_update: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            sort_field: SortField::Tree,
            sort_desc: false,
            theme: None,
            diff_mode: DiffMode::Working,
            split_pct: 30,
            follow: false,
            auto_update: true,
        }
    }
}

impl Config {
    /// Load the config, returning defaults if it's missing or unreadable.
    pub fn load() -> Config {
        let mut cfg = Config::default();
        let Some(path) = config_path() else {
            return cfg;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return cfg;
        };
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "sort_field" => {
                    if let Some(v) = SortField::from_key(value) {
                        cfg.sort_field = v;
                    }
                }
                "sort_desc" => cfg.sort_desc = value == "true",
                "theme" => cfg.theme = (!value.is_empty()).then(|| value.to_string()),
                "diff_mode" => {
                    if let Some(v) = DiffMode::from_key(value) {
                        cfg.diff_mode = v;
                    }
                }
                "split_pct" => {
                    if let Ok(v) = value.parse::<u16>() {
                        cfg.split_pct = v.clamp(15, 85);
                    }
                }
                "follow" => cfg.follow = value == "true",
                "auto_update" => cfg.auto_update = value != "false",
                _ => {}
            }
        }
        cfg
    }

    /// Write the config, best-effort (errors are ignored — persistence is a
    /// convenience, not a requirement).
    pub fn save(&self) {
        let Some(path) = config_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let body = format!(
            "sort_field={}\nsort_desc={}\ntheme={}\ndiff_mode={}\nsplit_pct={}\nfollow={}\nauto_update={}\n",
            self.sort_field.as_key(),
            self.sort_desc,
            self.theme.as_deref().unwrap_or(""),
            self.diff_mode.as_key(),
            self.split_pct,
            self.follow,
            self.auto_update,
        );
        let _ = std::fs::write(&path, body);
    }
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("diffski").join("config"))
}
