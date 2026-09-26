//! ThermalStats web API: result submission, community comparison, feedback
//! and debug logs. Blocking calls — the interface runs them on worker threads.

use crate::series::Series;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

pub const DEFAULT_API_URL: &str = "https://thermalstats.com/api/submissions";
const USER_AGENT: &str = concat!("ThermalStats-CLI/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone)]
pub enum ApiError {
    /// Could not reach the server (offline, DNS, TLS, timeout…)
    Connection(String),
    /// The server answered with an error; `message` is its explanation.
    Rejected { status: u16, message: String },
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Connection(msg) => write!(f, "{}", msg),
            ApiError::Rejected { status, message } => write!(f, "{} (HTTP {})", message, status),
        }
    }
}

/// Site root derived from the `--api-url` flag, which (for compatibility with
/// v1) points at the submissions endpoint.
pub fn site_root(api_url: &str) -> String {
    api_url
        .trim_end_matches('/')
        .trim_end_matches("/api/submissions")
        .to_string()
}

/// Localised page URL: English has no prefix, other locales do ("/fr/support").
pub fn page_url(site: &str, locale: &str, path: &str) -> String {
    if locale == "en" {
        format!("{}{}", site, path)
    } else {
        format!("{}/{}{}", site, locale, path)
    }
}

fn client() -> Result<reqwest::blocking::Client, ApiError> {
    reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| ApiError::Connection(e.to_string()))
}

fn post<T: Serialize>(url: &str, body: &T) -> Result<serde_json::Value, ApiError> {
    let response = client()?
        .post(url)
        .json(body)
        .send()
        .map_err(|e| ApiError::Connection(format!("Could not reach ThermalStats: {}", e)))?;

    let status = response.status();
    let text = response.text().unwrap_or_default();
    let json: Option<serde_json::Value> = serde_json::from_str(&text).ok();

    if !status.is_success() {
        let message = json
            .as_ref()
            .and_then(|v| v.get("error"))
            .and_then(|e| e.as_str())
            .map(String::from)
            .unwrap_or_else(|| text.chars().take(200).collect());
        return Err(ApiError::Rejected { status: status.as_u16(), message });
    }
    json.ok_or_else(|| ApiError::Connection("Unexpected response from ThermalStats".into()))
}

fn id_of(json: &serde_json::Value) -> Result<String, ApiError> {
    json.get("id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .ok_or_else(|| ApiError::Connection("Unexpected response from ThermalStats".into()))
}

// ─── Submissions ───────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmissionPayload {
    pub test_type: String,
    pub stress_method: String,
    pub cpu_model: Option<String>,
    pub cpu_cores: Option<i32>,
    pub cpu_threads: Option<i32>,
    pub gpu_model: Option<String>,
    pub gpu_vram: Option<String>,
    pub os: Option<String>,
    pub device_type: Option<String>,
    pub laptop_model: Option<String>,
    pub cooling_type: Option<String>,
    pub cooling_model: Option<String>,
    pub ambient_temp: Option<f64>,
    pub cpu_temp_idle: Option<f64>,
    pub cpu_temp_load: Option<f64>,
    pub gpu_temp_idle: Option<f64>,
    pub gpu_temp_load: Option<f64>,
    pub cpu_usage_max: Option<f64>,
    pub gpu_usage_max: Option<f64>,
    pub test_duration: Option<i64>,
    pub cli_version: Option<String>,
    pub session_id: Option<String>,
    /// The run's temperature/load curve (2.1+).
    pub series: Option<Series>,
    /// Before/after: an earlier result from this machine this run re-tests (2.1+).
    pub baseline_id: Option<String>,
    /// What changed since the baseline, one of `app::CHANGE_TYPES`.
    pub change_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Submitted {
    pub id: String,
    /// The baseline the server linked this result to (None if it refused the link).
    pub baseline_id: Option<String>,
}

/// Submit a result; returns the new result's ID.
pub fn submit_results(site: &str, payload: &SubmissionPayload) -> Result<Submitted, ApiError> {
    let json = post(&format!("{}/api/submissions", site), payload)?;
    Ok(Submitted {
        id: id_of(&json)?,
        baseline_id: json.get("baselineId").and_then(|v| v.as_str()).map(String::from),
    })
}

// ─── Comparison ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareRequest {
    pub test_type: String,
    pub cpu_model: Option<String>,
    pub gpu_model: Option<String>,
    pub cpu_temp_load: Option<f64>,
    pub cpu_temp_idle: Option<f64>,
    pub gpu_temp_load: Option<f64>,
    pub gpu_temp_idle: Option<f64>,
    /// The result just submitted, left out of the comparison.
    pub result_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompareEntry {
    pub model: Option<String>,
    pub count: Option<u64>,
    /// "exact", "similar" or "manufacturer"
    pub scope: Option<String>,
    pub avg_load: Option<f64>,
    /// Percentile among the exact model's results (2.1+ servers, enough data).
    pub standing: Option<Standing>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Standing {
    /// Share of results hotter than this one, 0–100.
    pub cooler_than: f64,
    /// Share of results cooler than this one, 0–100.
    pub hotter_than: f64,
    pub count: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Comparison {
    pub cpu: Option<CompareEntry>,
    pub gpu: Option<CompareEntry>,
}

/// How this result compares with others for the same hardware (read-only).
pub fn compare(site: &str, request: &CompareRequest) -> Result<Comparison, ApiError> {
    let json = post(&format!("{}/api/compare", site), request)?;
    serde_json::from_value(json).map_err(|e| ApiError::Connection(e.to_string()))
}

// ─── Feedback ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackPayload {
    /// 1–5, or none
    pub rating: Option<u8>,
    /// "bug", "idea", "praise" or "other"
    pub category: String,
    pub message: String,
    pub email: Option<String>,
    pub cli_version: String,
    pub os: Option<String>,
    pub cpu_model: Option<String>,
    pub gpu_model: Option<String>,
    pub locale: String,
    /// Screen the feedback was sent from ("home", "results", …)
    pub context: String,
    /// Optional system summary (sensor sources, last test) the user agreed to include.
    pub diagnostics: Option<String>,
    pub session_id: Option<String>,
}

pub fn send_feedback(site: &str, payload: &FeedbackPayload) -> Result<(), ApiError> {
    post(&format!("{}/api/feedback", site), payload).map(|_| ())
}

// ─── Debug logs ────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DebugLogPayload {
    pub log: String,
    pub cpu_model: Option<String>,
    pub gpu_model: Option<String>,
    pub os: Option<String>,
    pub cli_version: Option<String>,
}

/// Upload a diagnostics log; returns its ID (viewable at /debug/{id}).
pub fn submit_debug_log(site: &str, payload: &DebugLogPayload) -> Result<String, ApiError> {
    id_of(&post(&format!("{}/api/debug-logs", site), payload)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_site_root_from_v1_flag() {
        assert_eq!(site_root(DEFAULT_API_URL), "https://thermalstats.com");
        assert_eq!(site_root("http://localhost:3000/api/submissions/"), "http://localhost:3000");
    }

    #[test]
    fn reads_standing_from_compare_replies() {
        let json = r#"{"gpu":{"model":"NVIDIA GeForce RTX 3060","count":40,"scope":"exact","avgLoad":70,"standing":{"coolerThan":81,"hotterThan":19,"count":39}}}"#;
        let c: Comparison = serde_json::from_str(json).unwrap();
        let s = c.gpu.unwrap().standing.unwrap();
        assert_eq!((s.cooler_than, s.count), (81.0, 39));
        // Older servers: no standing.
        let c: Comparison = serde_json::from_str(r#"{"cpu":{"model":"X","count":3,"scope":"exact","avgLoad":60}}"#).unwrap();
        assert!(c.cpu.unwrap().standing.is_none());
    }

    #[test]
    fn localises_page_urls() {
        assert_eq!(page_url("https://t.com", "en", "/support"), "https://t.com/support");
        assert_eq!(page_url("https://t.com", "fr", "/support"), "https://t.com/fr/support");
    }
}
