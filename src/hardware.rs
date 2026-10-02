use crate::gpus::GpuDevice;
use sysinfo::System;

#[derive(Debug, Clone)]
pub struct HardwareInfo {
    pub cpu_model: Option<String>,
    pub cpu_cores: Option<i32>,
    pub cpu_threads: Option<i32>,
    pub os: Option<String>,
    pub is_laptop: bool,
    /// The model as the BIOS names it, e.g. "Lenovo Legion 5 15ACH6H":
    /// prefills the laptop model field.
    pub system_model: Option<String>,
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
    let system_model = crate::platform::system_identity()
        .and_then(|id| laptop_model_name(&id.vendor, &id.product, &id.version, &id.family));

    HardwareInfo {
        cpu_model,
        cpu_cores,
        cpu_threads,
        os,
        is_laptop,
        system_model,
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
/// - GPU: "Laptop GPU" in name, AMD mobile GPU suffixes (M, S)
/// - CPU: a laptop-class chip (see `is_mobile_cpu`), Apple Silicon on macOS
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

    if let Some(c) = cpu {
        if is_mobile_cpu(c) {
            return true;
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

/// A laptop-class CPU, from its (normalized) name: a mobile part, or a
/// soldered low-power chip from a mini PC or handheld. None of these take a
/// tower cooler. Mirrors isMobileCpu in the website's src/lib/cooling-evidence.ts.
fn is_mobile_cpu(cpu: &str) -> bool {
    let words: Vec<&str> = cpu.split_whitespace().collect();
    let split_digits = |w: &str| {
        let n = w.chars().take_while(|c| c.is_ascii_digit()).count();
        (w[..n].to_string(), w[n..].to_string())
    };

    if cpu.contains("Ryzen AI") || words.contains(&"Atom") {
        return true;
    }
    for w in &words {
        // Intel Core: "i7-13700H", "i5-1135G7", "i7-7700HQ", "i3-N305"
        if let Some(model) = ["i3-", "i5-", "i7-", "i9-"].iter().find_map(|p| w.strip_prefix(p)) {
            if model.starts_with('N') {
                return true;
            }
            let (digits, suffix) = split_digits(model);
            if digits.len() < 4 {
                continue;
            }
            let g_series = suffix.len() == 2 && suffix.starts_with('G') && suffix[1..].chars().all(|c| c.is_ascii_digit());
            // "P" is mobile on 12th/13th gen (i7-1260P) but desktop on old chips (i5-3350P)
            let p_series = suffix == "P" && digits.len() == 4 && digits.starts_with('1');
            if g_series || p_series || ["H", "HX", "HK", "HQ", "HS", "MQ", "QM", "M", "U", "Y"].contains(&suffix.as_str()) {
                return true;
            }
        }
    }
    // Intel Core Ultra mobile: "Intel Core Ultra 7 155H", "258V"
    if cpu.contains("Ultra") {
        if let Some(last) = words.last() {
            let (digits, suffix) = split_digits(last);
            if digits.len() == 3 && ["H", "HX", "U", "V"].contains(&suffix.as_str()) {
                return true;
            }
        }
    }
    // Intel N-series: "Intel N95", "Intel N150"
    if words.first() == Some(&"Intel")
        && words.get(1).is_some_and(|w| w.len() >= 3 && w.starts_with('N') && w[1..].chars().all(|c| c.is_ascii_digit()))
    {
        return true;
    }
    // Celeron / Pentium mobile and embedded: "N4500", "J1800", "1007U"
    if cpu.contains("Celeron") || cpu.contains("Pentium") {
        for w in &words {
            let (digits, suffix) = split_digits(w);
            let nj = w.len() == 5 && (w.starts_with('N') || w.starts_with('J')) && w[1..].chars().all(|c| c.is_ascii_digit());
            if nj || (digits.len() == 4 && (suffix == "U" || suffix == "Y")) {
                return true;
            }
        }
    }
    if cpu.contains("Ryzen") {
        for w in &words {
            // Ryzen mobile: "7840HS", "7945HX", "5800H", "7530U"; handhelds: "Z1"
            let (digits, suffix) = split_digits(w);
            if digits.len() == 4 && ["H", "HS", "HX", "U"].contains(&suffix.as_str()) {
                return true;
            }
            if w.len() >= 2 && w.starts_with('Z') && w[1..].chars().all(|c| c.is_ascii_digit()) {
                return true;
            }
        }
    }
    false
}

/// What the BIOS (SMBIOS / DMI) says about the machine.
#[derive(Debug, Clone, Default)]
pub struct SystemIdentity {
    pub vendor: String,
    pub product: String,
    pub version: String,
    pub family: String,
}

/// A readable laptop model from the BIOS fields, e.g. "Lenovo Legion 5
/// 15ACH6H" or "ASUS TUF Gaming A15 FA506IH". None when the BIOS only has
/// placeholders. The website cleans names further, so this only needs to be
/// recognisable.
pub fn laptop_model_name(vendor: &str, product: &str, version: &str, family: &str) -> Option<String> {
    let clean = |s: &str| {
        let s = s.trim();
        let lower = s.to_lowercase();
        const PLACEHOLDERS: [&str; 13] = [
            "to be filled", "system product", "system version", "system manufacturer", "default string",
            "not applicable", "not specified", "o.e.m", "invalid", "type1", "all series", "none", "123456789",
        ];
        let junk = s.is_empty() || !s.chars().any(|c| c.is_alphabetic()) || PLACEHOLDERS.iter().any(|p| lower.contains(p));
        (!junk).then(|| s.to_string())
    };

    let vendor_lower = vendor.to_lowercase();
    let brand = [
        ("lenovo", "Lenovo"), ("asus", "ASUS"), ("hewlett", "HP"), ("dell", "Dell"), ("acer", "Acer"),
        ("micro-star", "MSI"), ("razer", "Razer"), ("gigabyte", "Gigabyte"), ("microsoft", "Microsoft"),
        ("samsung", "Samsung"), ("huawei", "Huawei"), ("xiaomi", "Xiaomi"), ("alienware", "Alienware"),
        ("framework", "Framework"), ("toshiba", "Toshiba"), ("dynabook", "Dynabook"), ("lg electronics", "LG"),
    ]
    .iter()
    .find(|(key, _)| vendor_lower.contains(key))
    .map(|(_, name)| name.to_string())
    .or_else(|| if vendor_lower.trim() == "hp" || vendor_lower.trim() == "msi" { Some(vendor.trim().to_uppercase()) } else { None })
    .or_else(|| clean(vendor));

    // Lenovo puts a machine-type code ("82JW") in the product name and the
    // model ("Legion 5 15ACH6H") in the version or family.
    let model = if vendor_lower.contains("lenovo") {
        clean(version).or_else(|| clean(family)).or_else(|| clean(product))
    } else {
        clean(product)
    }?;
    // ASUS repeats the code: "TUF Gaming A15 FA506IH_FA506IH"
    let model = match model.split_once('_') {
        Some((a, b)) if b.trim() == a.split_whitespace().last().unwrap_or("") => a.trim().to_string(),
        _ => model.replace('_', " "),
    };

    let name = match brand {
        Some(b) if !model.to_lowercase().starts_with(&b.to_lowercase()) => format!("{} {}", b, model),
        _ => model,
    };
    Some(name.chars().take(120).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn laptop_class_cpus_are_recognised() {
        for cpu in [
            "Intel Core i7-13700H",
            "Intel Core i9-13950HX",
            "Intel Core i5-1135G7",
            "Intel Core i7-7700HQ",
            "Intel Core i7-4710MQ",
            "Intel Core i5-3210M",
            "Intel Core i3-1215U",
            "Intel Core i3-3217U",
            "Intel Core i7-1260P",
            "Intel Core i3-N305",
            "Intel Core Ultra 7 155H",
            "Intel Core Ultra 7 258V",
            "Intel N95",
            "Intel N150",
            "Intel Celeron N4500 @ 1.10GHz",
            "Intel Celeron CPU J1800 @ 2.41GHz",
            "Intel Celeron CPU 1007U @ 1.50GHz",
            "Intel Pentium Silver N5030",
            "AMD Ryzen 7 5800H",
            "AMD Ryzen 9 7945HX",
            "AMD Ryzen 7 7840HS",
            "AMD Ryzen 5 7530U",
            "AMD Ryzen Z1 Extreme",
            "AMD Ryzen AI 9 HX 370",
        ] {
            assert!(is_mobile_cpu(cpu), "{cpu}");
        }
        for cpu in [
            "Intel Core i5-12400F",
            "Intel Core i7-14700K",
            "Intel Core i9-13900KS",
            "Intel Core i7-8700T",
            "Intel Core i5-3350P",
            "Intel Core i5-12400",
            "Intel Core Ultra 9 285K",
            "Intel Pentium Gold G6400",
            "Intel Celeron G5905",
            "AMD Ryzen 5 5600X",
            "AMD Ryzen 5 5600G",
            "AMD Ryzen 7 9800X3D",
        ] {
            assert!(!is_mobile_cpu(cpu), "{cpu}");
        }
    }

    #[test]
    fn laptop_model_comes_from_the_bios() {
        let m = |v: &str, p: &str, ver: &str, fam: &str| laptop_model_name(v, p, ver, fam);
        assert_eq!(m("LENOVO", "82JW", "Legion 5 15ACH6H", "Legion 5 15ACH6H").as_deref(), Some("Lenovo Legion 5 15ACH6H"));
        assert_eq!(m("LENOVO", "82JW", "", "IdeaPad Gaming 3 15ACH6").as_deref(), Some("Lenovo IdeaPad Gaming 3 15ACH6"));
        assert_eq!(
            m("ASUSTeK COMPUTER INC.", "TUF Gaming FA506IH_FA506IH", "1.0", "TUF Gaming").as_deref(),
            Some("ASUS TUF Gaming FA506IH")
        );
        assert_eq!(m("ASUSTeK COMPUTER INC.", "ROG Zephyrus G14 GA402RJ", "1.0", "").as_deref(), Some("ASUS ROG Zephyrus G14 GA402RJ"));
        assert_eq!(m("HP", "Victus by HP Laptop 16-d0xxx", "", "").as_deref(), Some("HP Victus by HP Laptop 16-d0xxx"));
        assert_eq!(m("HP", "HP Pavilion Gaming Laptop 15-ec1xxx", "", "").as_deref(), Some("HP Pavilion Gaming Laptop 15-ec1xxx"));
        assert_eq!(m("Dell Inc.", "G15 5511", "", "").as_deref(), Some("Dell G15 5511"));
        assert_eq!(m("Micro-Star International Co., Ltd.", "Katana GF66 11UC", "1.0", "").as_deref(), Some("MSI Katana GF66 11UC"));
        assert_eq!(m("Acer", "Nitro AN515-57", "V1.10", "").as_deref(), Some("Acer Nitro AN515-57"));
        // Placeholder BIOS fields from white-label and self-built machines
        assert_eq!(m("To Be Filled By O.E.M.", "To Be Filled By O.E.M.", "", ""), None);
        assert_eq!(m("System manufacturer", "System Product Name", "System Version", ""), None);
        assert_eq!(m("Default string", "Default string", "Default string", ""), None);
        assert_eq!(m("Notebook", "NH5x_7xDCx_DDx", "", "").as_deref(), Some("Notebook NH5x 7xDCx DDx"));
    }
}
