//! ThermalStats web API: result submission, community comparison, feedback,
//! debug logs and linking to an account. Blocking calls — the interface runs
//! them on worker threads.

use crate::series::Series;
use crate::settings::AccountLink;
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

/// Sends a request and returns the status and JSON body, whatever the status.
/// `bearer` is the account install token of a linked CLI.
fn send<T: Serialize>(
    method: reqwest::Method,
    url: &str,
    body: Option<&T>,
    bearer: Option<&str>,
) -> Result<(u16, Option<serde_json::Value>, String), ApiError> {
    let mut request = client()?.request(method, url);
    if let Some(body) = body {
        request = request.json(body);
    }
    if let Some(token) = bearer {
        request = request.bearer_auth(token);
    }
    let response = request
        .send()
        .map_err(|e| ApiError::Connection(format!("Could not reach ThermalStats: {}", e)))?;
    let status = response.status().as_u16();
    let text = response.text().unwrap_or_default();
    Ok((status, serde_json::from_str(&text).ok(), text))
}

/// The JSON body of a successful response, or the server's explanation.
fn expect_ok(status: u16, json: Option<serde_json::Value>, text: String) -> Result<serde_json::Value, ApiError> {
    if !(200..300).contains(&status) {
        let message = json
            .as_ref()
            .and_then(|v| v.get("error"))
            .and_then(|e| e.as_str())
            .map(String::from)
            .unwrap_or_else(|| text.chars().take(200).collect());
        return Err(ApiError::Rejected { status, message });
    }
    json.ok_or_else(|| ApiError::Connection("Unexpected response from ThermalStats".into()))
}

fn post<T: Serialize>(url: &str, body: &T) -> Result<serde_json::Value, ApiError> {
    let (status, json, text) = send(reqwest::Method::POST, url, Some(body), None)?;
    expect_ok(status, json, text)
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
    /// One-time secret that lets whoever ran the test save it to a ThermalStats
    /// account (2.2+, servers with accounts on). Only ever goes in the URL
    /// fragment of the page we open, never in a link we show or copy.
    pub claim_token: Option<String>,
    /// Saved straight to the linked account (2.3+).
    pub saved: bool,
    /// We sent an account token, but the link was revoked on the website.
    pub link_invalid: bool,
}

/// Submit a result; returns the new result's ID. With `account` (a linked
/// CLI's install token) the server saves it to that account.
pub fn submit_results(site: &str, payload: &SubmissionPayload, account: Option<&str>) -> Result<Submitted, ApiError> {
    let (status, json, text) = send(reqwest::Method::POST, &format!("{}/api/submissions", site), Some(payload), account)?;
    let json = expect_ok(status, json, text)?;
    let flag = |key: &str| json.get(key).and_then(|v| v.as_bool()).unwrap_or(false);
    Ok(Submitted {
        saved: flag("saved"),
        link_invalid: flag("linkInvalid"),
        id: id_of(&json)?,
        baseline_id: json.get("baselineId").and_then(|v| v.as_str()).map(String::from),
        claim_token: json
            .get("claimToken")
            .and_then(|v| v.as_str())
            .filter(|t| (20..=100).contains(&t.len()) && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
            .map(String::from),
    })
}

// ─── Account link (2.3+) ───────────────────────────────────────────
//
// The CLI shows a code; the user approves it on the website (signed in). The
// CLI polls until then and receives an install token that it sends with each
// submission, so results save to the account automatically.

pub const CLI_CLIENT_ID: &str = "thermalstats-cli";

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    /// Seconds until the code expires.
    pub expires_in: u64,
    /// Seconds to wait between polls.
    pub interval: u64,
}

impl DeviceCode {
    /// "ABCDEFGH" → "ABCD-EFGH", as the website shows it.
    pub fn display_code(&self) -> String {
        let code: String = self.user_code.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
        if code.len() == 8 { format!("{}-{}", &code[..4], &code[4..]) } else { code }
    }

    /// The page where the user approves this code.
    pub fn approve_url(&self, site: &str, locale: &str) -> String {
        page_url(site, locale, &format!("/account/link?user_code={}", self.display_code()))
    }
}

/// Start linking: a code for the user to approve on the website.
pub fn request_device_code(site: &str) -> Result<DeviceCode, ApiError> {
    let json = post(&format!("{}/api/account/device/code", site), &serde_json::json!({ "client_id": CLI_CLIENT_ID }))?;
    serde_json::from_value(json).map_err(|e| ApiError::Connection(e.to_string()))
}

#[derive(Debug, Clone, PartialEq)]
pub enum LinkPoll {
    /// Not approved yet: ask again after the interval.
    Pending,
    /// Asked too often: wait longer.
    SlowDown,
    Linked(AccountLink),
    Denied,
    Expired,
}

/// Has the user approved the code yet? `name` labels this PC on the website.
pub fn poll_link(site: &str, code: &DeviceCode, name: &str, machine_id: &str) -> Result<LinkPoll, ApiError> {
    let body = serde_json::json!({ "device_code": code.device_code, "name": name, "machineId": machine_id });
    let (status, json, text) = send(reqwest::Method::POST, &format!("{}/api/member/cli/token", site), Some(&body), None)?;
    parse_link_poll(status, json.as_ref(), &text)
}

fn parse_link_poll(status: u16, json: Option<&serde_json::Value>, text: &str) -> Result<LinkPoll, ApiError> {
    let field = |key: &str| json.and_then(|j| j.get(key)).and_then(|v| v.as_str()).map(String::from);
    if (200..300).contains(&status) {
        return match (field("token"), field("email")) {
            (Some(token), Some(email)) => Ok(LinkPoll::Linked(AccountLink { token, email })),
            _ => Err(ApiError::Connection("Unexpected response from ThermalStats".into())),
        };
    }
    match field("error").as_deref() {
        Some("authorization_pending") => Ok(LinkPoll::Pending),
        Some("slow_down") => Ok(LinkPoll::SlowDown),
        Some("access_denied") => Ok(LinkPoll::Denied),
        Some("expired_token" | "invalid_grant") => Ok(LinkPoll::Expired),
        _ => Err(ApiError::Rejected {
            status,
            message: field("error_description").or_else(|| field("error")).unwrap_or_else(|| text.chars().take(200).collect()),
        }),
    }
}

/// Saves this machine's earlier, unsaved results to the account; returns how many.
pub fn import_history(site: &str, account: &str, machine_id: &str) -> Result<u64, ApiError> {
    let body = serde_json::json!({ "machineId": machine_id });
    let (status, json, text) = send(reqwest::Method::POST, &format!("{}/api/member/cli/import", site), Some(&body), Some(account))?;
    let json = expect_ok(status, json, text)?;
    Ok(json.get("imported").and_then(|v| v.as_u64()).unwrap_or(0))
}

/// Ends the link on the website too (best effort: forgetting the token locally is what matters).
pub fn unlink(site: &str, account: &str) -> Result<(), ApiError> {
    let (status, json, text) = send::<()>(reqwest::Method::DELETE, &format!("{}/api/member/cli/link", site), None, Some(account))?;
    expect_ok(status, json, text).map(|_| ())
}

/// The results URL to open in the browser: with the claim token in the
/// fragment, which browsers never send to the server. The page stores it and
/// removes it from the address bar.
pub fn with_claim(url: &str, claim_token: Option<&str>) -> String {
    match claim_token {
        Some(token) => format!("{}#claim={}", url, token),
        None => url.to_string(),
    }
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
    fn link_codes_display_like_the_website() {
        let code = DeviceCode { device_code: "d".into(), user_code: "ABCDEFGH".into(), expires_in: 900, interval: 5 };
        assert_eq!(code.display_code(), "ABCD-EFGH");
        assert_eq!(code.approve_url("https://thermalstats.com", "fr"), "https://thermalstats.com/fr/account/link?user_code=ABCD-EFGH");
        assert_eq!(code.approve_url("https://thermalstats.com", "en"), "https://thermalstats.com/account/link?user_code=ABCD-EFGH");
    }

    #[test]
    fn reads_link_poll_answers() {
        let j = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        assert_eq!(parse_link_poll(400, Some(&j(r#"{"error":"authorization_pending"}"#)), "").unwrap(), LinkPoll::Pending);
        assert_eq!(parse_link_poll(400, Some(&j(r#"{"error":"slow_down"}"#)), "").unwrap(), LinkPoll::SlowDown);
        assert_eq!(parse_link_poll(429, Some(&j(r#"{"error":"slow_down"}"#)), "").unwrap(), LinkPoll::SlowDown);
        assert_eq!(parse_link_poll(400, Some(&j(r#"{"error":"access_denied"}"#)), "").unwrap(), LinkPoll::Denied);
        assert_eq!(parse_link_poll(400, Some(&j(r#"{"error":"expired_token"}"#)), "").unwrap(), LinkPoll::Expired);
        assert_eq!(parse_link_poll(400, Some(&j(r#"{"error":"invalid_grant"}"#)), "").unwrap(), LinkPoll::Expired);
        assert_eq!(
            parse_link_poll(201, Some(&j(r#"{"token":"tsc_x","email":"a@b.c"}"#)), "").unwrap(),
            LinkPoll::Linked(AccountLink { token: "tsc_x".into(), email: "a@b.c".into() })
        );
        assert!(parse_link_poll(500, None, "oops").is_err());
        assert!(parse_link_poll(201, Some(&j(r#"{"token":"tsc_x"}"#)), "").is_err());
    }

    #[test]
    fn claim_token_only_goes_in_the_fragment() {
        let url = "https://thermalstats.com/fr/results/abc";
        assert_eq!(with_claim(url, None), url);
        assert_eq!(with_claim(url, Some("tok_EN-123")), "https://thermalstats.com/fr/results/abc#claim=tok_EN-123");
    }

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
