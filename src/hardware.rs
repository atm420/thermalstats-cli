use crate::gpus::GpuDevice;
use sysinfo::System;

#[derive(Debug, Clone)]
pub struct HardwareInfo {
    pub cpu_model: Option<String>,
    pub cpu_cores: Option<i32>,
    pub cpu_threads: Option<i32>,
    pub os: Option<String>,
    pub is_laptop: bool,
    /// Every physical GPU, discrete cards first.
    pub gpus: Vec<GpuDevice>,
}

pub fn detect_hardware() -> HardwareInfo {
    let mut sys = System::new();
    sys.refresh_cpu_all();

    // CPU info — normalize the raw brand string
    let cpu_model = sys.cpus().first()
        .map(|cpu| normalize_cpu_name(cpu.brand().trim()))
        .filter(|name| !name.is_empty());
    let cpu_cores = Some(num_cpus::get_physical() as i32);
    let cpu_threads = Some(num_cpus::get() as i32);

    // OS info
    let os = Some(format!(
        "{} {}",
        System::name().unwrap_or_default(),
        System::os_version().unwrap_or_default()
    ));

    let gpus = crate::gpus::detect();
    let gpu_names: Vec<&str> = gpus.iter().map(|g| g.name.as_str()).collect();
    let is_laptop = detect_is_laptop(cpu_model.as_deref(), &gpu_names);

    HardwareInfo {
        cpu_model,
        cpu_cores,
        cpu_threads,
        os,
        is_laptop,
        gpus,
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

/// Detect whether the system is a laptop based on CPU and GPU names.
///
/// Laptop indicators:
/// - CPU: AMD mobile suffixes (H, HX, HS, U), Intel mobile suffixes (H, HX, HK),
///   Intel Core Ultra mobile (H suffix), Apple Silicon (always laptop-capable)
/// - GPU: "Laptop GPU" in name, AMD mobile GPU suffixes (M, S)
/// - System: a battery is present
fn detect_is_laptop(cpu: Option<&str>, gpus: &[&str]) -> bool {
    // GPU-based detection (most reliable)
    for g in gpus {
        let gl = g.to_lowercase();
        if gl.contains("laptop gpu") {
            return true;
        }
        // AMD mobile GPUs: RX 7900M, RX 7700S, RX 7600M XT, etc.
        if gl.contains("radeon") {
            for part in g.split_whitespace() {
                let p = part.trim_end_matches(|c: char| c == ',' || c == ')');
                if p.len() >= 4 && (p.ends_with('M') || p.ends_with('S'))
                    && p[..p.len() - 1].chars().all(|c| c.is_ascii_digit())
                {
                    return true;
                }
            }
        }
    }

    // CPU-based detection
    if let Some(c) = cpu {
        // AMD Ryzen mobile: H, HX, HS, U suffixes after 4-digit model number
        // e.g. "AMD Ryzen 9 7945HX", "AMD Ryzen 7 7840U"
        if c.contains("Ryzen") {
            for part in c.split_whitespace() {
                if part.len() >= 5 {
                    let suffix = &part[part.len().saturating_sub(2)..];
                    if suffix == "HX" || suffix == "HS" {
                        return true;
                    }
                }
                if part.len() >= 5 && (part.ends_with('H') || part.ends_with('U')) {
                    // Check the preceding chars are digits (model number)
                    let prefix = &part[..part.len()-1];
                    if prefix.chars().last().map_or(false, |c| c.is_ascii_digit()) {
                        return true;
                    }
                }
            }
        }

        // Intel mobile: i7-13700H, i9-13950HX, i5-12500H, etc.
        if c.contains("Intel") {
            for part in c.split_whitespace() {
                if part.contains('-') && (part.ends_with('H') || part.ends_with("HX") || part.ends_with("HK")) {
                    return true;
                }
            }
            // Intel Core Ultra mobile: "185H", "165H", etc.
            if c.contains("Ultra") {
                for part in c.split_whitespace() {
                    if part.ends_with('H') && part.len() >= 3 {
                        let num_part = &part[..part.len()-1];
                        if num_part.chars().all(|c| c.is_ascii_digit()) {
                            return true;
                        }
                    }
                }
            }
        }

        // Apple Silicon — all are laptop-capable, detect via macOS
        if c.starts_with("Apple M") {
            #[cfg(target_os = "macos")]
            return true;
        }
    }

    // Platform-level battery detection (fallback)
    crate::platform::has_battery()
}
