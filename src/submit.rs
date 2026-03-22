use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug)]
pub enum SubmitError {
    Connection(String),
    ApiRejected { status: u16, message: String },
}

impl fmt::Display for SubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SubmitError::Connection(msg) => write!(f, "{}", msg),
            SubmitError::ApiRejected { status, message } => {
                write!(f, "HTTP {}: {}", status, message)
            }
        }
    }
}

impl std::error::Error for SubmitError {}

#[derive(Debug, Serialize)]
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
}

#[derive(Debug, Deserialize)]
pub struct SubmissionResponse {
    pub id: String,
}

pub async fn submit_results(api_url: &str, payload: &SubmissionPayload) -> Result<String, SubmitError> {
    let client = reqwest::Client::new();

    let response = client
        .post(api_url)
        .header("Content-Type", "application/json")
        .header("User-Agent", format!("ThermalStats-CLI/{}", env!("CARGO_PKG_VERSION")))
        .json(payload)
        .send()
        .await
        .map_err(|e| SubmitError::Connection(format!("Failed to connect to ThermalStats API: {}", e)))?;

    let status = response.status();

    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        // Try to extract the error message from JSON response
        let message = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
            .unwrap_or(body);
        return Err(SubmitError::ApiRejected {
            status: status.as_u16(),
            message,
        });
    }

    let result: SubmissionResponse = response
        .json()
        .await
        .map_err(|e| SubmitError::Connection(format!("Failed to parse API response: {}", e)))?;

    Ok(result.id)
}
