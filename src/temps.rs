use colored::Colorize;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct TemperatureReading {
    pub cpu_temp: Option<f64>,
    pub gpu_temp: Option<f64>,
}

/// Read system temperatures.
/// If an LHM directory is provided, uses the embedded LibreHardwareMonitor
/// for accurate CPU die temps. Falls back to WMI/PerfCounter otherwise.
/// GPU temps prefer nvidia-smi for NVIDIA, LHM for AMD, sysfs on Linux, powermetrics on macOS.
pub fn read_temperatures_with_lhm(lhm_dir: Option<&PathBuf>) -> TemperatureReading {
    let mut cpu_temp = None;
    let mut _lhm_gpu_temp: Option<f64> = None;

    // Try LHM first for CPU die temperature (requires admin, Windows only)
    #[cfg(windows)]
    if let Some(dir) = lhm_dir {
        if let Some(reading) = crate::lhm::read_temps(dir) {
            cpu_temp = reading.cpu_temp;
            _lhm_gpu_temp = reading.gpu_temp;
        }
    }

    // Suppress unused variable warning on non-Windows
    #[cfg(not(windows))]
    let _ = lhm_dir;

    // Fall back to platform-specific methods if LHM didn't return CPU temp
    if cpu_temp.is_none() {
        cpu_temp = read_cpu_temp();
    }

    // GPU temp: prefer nvidia-smi, fall back to LHM (AMD GPUs on Windows) or sysfs (Linux)
    let gpu_temp = read_gpu_temp();

    // If nvidia-smi didn't work, try LHM GPU temp (covers AMD/Intel GPUs on Windows)
    #[cfg(windows)]
    let gpu_temp = if gpu_temp.is_none() {
        _lhm_gpu_temp
    } else {
        gpu_temp
    };

    TemperatureReading { cpu_temp, gpu_temp }
}

// ─── CPU Temperature ────────────────────────────────────────────────

#[cfg(windows)]
fn read_cpu_temp() -> Option<f64> {
    // Strategy 1: WMI MSAcpi_ThermalZoneTemperature (requires admin on most systems)
    if let Some(temp) = read_cpu_temp_wmi() {
        return Some(temp);
    }

    // Strategy 2: Open Hardware Monitor / LibreHardwareMonitor WMI namespace
    if let Some(temp) = read_cpu_temp_ohm() {
        return Some(temp);
    }

    // Strategy 3: Performance Counter thermal zones (works WITHOUT admin)
    if let Some(temp) = read_cpu_temp_perfcounter() {
        return Some(temp);
    }

    None
}

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

#[cfg(windows)]
fn read_cpu_temp_ohm() -> Option<f64> {
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

        // Find CPU Package temperature (most representative)
        if let Some(sensor) = results.iter().find(|s| {
            s.name
                .as_deref()
                .map(|n| n.contains("CPU Package") || n.contains("CPU (Tctl"))
                .unwrap_or(false)
        }) {
            if let Some(v) = sensor.value {
                return Some(v as f64);
            }
        }

        // Fallback: any CPU temperature
        if let Some(sensor) = results.iter().find(|s| {
            s.name
                .as_deref()
                .map(|n| n.to_lowercase().contains("cpu"))
                .unwrap_or(false)
        }) {
            if let Some(v) = sensor.value {
                return Some(v as f64);
            }
        }
    }

    None
}

#[cfg(target_os = "linux")]
fn read_cpu_temp() -> Option<f64> {
    // Strategy 1: /sys/class/thermal/ (most Linux systems)
    if let Some(temp) = read_cpu_temp_sysfs() {
        return Some(temp);
    }

    // Strategy 2: /sys/class/hwmon/ (coretemp, k10temp)
    if let Some(temp) = read_cpu_temp_hwmon() {
        return Some(temp);
    }

    println!(
        "  {} Could not read CPU temperature. Try installing {}.",
        "⚠".yellow(),
        "lm-sensors".cyan()
    );
    None
}

#[cfg(target_os = "macos")]
fn read_cpu_temp() -> Option<f64> {
    // Try powermetrics first (requires sudo, most accurate)
    if let Some(temp) = read_cpu_temp_powermetrics() {
        return Some(temp);
    }

    // Fallback: try reading via IOKit SMC keys (works without sudo on some systems)
    if let Some(temp) = read_cpu_temp_smc() {
        return Some(temp);
    }

    println!(
        "  {} Could not read CPU temperature. Try running with {}.",
        "⚠".yellow(),
        "sudo".cyan()
    );
    None
}

/// Read CPU temperature via macOS powermetrics (requires sudo)
#[cfg(target_os = "macos")]
fn read_cpu_temp_powermetrics() -> Option<f64> {
    use std::process::Command;

    // powermetrics requires root, sample for 1 second
    let output = Command::new("sudo")
        .args(["-n", "powermetrics", "-n", "1", "-i", "1000", "--samplers", "smc"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Look for CPU die temperature line like: "CPU die temperature: 45.31 C"
    for line in stdout.lines() {
        let lower = line.to_lowercase();
        if lower.contains("cpu die temperature") || lower.contains("cpu thermal level") {
            // Extract the numeric value
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

/// Read CPU temperature via IOKit SMC (may work without sudo on some macOS versions)
#[cfg(target_os = "macos")]
fn read_cpu_temp_smc() -> Option<f64> {
    use std::process::Command;

    // Try using osx-cpu-temp if installed, or read from sysctl
    let output = Command::new("sysctl")
        .args(["-a"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // Some macOS versions expose temperature via sysctl
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

#[cfg(target_os = "linux")]
fn read_cpu_temp_sysfs() -> Option<f64> {
    use std::fs;

    // Check thermal zones
    for i in 0..10 {
        let type_path = format!("/sys/class/thermal/thermal_zone{}/type", i);
        let temp_path = format!("/sys/class/thermal/thermal_zone{}/temp", i);

        if let (Ok(zone_type), Ok(temp_str)) = (fs::read_to_string(&type_path), fs::read_to_string(&temp_path)) {
            let zone_type = zone_type.trim().to_lowercase();
            if zone_type.contains("cpu") || zone_type.contains("x86_pkg") || zone_type.contains("soc") {
                if let Ok(millideg) = temp_str.trim().parse::<i64>() {
                    return Some(millideg as f64 / 1000.0);
                }
            }
        }
    }

    // Fallback: just use thermal_zone0 which is often CPU
    let temp_path = "/sys/class/thermal/thermal_zone0/temp";
    if let Ok(temp_str) = std::fs::read_to_string(temp_path) {
        if let Ok(millideg) = temp_str.trim().parse::<i64>() {
            return Some(millideg as f64 / 1000.0);
        }
    }

    None
}

#[cfg(target_os = "linux")]
fn read_cpu_temp_hwmon() -> Option<f64> {
    use std::fs;

    // Search hwmon devices for coretemp or k10temp
    let hwmon_dir = "/sys/class/hwmon";
    let entries = fs::read_dir(hwmon_dir).ok()?;

    for entry in entries.flatten() {
        let name_path = entry.path().join("name");
        if let Ok(name) = fs::read_to_string(&name_path) {
            let name = name.trim();
            if name == "coretemp" || name == "k10temp" || name == "zenpower" {
                // Read temp1_input (Package/Tdie temperature)
                let temp_path = entry.path().join("temp1_input");
                if let Ok(temp_str) = fs::read_to_string(&temp_path) {
                    if let Ok(millideg) = temp_str.trim().parse::<i64>() {
                        return Some(millideg as f64 / 1000.0);
                    }
                }
            }
        }
    }

    None
}

// ─── GPU Temperature ────────────────────────────────────────────────

fn read_gpu_temp() -> Option<f64> {
    // Strategy 1: nvidia-smi (NVIDIA GPUs)
    if let Some(temp) = read_gpu_temp_nvidia_smi() {
        return Some(temp);
    }

    // Strategy 2: On Windows, try WMI
    #[cfg(windows)]
    if let Some(temp) = read_gpu_temp_wmi() {
        return Some(temp);
    }

    // Strategy 3: On Linux, try /sys/class/drm for AMD
    #[cfg(target_os = "linux")]
    if let Some(temp) = read_gpu_temp_amd_sysfs() {
        return Some(temp);
    }

    // Strategy 4: On macOS, try powermetrics for GPU temp
    #[cfg(target_os = "macos")]
    if let Some(temp) = read_gpu_temp_macos() {
        return Some(temp);
    }

    println!(
        "  {} Could not read GPU temperature. Ensure GPU drivers are installed.",
        "⚠".yellow()
    );
    None
}

fn read_gpu_temp_nvidia_smi() -> Option<f64> {
    // Try hotspot temperature first (RTX 30-series and newer)
    let output = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=temperature.gpu_hotspot", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;

    if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(temp) = stdout.trim().lines().next().and_then(|l| l.trim().parse::<f64>().ok()) {
            if temp > 0.0 && temp < 150.0 {
                return Some(temp);
            }
        }
    }

    // Fall back to core/edge temperature
    let output = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=temperature.gpu", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.trim().lines().next()?.trim().parse::<f64>().ok()
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

#[cfg(target_os = "linux")]
fn read_gpu_temp_amd_sysfs() -> Option<f64> {
    use std::fs;

    // AMD GPUs expose temp through hwmon under /sys/class/drm
    let drm_dir = "/sys/class/drm";
    let entries = fs::read_dir(drm_dir).ok()?;

    for entry in entries.flatten() {
        let device_path = entry.path().join("device/hwmon");
        if let Ok(hwmon_entries) = fs::read_dir(&device_path) {
            for hwmon in hwmon_entries.flatten() {
                let temp_path = hwmon.path().join("temp1_input");
                if let Ok(temp_str) = fs::read_to_string(&temp_path) {
                    if let Ok(millideg) = temp_str.trim().parse::<i64>() {
                        return Some(millideg as f64 / 1000.0);
                    }
                }
            }
        }
    }

    None
}

/// Read GPU temperature on macOS via powermetrics
#[cfg(target_os = "macos")]
fn read_gpu_temp_macos() -> Option<f64> {
    use std::process::Command;

    let output = Command::new("sudo")
        .args(["-n", "powermetrics", "-n", "1", "-i", "1000", "--samplers", "smc"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let lower = line.to_lowercase();
        if lower.contains("gpu die temperature") || lower.contains("gpu thermal level") {
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