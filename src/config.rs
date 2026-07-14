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
}

impl Default for Config {
    fn default() -> Self {
        Config {
            sort_field: SortField::Tree,
            sort_desc: false,
            theme: None,
            diff_mode: DiffMode::Working,
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
            "sort_field={}\nsort_desc={}\ntheme={}\ndiff_mode={}\n",
            self.sort_field.as_key(),
            self.sort_desc,
            self.theme.as_deref().unwrap_or(""),
            self.diff_mode.as_key(),
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
