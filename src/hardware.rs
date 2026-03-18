use sysinfo::System;

#[derive(Debug, Clone)]
pub struct HardwareInfo {
    pub cpu_model: Option<String>,
    pub cpu_cores: Option<i32>,
    pub gpu_model: Option<String>,
    pub gpu_vram: Option<String>,
    pub os: Option<String>,
}

pub fn detect_hardware() -> HardwareInfo {
    let mut sys = System::new_all();
    sys.refresh_all();

    // CPU info — normalize the raw brand string
    let cpu_model = sys.cpus().first()
        .map(|cpu| normalize_cpu_name(cpu.brand().trim()));
    let cpu_cores = Some(num_cpus::get_physical() as i32);

    // OS info
    let os = Some(format!(
        "{} {}",
        System::name().unwrap_or_default(),
        System::os_version().unwrap_or_default()
    ));

    // GPU detection (platform-specific)
    let (gpu_model, gpu_vram) = detect_gpu();

    HardwareInfo {
        cpu_model,
        cpu_cores,
        gpu_model,
        gpu_vram,
        os,
    }
}

/// Normalize CPU model name to a consistent format.
///
/// Examples:
///   "13th Gen Intel(R) Core(TM) i9-13950HX" → "Intel Core i9-13950HX"
///   "Intel(R) Core(TM) i7-14700K CPU @ 3.40GHz" → "Intel Core i7-14700K"
///   "AMD Ryzen 9 7950X 16-Core Processor" → "AMD Ryzen 9 7950X"
fn normalize_cpu_name(raw: &str) -> String {
    let s = raw.trim();

    // Intel: extract "i3/i5/i7/i9-XXXXX" or "Ultra N XXXXX" from noisy string
    if let Some(caps) = regex_find_intel(s) {
        return format!("Intel Core {}", caps);
    }

    // AMD Ryzen: extract tier + model, drop "N-Core Processor" suffix
    if let Some((tier, model)) = regex_find_amd_ryzen(s) {
        return format!("AMD Ryzen {} {}", tier, model);
    }

    // AMD Threadripper
    if let Some(model) = regex_find_threadripper(s) {
        return format!("AMD Ryzen Threadripper {}", model);
    }

    // General cleanup for anything else
    let mut result = s.to_string();
    result = result.replace("(R)", "").replace("(TM)", "").replace("(tm)", "");
    // Remove "Nth Gen " prefix
    if let Some(pos) = result.find("Gen ") {
        result = result[pos + 4..].to_string();
    }
    // Remove trailing " N-Core Processor" or " CPU @ ..."
    if let Some(pos) = result.find(" CPU @ ") {
        result = result[..pos].to_string();
    }
    if let Some(pos) = result.find("-Core Processor") {
        // Walk back to find the space before the core count
        if let Some(space) = result[..pos].rfind(' ') {
            result = result[..space].to_string();
        }
    }
    result = result.replace("  ", " ").trim().to_string();
    result
}

fn regex_find_intel(s: &str) -> Option<String> {
    // Look for "Core ... iN-XXXXX" or "Core ... Ultra N XXXXX"
    // Handle messy strings like "13th Gen Intel(R) Core(TM) i9-13950HX"
    let s_clean = s.replace("(R)", "").replace("(TM)", "");

    // Try "Ultra N XXXXX" pattern first (Arrow Lake+)
    if let Some(ultra_pos) = s_clean.find("Ultra") {
        let after = s_clean[ultra_pos..].trim();
        let parts: Vec<&str> = after.split_whitespace().collect();
        // "Ultra 9 285K" or "Ultra 7 265K"
        if parts.len() >= 3 {
            let model = parts[..3].join(" ");
            // Strip any trailing junk
            let model = model.split(" CPU").next().unwrap_or(&model);
            return Some(model.to_string());
        }
    }

    // Try "iN-XXXXX" pattern (traditional Core)
    for part in s_clean.split_whitespace() {
        if (part.starts_with("i3-") || part.starts_with("i5-") ||
            part.starts_with("i7-") || part.starts_with("i9-")) && part.len() >= 5 {
            // Take just the iN-XXXXX part, strip trailing qualifiers
            let model = part.split(|c: char| c == ' ' || c == ',').next().unwrap_or(part);
            return Some(model.to_string());
        }
    }

    None
}

fn regex_find_amd_ryzen(s: &str) -> Option<(String, String)> {
    let s_lower = s.to_lowercase();
    if !s_lower.contains("ryzen") || s_lower.contains("threadripper") {
        return None;
    }

    let parts: Vec<&str> = s.split_whitespace().collect();
    // Find "Ryzen" then take the next two tokens (tier + model)
    for (i, part) in parts.iter().enumerate() {
        if part.eq_ignore_ascii_case("Ryzen") && i + 2 < parts.len() {
            let tier = parts[i + 1].to_string(); // "9", "7", "5", "3"
            let model = parts[i + 2].to_string(); // "7950X", "5800X3D"
            return Some((tier, model));
        }
    }
    None
}

fn regex_find_threadripper(s: &str) -> Option<String> {
    let s_lower = s.to_lowercase();
    if !s_lower.contains("threadripper") {
        return None;
    }

    let parts: Vec<&str> = s.split_whitespace().collect();
    for (i, part) in parts.iter().enumerate() {
        if part.eq_ignore_ascii_case("Threadripper") && i + 1 < parts.len() {
            return Some(parts[i + 1].to_string());
        }
    }
    None
}

#[cfg(windows)]
fn detect_gpu() -> (Option<String>, Option<String>) {
    // Use WMI to query Win32_VideoController for discrete GPU info
    use wmi::{COMLibrary, WMIConnection};
    use serde::Deserialize;

    #[derive(Deserialize, Debug)]
    #[serde(rename_all = "PascalCase")]
    #[allow(dead_code)]
    struct VideoController {
        name: Option<String>,
        adapter_r_a_m: Option<u64>,
        adapter_compatibility: Option<String>,
    }

    let com = match COMLibrary::new() {
        Ok(c) => c,
        Err(_) => return (None, None),
    };
    let wmi = match WMIConnection::new(com) {
        Ok(w) => w,
        Err(_) => return (None, None),
    };

    let controllers: Vec<VideoController> = wmi
        .raw_query("SELECT Name, AdapterRAM, AdapterCompatibility FROM Win32_VideoController")
        .unwrap_or_default();

    // Prefer discrete GPU (NVIDIA, AMD) over integrated (Intel)
    let discrete = controllers
        .iter()
        .find(|c| {
            let compat = c.adapter_compatibility.as_deref().unwrap_or("");
            let name = c.name.as_deref().unwrap_or("");
            compat.contains("NVIDIA")
                || compat.contains("AMD")
                || compat.contains("ATI")
                || name.contains("NVIDIA")
                || name.contains("Radeon")
                || name.contains("GeForce")
        });

    let gpu = discrete.or(controllers.first());

    match gpu {
        Some(vc) => {
            let name = vc.name.clone();
            // AdapterRAM is a 32-bit uint in WMI, overflows at 4 GB.
            // Use nvidia-smi for NVIDIA, rocm-smi for AMD, or WMI fallback.
            let vram = get_vram_nvidia_smi()
                .or_else(get_vram_rocm_smi)
                .or_else(|| {
                    vc.adapter_r_a_m.and_then(|bytes| {
                        // If exactly 4 GB (0xFFFFFFFF or 0x100000000), it's likely overflow
                        if bytes >= 0xFFFF_FFFF {
                            return None;
                        }
                        let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                        if gb >= 1.0 {
                            Some(format!("{:.0} GB", gb))
                        } else {
                            let mb = bytes as f64 / (1024.0 * 1024.0);
                            Some(format!("{:.0} MB", mb))
                        }
                    })
                });
            (name, vram)
        }
        None => (None, None),
    }
}

/// Get VRAM via nvidia-smi (accurate, no 4 GB overflow issue)
fn get_vram_nvidia_smi() -> Option<String> {
    let output = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mb: f64 = stdout.trim().lines().next()?.trim().parse().ok()?;
    let gb = mb / 1024.0;
    if gb >= 1.0 {
        Some(format!("{:.0} GB", gb))
    } else {
        Some(format!("{:.0} MB", mb))
    }
}

/// Get VRAM via rocm-smi for AMD GPUs
#[allow(dead_code)]
fn get_vram_rocm_smi() -> Option<String> {
    let output = std::process::Command::new("rocm-smi")
        .args(["--showmeminfo", "vram"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    // rocm-smi outputs lines like "GPU[0] : vram Total Memory (B): 8589934592"
    for line in stdout.lines() {
        let lower = line.to_lowercase();
        if lower.contains("total") && lower.contains("vram") {
            // Extract the byte value from the end of the line
            if let Some(bytes_str) = line.split(':').last() {
                if let Ok(bytes) = bytes_str.trim().parse::<u64>() {
                    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                    if gb >= 1.0 {
                        return Some(format!("{:.0} GB", gb));
                    } else {
                        let mb = bytes as f64 / (1024.0 * 1024.0);
                        return Some(format!("{:.0} MB", mb));
                    }
                }
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn detect_gpu() -> (Option<String>, Option<String>) {
    use std::process::Command;

    let output = Command::new("lspci")
        .output()
        .ok();

    let mut gpu_name = None;

    if let Some(output) = output {
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Look for VGA/3D controller lines
        for line in stdout.lines() {
            let lower = line.to_lowercase();
            if (lower.contains("vga") || lower.contains("3d controller"))
                && (lower.contains("nvidia") || lower.contains("amd") || lower.contains("radeon"))
            {
                if let Some(pos) = line.find(": ") {
                    gpu_name = Some(line[pos + 2..].trim().to_string());
                    break;
                }
            }
        }
        // Fallback to first VGA device
        if gpu_name.is_none() {
            for line in stdout.lines() {
                if line.to_lowercase().contains("vga") {
                    if let Some(pos) = line.find(": ") {
                        gpu_name = Some(line[pos + 2..].trim().to_string());
                        break;
                    }
                }
            }
        }
    }

    let vram = get_vram_nvidia_smi().or_else(get_vram_rocm_smi);
    (gpu_name, vram)
}

#[cfg(target_os = "macos")]
fn detect_gpu() -> (Option<String>, Option<String>) {
    use std::process::Command;

    // Use system_profiler to detect GPU on macOS
    let output = Command::new("system_profiler")
        .args(["SPDisplaysDataType", "-json"])
        .output()
        .ok();

    if let Some(output) = output {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&stdout) {
                // Navigate: SPDisplaysDataType[0].sppci_model and spdisplays_vram
                if let Some(displays) = json.get("SPDisplaysDataType").and_then(|d| d.as_array()) {
                    for display in displays {
                        let name = display.get("sppci_model")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        let vram = display.get("spdisplays_vram")
                            .or_else(|| display.get("spdisplays_vram_shared"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        if name.is_some() {
                            return (name, vram);
                        }
                    }
                }
            }
        }
    }

    // Fallback: try plain text output
    let output = Command::new("system_profiler")
        .args(["SPDisplaysDataType"])
        .output()
        .ok();

    if let Some(output) = output {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let mut name = None;
            let mut vram = None;
            for line in stdout.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("Chipset Model:") {
                    name = trimmed.strip_prefix("Chipset Model:").map(|s| s.trim().to_string());
                }
                if trimmed.starts_with("VRAM") || trimmed.starts_with("Total Number of Cores") {
                    if let Some(val) = trimmed.split(':').last() {
                        vram = Some(val.trim().to_string());
                    }
                }
            }
            if name.is_some() {
                return (name, vram);
            }
        }
    }

    (None, None)
}
