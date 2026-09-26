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
    /// The last result this machine submitted: the "before" of a re-test.
    pub last_result: Option<LastResult>,
}

/// Enough of a submitted result to compare a re-test against it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LastResult {
    pub id: String,
    pub machine_id: String,
    /// When it was submitted (RFC 3339).
    pub at: String,
    pub test_type: String,
    pub cpu_idle: Option<f64>,
    pub cpu_load: Option<f64>,
    pub gpu_idle: Option<f64>,
    pub gpu_load: Option<f64>,
}

/// The server only links re-tests to results up to this old.
const MAX_BASELINE_DAYS: i64 = 90;

impl LastResult {
    pub fn submitted_at(&self) -> Option<chrono::DateTime<chrono::Local>> {
        chrono::DateTime::parse_from_rfc3339(&self.at).ok().map(|d| d.with_timezone(&chrono::Local))
    }

    /// Whether a new run on `machine_id` can be compared with this result.
    pub fn usable_for(&self, machine_id: &str) -> bool {
        !self.id.is_empty()
            && !machine_id.is_empty()
            && self.machine_id == machine_id
            && self
                .submitted_at()
                .is_some_and(|at| (chrono::Local::now() - at).num_days() <= MAX_BASELINE_DAYS)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_result_must_be_this_machine_and_recent() {
        let recent = LastResult {
            id: "abc".into(),
            machine_id: "cli-1".into(),
            at: chrono::Local::now().to_rfc3339(),
            ..Default::default()
        };
        assert!(recent.usable_for("cli-1"));
        assert!(!recent.usable_for("cli-2"));
        let old = LastResult { at: "2020-01-01T00:00:00+00:00".into(), ..recent.clone() };
        assert!(!old.usable_for("cli-1"));
        let broken = LastResult { at: "yesterday".into(), ..recent };
        assert!(!broken.usable_for("cli-1"));
    }

    #[test]
    fn old_settings_files_still_load() {
        let s: Settings = serde_json::from_str(r#"{"lang":"fr","durationSecs":120}"#).unwrap();
        assert!(s.last_result.is_none());
    }
}
