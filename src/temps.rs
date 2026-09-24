//! Temperature and GPU-load sources, tried in a fixed priority order.
//!
//! The order matches earlier CLI versions so new results stay comparable
//! with the existing database:
//!   CPU (Windows): HWiNFO → MSI Afterburner → AIDA64 → Core Temp →
//!                  LibreHardwareMonitor → WMI ACPI → OHM WMI → perf counters
//!   GPU (Windows): NVIDIA driver → HWiNFO → Afterburner → AIDA64 → LHM → WMI
//!
//! Nothing here prints: the interface owns the terminal. Every reading
//! carries a short source label that the interface shows next to it.

use crate::gpus::GpuDevice;

#[derive(Debug, Clone)]
pub struct TempReading {
    pub celsius: f64,
    pub source: String,
}

impl TempReading {
    fn new(celsius: f64, source: impl Into<String>) -> Option<Self> {
        (celsius > 0.0 && celsius < 150.0).then(|| TempReading { celsius, source: source.into() })
    }
}

/// Readings that come from a long-running helper rather than a direct query.
#[derive(Debug, Clone, Default)]
pub struct Context {
    #[cfg(windows)]
    pub lhm: Option<crate::lhm::LhmSample>,
}

// ─── CPU ────────────────────────────────────────────────────────────

pub fn read_cpu(ctx: &Context) -> Option<TempReading> {
    #[cfg(windows)]
    {
        // Shared-memory sources first — zero-install, no admin required.
        if let Some(r) = crate::hwinfo::read_temps() {
            if let Some(t) = r.cpu_temp {
                return TempReading::new(t, format!("HWiNFO / {}", r.cpu_source.unwrap_or_default()));
            }
        }
        if let Some(r) = crate::afterburner::read_temps() {
            if let Some(t) = r.cpu_temp {
                return TempReading::new(t, r.cpu_source.unwrap_or_else(|| "MSI Afterburner".into()));
            }
        }
        if let Some(r) = crate::aida64::read_temps() {
            if let Some(t) = r.cpu_temp {
                return TempReading::new(t, r.cpu_source.unwrap_or_else(|| "AIDA64".into()));
            }
        }
        if let Some(r) = crate::coretemp::read_temps() {
            if let Some(t) = r.cpu_temp {
                return TempReading::new(t, r.cpu_source.unwrap_or_else(|| "Core Temp".into()));
            }
        }
        // Embedded LibreHardwareMonitor (needs admin + PawnIO)
        if let Some(sample) = &ctx.lhm {
            if let Some(t) = sample.cpu_temp {
                let sensor = sample.cpu_sensor.as_deref().unwrap_or("CPU");
                return TempReading::new(t, format!("LibreHardwareMonitor / {}", sensor));
            }
        }
        if let Some(t) = read_cpu_temp_wmi() {
            return TempReading::new(t, "ACPI thermal zone (motherboard)");
        }
        if let Some((t, name)) = read_cpu_temp_ohm() {
            return TempReading::new(t, format!("Hardware monitor WMI / {}", name));
        }
        if let Some(t) = read_cpu_temp_perfcounter() {
            return TempReading::new(t, "Windows thermal zone (motherboard)");
        }
        None
    }

    #[cfg(target_os = "linux")]
    {
        let _ = ctx;
        read_cpu_temp_linux()
    }

    #[cfg(target_os = "macos")]
    {
        let _ = ctx;
        read_cpu_temp_powermetrics()
            .map(|t| TempReading { celsius: t, source: "powermetrics / CPU die".into() })
            .or_else(|| read_cpu_temp_sysctl().map(|t| TempReading { celsius: t, source: "sysctl".into() }))
    }
}

// ─── GPU ────────────────────────────────────────────────────────────

/// Temperature of `gpu`. With several GPUs installed (`multi`), sources that
/// can't tell GPUs apart are skipped rather than risk reading the wrong card.
pub fn read_gpu(ctx: &Context, gpu: &GpuDevice, multi: bool) -> Option<TempReading> {
    if let Some(index) = gpu.nvml_index {
        if let Some((t, label)) = crate::nvidia::temperature(index) {
            return TempReading::new(t, label);
        }
    }

    #[cfg(windows)]
    {
        let filter = |name: &str| !multi || gpu.matches_name(name);
        if let Some(r) = crate::hwinfo::read_filtered(&filter) {
            if let Some(t) = r.gpu_temp {
                return TempReading::new(t, format!("HWiNFO / {}", r.gpu_source.unwrap_or_default()));
            }
        }
        if let Some(r) = crate::afterburner::read_filtered(&filter) {
            if let Some(t) = r.gpu_temp {
                return TempReading::new(t, r.gpu_source.unwrap_or_else(|| "MSI Afterburner".into()));
            }
        }
        if !multi {
            if let Some(r) = crate::aida64::read_temps() {
                if let Some(t) = r.gpu_temp {
                    return TempReading::new(t, r.gpu_source.unwrap_or_else(|| "AIDA64".into()));
                }
            }
        }
        if let Some(g) = lhm_gpu(ctx, gpu, multi) {
            if let Some(t) = g.temp {
                let sensor = g.sensor.as_deref().unwrap_or("GPU");
                return TempReading::new(t, format!("LibreHardwareMonitor / {}", sensor));
            }
        }
        if !multi {
            if let Some(t) = read_gpu_temp_wmi() {
                return TempReading::new(t, "WMI temperature probe");
            }
        }
        None
    }

    #[cfg(target_os = "linux")]
    {
        let _ = ctx;
        if let Some(dev) = &gpu.sysfs_device {
            if let Some(t) = read_hwmon_temp(dev) {
                return TempReading::new(t, "amdgpu / edge");
            }
        }
        if !multi {
            return read_gpu_temp_drm_any().and_then(|t| TempReading::new(t, "hwmon / GPU"));
        }
        None
    }

    #[cfg(target_os = "macos")]
    {
        let _ = (ctx, gpu, multi);
        read_gpu_temp_powermetrics().and_then(|t| TempReading::new(t, "powermetrics / GPU die"))
    }
}

/// GPU load (%) for `gpu`, when some source reports it.
pub fn read_gpu_usage(ctx: &Context, gpu: &GpuDevice, multi: bool) -> Option<f64> {
    if let Some(index) = gpu.nvml_index {
        if let Some(u) = crate::nvidia::utilization(index) {
            return Some(u);
        }
    }

    #[cfg(windows)]
    {
        let filter = |name: &str| !multi || gpu.matches_name(name);
        if let Some(u) = crate::hwinfo::read_filtered(&filter).and_then(|r| r.gpu_usage) {
            return Some(u);
        }
        if let Some(u) = crate::afterburner::read_filtered(&filter).and_then(|r| r.gpu_usage) {
            return Some(u);
        }
        lhm_gpu(ctx, gpu, multi).and_then(|g| g.load)
    }

    #[cfg(target_os = "linux")]
    {
        let _ = (ctx, multi);
        let dev = gpu.sysfs_device.as_ref()?;
        std::fs::read_to_string(dev.join("gpu_busy_percent"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    #[cfg(target_os = "macos")]
    {
        let _ = (ctx, multi);
        None
    }
}

#[cfg(windows)]
fn lhm_gpu<'a>(ctx: &'a Context, gpu: &GpuDevice, multi: bool) -> Option<&'a crate::lhm::LhmGpu> {
    let sample = ctx.lhm.as_ref()?;
    sample
        .gpus
        .iter()
        .find(|g| gpu.matches_name(&g.name))
        .or_else(|| if multi { None } else { sample.gpus.last() })
}

// ─── Windows WMI fallbacks ─────────────────────────────────────────

/// Read CPU-adjacent temperature from Windows Performance Counter thermal zones.
/// Uses Win32_PerfFormattedData_Counters_ThermalZoneInformation which is
/// available without admin privileges. Returns the highest thermal zone value
/// (typically the embedded controller zone closest to the CPU).
#[cfg(windows)]
fn read_cpu_temp_perfcounter() -> Option<f64> {
    use wmi::{COMLibrary, WMIConnection};
    use serde::Deserialize;

    #[derive(Deserialize, Debug)]
    #[allow(dead_code)]
    struct ThermalZonePerf {
        #[serde(rename = "Name")]
        name: Option<String>,
        #[serde(rename = "HighPrecisionTemperature")]
        high_precision_temperature: Option<u32>,
        #[serde(rename = "Temperature")]
        temperature: Option<u32>,
    }

    let com = COMLibrary::without_security().ok()?;
    let wmi = WMIConnection::new(com).ok()?;

    let results: Vec<ThermalZonePerf> = wmi
        .raw_query("SELECT Name, HighPrecisionTemperature, Temperature FROM Win32_PerfFormattedData_Counters_ThermalZoneInformation")
        .ok()?;

    // Prefer HighPrecisionTemperature (tenths of Kelvin) for accuracy
    // Fall back to Temperature (Kelvin)
    results
        .iter()
        .filter_map(|r| {
            if let Some(hp) = r.high_precision_temperature {
                Some((hp as f64 / 10.0) - 273.15)
            } else if let Some(t) = r.temperature {
                Some(t as f64 - 273.15)
            } else {
                None
            }
        })
        .filter(|t| *t > 0.0 && *t < 150.0) // sanity check
        .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
}

#[cfg(windows)]
fn read_cpu_temp_wmi() -> Option<f64> {
    use wmi::{COMLibrary, WMIConnection};
    use serde::Deserialize;

    #[derive(Deserialize, Debug)]
    #[allow(dead_code)]
    struct ThermalZone {
        #[serde(rename = "CurrentTemperature")]
        current_temperature: Option<u32>,
    }

    let com = COMLibrary::without_security().ok()?;
    let wmi = WMIConnection::with_namespace_path("root\\WMI", com).ok()?;

    let results: Vec<ThermalZone> = wmi
        .raw_query("SELECT CurrentTemperature FROM MSAcpi_ThermalZoneTemperature")
        .ok()?;

    // MSAcpi returns temperature in tenths of Kelvin
    results
        .iter()
        .filter_map(|r| r.current_temperature)
        .map(|t| (t as f64 / 10.0) - 273.15) // Convert from deciKelvin to Celsius
        .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
}

/// OpenHardwareMonitor / LibreHardwareMonitor GUI running with WMI publishing.
#[cfg(windows)]
fn read_cpu_temp_ohm() -> Option<(f64, String)> {
    use wmi::{COMLibrary, WMIConnection};
    use serde::Deserialize;

    #[derive(Deserialize, Debug)]
    #[allow(dead_code)]
    struct OhmSensor {
        #[serde(rename = "SensorType")]
        sensor_type: Option<String>,
        #[serde(rename = "Value")]
        value: Option<f32>,
        #[serde(rename = "Name")]
        name: Option<String>,
    }

    let com = COMLibrary::without_security().ok()?;

    // Try LibreHardwareMonitor first, then OpenHardwareMonitor
    for namespace in &[
        "root\\LibreHardwareMonitor",
        "root\\OpenHardwareMonitor",
    ] {
        let wmi = match WMIConnection::with_namespace_path(namespace, com) {
            Ok(w) => w,
            Err(_) => continue,
        };

        let results: Vec<OhmSensor> = wmi
            .raw_query("SELECT SensorType, Value, Name FROM Sensor WHERE SensorType='Temperature'")
            .unwrap_or_default();

        // Find CPU Package temperature (most representative), then any CPU temperature
        let package = results.iter().find(|s| {
            s.name
                .as_deref()
                .map(|n| n.contains("CPU Package") || n.contains("CPU (Tctl"))
                .unwrap_or(false)
        });
        let any_cpu = || results.iter().find(|s| {
            s.name
                .as_deref()
                .map(|n| n.to_lowercase().contains("cpu"))
                .unwrap_or(false)
        });
        if let Some(sensor) = package.or_else(any_cpu) {
            if let Some(v) = sensor.value {
                return Some((v as f64, sensor.name.clone().unwrap_or_default()));
            }
        }
    }

    None
}

#[cfg(windows)]
fn read_gpu_temp_wmi() -> Option<f64> {
    use wmi::{COMLibrary, WMIConnection};
    use serde::Deserialize;

    #[derive(Deserialize, Debug)]
    #[allow(dead_code)]
    struct GpuTemp {
        #[serde(rename = "CurrentTemperature")]
        current_temperature: Option<u32>,
    }

    let com = COMLibrary::without_security().ok()?;
    let wmi = WMIConnection::new(com).ok()?;

    let results: Vec<GpuTemp> = wmi
        .raw_query("SELECT CurrentTemperature FROM Win32_TemperatureProbe")
        .unwrap_or_default();

    results
        .iter()
        .filter_map(|r| r.current_temperature)
        .max()
        .map(|t| t as f64)
}

// ─── Linux ──────────────────────────────────────────────────────────

/// CPU package sensors first (x86_pkg_temp, coretemp, k10temp, zenpower);
/// thermal_zone0 — often the motherboard's ACPI zone — only as a last resort.
#[cfg(target_os = "linux")]
fn read_cpu_temp_linux() -> Option<TempReading> {
    use std::fs;

    for i in 0..16 {
        let base = format!("/sys/class/thermal/thermal_zone{}", i);
        let Ok(kind) = fs::read_to_string(format!("{}/type", base)) else { continue };
        let kind = kind.trim().to_lowercase();
        if kind.contains("cpu") || kind.contains("x86_pkg") || kind.contains("soc") {
            if let Some(t) = read_millideg(&format!("{}/temp", base)) {
                return TempReading::new(t, format!("thermal zone / {}", kind));
            }
        }
    }

    if let Ok(entries) = fs::read_dir("/sys/class/hwmon") {
        for entry in entries.flatten() {
            let name = fs::read_to_string(entry.path().join("name")).unwrap_or_default();
            let name = name.trim();
            if name == "coretemp" || name == "k10temp" || name == "zenpower" {
                // temp1 is Package (Intel) / Tctl (AMD)
                if let Some(t) = read_millideg(&entry.path().join("temp1_input").to_string_lossy()) {
                    return TempReading::new(t, format!("hwmon / {}", name));
                }
            }
        }
    }

    read_millideg("/sys/class/thermal/thermal_zone0/temp")
        .and_then(|t| TempReading::new(t, "thermal zone 0 (may be motherboard)"))
}

#[cfg(target_os = "linux")]
fn read_millideg(path: &str) -> Option<f64> {
    let text = std::fs::read_to_string(path).ok()?;
    let millideg: i64 = text.trim().parse().ok()?;
    Some(millideg as f64 / 1000.0)
}

/// temp1_input (edge) of the GPU at `device` (e.g. /sys/bus/pci/devices/0000:03:00.0)
#[cfg(target_os = "linux")]
fn read_hwmon_temp(device: &std::path::Path) -> Option<f64> {
    let entries = std::fs::read_dir(device.join("hwmon")).ok()?;
    for hwmon in entries.flatten() {
        if let Some(t) = read_millideg(&hwmon.path().join("temp1_input").to_string_lossy()) {
            return Some(t);
        }
    }
    None
}

/// First GPU hwmon temperature under /sys/class/drm (single-GPU systems).
#[cfg(target_os = "linux")]
fn read_gpu_temp_drm_any() -> Option<f64> {
    let entries = std::fs::read_dir("/sys/class/drm").ok()?;
    for entry in entries.flatten() {
        if let Some(t) = read_hwmon_temp(&entry.path().join("device")) {
            return Some(t);
        }
    }
    None
}

// ─── macOS ──────────────────────────────────────────────────────────

/// powermetrics needs root; `sudo -n` fails fast instead of prompting.
#[cfg(target_os = "macos")]
fn powermetrics_value(needles: &[&str]) -> Option<f64> {
    let output = std::process::Command::new("sudo")
        .args(["-n", "powermetrics", "-n", "1", "-i", "1000", "--samplers", "smc"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let lower = line.to_lowercase();
        if needles.iter().any(|n| lower.contains(n)) {
            for word in line.split_whitespace() {
                if let Ok(temp) = word.parse::<f64>() {
                    if temp > 0.0 && temp < 150.0 {
                        return Some(temp);
                    }
                }
            }
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn read_cpu_temp_powermetrics() -> Option<f64> {
    powermetrics_value(&["cpu die temperature", "cpu thermal level"])
}

#[cfg(target_os = "macos")]
fn read_gpu_temp_powermetrics() -> Option<f64> {
    powermetrics_value(&["gpu die temperature", "gpu thermal level"])
}

/// Some macOS versions expose a CPU temperature via sysctl.
#[cfg(target_os = "macos")]
fn read_cpu_temp_sysctl() -> Option<f64> {
    let output = std::process::Command::new("sysctl").arg("-a").output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if line.contains("temperature") && line.contains("CPU") {
            for word in line.split_whitespace() {
                if let Ok(temp) = word.parse::<f64>() {
                    if temp > 0.0 && temp < 150.0 {
                        return Some(temp);
                    }
                }
            }
        }
    }
    None
}

// ─── Debug Diagnostic Functions ─────────────────────────────────────

/// Debug: test WMI MSAcpi temperature method and return detailed result
#[cfg(windows)]
pub fn debug_read_cpu_temp_wmi() -> String {
    use wmi::{COMLibrary, WMIConnection};
    use serde::Deserialize;

    #[derive(Deserialize, Debug)]
    #[allow(dead_code)]
    struct ThermalZone {
        #[serde(rename = "CurrentTemperature")]
        current_temperature: Option<u32>,
    }

    let com = match COMLibrary::without_security() {
        Ok(c) => c,
        Err(e) => return format!("COM init failed: {}", e),
    };
    let wmi = match WMIConnection::with_namespace_path("root\\WMI", com) {
        Ok(w) => w,
        Err(e) => return format!("WMI root\\WMI connection failed: {} (requires admin)", e),
    };

    let results: Result<Vec<ThermalZone>, _> = wmi
        .raw_query("SELECT CurrentTemperature FROM MSAcpi_ThermalZoneTemperature");

    match results {
        Ok(zones) => {
            if zones.is_empty() {
                return "No thermal zones found".to_string();
            }
            let mut out = format!("Found {} zone(s):", zones.len());
            for (i, z) in zones.iter().enumerate() {
                if let Some(t) = z.current_temperature {
                    let celsius = (t as f64 / 10.0) - 273.15;
                    out.push_str(&format!(" Zone{}: {:.1}°C (raw: {})", i, celsius, t));
                } else {
                    out.push_str(&format!(" Zone{}: null", i));
                }
            }
            out
        }
        Err(e) => format!("Query failed: {}", e),
    }
}

/// Debug: test OHM/LHM WMI namespace method and return detailed result
#[cfg(windows)]
pub fn debug_read_cpu_temp_ohm() -> String {
    use wmi::{COMLibrary, WMIConnection};
    use serde::Deserialize;

    #[derive(Deserialize, Debug)]
    #[allow(dead_code)]
    struct OhmSensor {
        #[serde(rename = "SensorType")]
        sensor_type: Option<String>,
        #[serde(rename = "Value")]
        value: Option<f32>,
        #[serde(rename = "Name")]
        name: Option<String>,
    }

    let com = match COMLibrary::without_security() {
        Ok(c) => c,
        Err(e) => return format!("COM init failed: {}", e),
    };

    for namespace in &["root\\LibreHardwareMonitor", "root\\OpenHardwareMonitor"] {
        let wmi = match WMIConnection::with_namespace_path(namespace, com) {
            Ok(w) => w,
            Err(_) => continue,
        };

        let results: Vec<OhmSensor> = wmi
            .raw_query("SELECT SensorType, Value, Name FROM Sensor WHERE SensorType='Temperature'")
            .unwrap_or_default();

        if !results.is_empty() {
            let mut out = format!("{}: {} sensor(s) —", namespace, results.len());
            for s in &results {
                let name = s.name.as_deref().unwrap_or("?");
                let val = s.value.map(|v| format!("{:.1}°C", v)).unwrap_or("null".into());
                out.push_str(&format!(" [{}={}]", name, val));
            }
            return out;
        }
    }

    "No OHM/LHM WMI namespace available (neither LibreHardwareMonitor nor OpenHardwareMonitor running)".to_string()
}

/// Debug: test Performance Counter thermal zone method and return detailed result
#[cfg(windows)]
pub fn debug_read_cpu_temp_perfcounter() -> String {
    use wmi::{COMLibrary, WMIConnection};
    use serde::Deserialize;

    #[derive(Deserialize, Debug)]
    #[allow(dead_code)]
    struct ThermalZonePerf {
        #[serde(rename = "Name")]
        name: Option<String>,
        #[serde(rename = "HighPrecisionTemperature")]
        high_precision_temperature: Option<u32>,
        #[serde(rename = "Temperature")]
        temperature: Option<u32>,
    }

    let com = match COMLibrary::without_security() {
        Ok(c) => c,
        Err(e) => return format!("COM init failed: {}", e),
    };
    let wmi = match WMIConnection::new(com) {
        Ok(w) => w,
        Err(e) => return format!("WMI connection failed: {}", e),
    };

    let results: Result<Vec<ThermalZonePerf>, _> = wmi
        .raw_query("SELECT Name, HighPrecisionTemperature, Temperature FROM Win32_PerfFormattedData_Counters_ThermalZoneInformation");

    match results {
        Ok(zones) => {
            if zones.is_empty() {
                return "No performance counter thermal zones found".to_string();
            }
            let mut out = format!("Found {} zone(s):", zones.len());
            for z in &zones {
                let name = z.name.as_deref().unwrap_or("?");
                if let Some(hp) = z.high_precision_temperature {
                    let celsius = (hp as f64 / 10.0) - 273.15;
                    out.push_str(&format!(" [{}={:.1}°C (HP: {})]", name, celsius, hp));
                } else if let Some(t) = z.temperature {
                    let celsius = t as f64 - 273.15;
                    out.push_str(&format!(" [{}={:.1}°C (raw: {})]", name, celsius, t));
                } else {
                    out.push_str(&format!(" [{}=null]", name));
                }
            }
            out
        }
        Err(e) => format!("Query failed: {}", e),
    }
}