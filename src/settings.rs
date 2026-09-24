//! Remembered answers from the previous run, so repeat tests (after a new
//! cooler, a repaste, a room change…) take a couple of key presses.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    pub lang: Option<String>,
    pub test_type: Option<String>,
    pub duration_secs: Option<u64>,
    pub gpu_name: Option<String>,
    pub cooling_type: Option<String>,
    pub cooling_model: Option<String>,
    pub laptop_model: Option<String>,
    pub ambient_temp: Option<f64>,
}

fn path() -> Option<PathBuf> {
    crate::platform::data_dir().map(|d| d.join("settings.json"))
}

impl Settings {
    /// Load saved settings; a missing or unreadable file gives the defaults.
    pub fn load() -> Self {
        path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Best effort: failing to save only means the next run starts blank.
    pub fn save(&self) {
        let Some(path) = path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, text);
        }
    }
}
