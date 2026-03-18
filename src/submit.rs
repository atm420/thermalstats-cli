use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

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
}

#[derive(Debug, Deserialize)]
pub struct SubmissionResponse {
    pub id: String,
}

pub async fn submit_results(api_url: &str, payload: &SubmissionPayload) -> Result<String> {
    let client = reqwest::Client::new();

    let response = client
        .post(api_url)
        .header("Content-Type", "application/json")
        .header("User-Agent", format!("ThermalStats-CLI/{}", env!("CARGO_PKG_VERSION")))
        .json(payload)
        .send()
        .await
        .context("Failed to connect to ThermalStats API")?;

    let status = response.status();

    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        anyhow::bail!(
            "API returned status {}: {}",
            status.as_u16(),
            body
        );
    }

    let result: SubmissionResponse = response
        .json()
        .await
        .context("Failed to parse API response")?;

    Ok(result.id)
}
