//! GPU inventory: every physical GPU in the machine, with the IDs needed to
//! match it across the OS, NVML, wgpu and third-party sensor tools. The GPU
//! the user picks decides which card is stressed, which temperature is read
//! and which model is submitted.

use std::path::PathBuf;

const VENDOR_NVIDIA: u32 = 0x10DE;
const VENDOR_AMD: u32 = 0x1002;
const VENDOR_INTEL: u32 = 0x8086;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GpuKind {
    Discrete,
    Integrated,
    Unknown,
}

#[derive(Debug, Clone)]
pub struct GpuDevice {
    /// Display name, also submitted as the GPU model.
    pub name: String,
    pub vram_bytes: Option<u64>,
    pub kind: GpuKind,
    pub vendor_id: Option<u32>,
    pub device_id: Option<u32>,
    /// NVML / nvidia-smi index (NVIDIA only).
    pub nvml_index: Option<u32>,
    /// Linux: the PCI device directory, e.g. /sys/bus/pci/devices/0000:01:00.0
    #[allow(dead_code)]
    pub sysfs_device: Option<PathBuf>,
}

impl GpuDevice {
    pub fn is_nvidia(&self) -> bool {
        self.vendor_id == Some(VENDOR_NVIDIA) || self.name.to_lowercase().contains("nvidia")
    }

    /// VRAM formatted the way earlier CLI versions submitted it ("16 GB", "512 MB").
    pub fn vram(&self) -> Option<String> {
        self.vram_bytes.map(format_vram)
    }

    /// Whether a sensor/device name reported by another tool refers to this GPU.
    pub fn matches_name(&self, other: &str) -> bool {
        let mine = normalize(&self.name);
        let theirs = normalize(other);
        !mine.is_empty() && !theirs.is_empty() && (theirs.contains(&mine) || mine.contains(&theirs))
    }
}

pub fn format_vram(bytes: u64) -> String {
    let gb = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gb >= 1.0 {
        format!("{:.0} GB", gb)
    } else {
        format!("{:.0} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Lowercase, strip ®/™ markers and punctuation so names from different
/// tools compare equal ("AMD Radeon(TM) Graphics" == "AMD Radeon Graphics").
pub fn normalize(name: &str) -> String {
    let lower = name
        .to_lowercase()
        .replace("(r)", " ")
        .replace("(tm)", " ")
        .replace(['®', '™'], " ");
    lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The GPU to select by default: the discrete card with the most memory.
pub fn default_index(gpus: &[GpuDevice]) -> usize {
    gpus.iter()
        .enumerate()
        .max_by_key(|(_, g)| (g.kind == GpuKind::Discrete, g.vram_bytes.unwrap_or(0)))
        .map(|(i, _)| i)
        .unwrap_or(0)
}

/// Detect all GPUs. Discrete cards come first.
pub fn detect() -> Vec<GpuDevice> {
    let mut gpus = detect_os();
    enrich_with_nvidia(&mut gpus);
    enrich_with_wgpu(&mut gpus);
    for gpu in &mut gpus {
        if gpu.kind == GpuKind::Unknown {
            gpu.kind = guess_kind(gpu);
        }
    }
    gpus.sort_by_key(|g| {
        (
            g.kind != GpuKind::Discrete,
            std::cmp::Reverse(g.vram_bytes.unwrap_or(0)),
        )
    });
    gpus
}

fn guess_kind(gpu: &GpuDevice) -> GpuKind {
    let name = gpu.name.to_lowercase();
    match gpu.vendor_id {
        Some(VENDOR_NVIDIA) => GpuKind::Discrete,
        Some(VENDOR_INTEL) => {
            // Arc A/B-series cards are discrete; "Arc Graphics" is the Core Ultra iGPU.
            if name.contains("arc a") || name.contains("arc b") || name.contains("arc pro") {
                GpuKind::Discrete
            } else {
                GpuKind::Integrated
            }
        }
        Some(VENDOR_AMD) => {
            if name.contains("rx ") || name.contains("radeon pro") || name.contains("instinct") {
                GpuKind::Discrete
            } else if name.contains("radeon graphics") || name.contains("vega") || name.ends_with("m graphics") {
                GpuKind::Integrated
            } else {
                GpuKind::Unknown
            }
        }
        _ if name.starts_with("apple") => GpuKind::Integrated,
        _ => GpuKind::Unknown,
    }
}

/// Attach NVML indices (and exact VRAM) to NVIDIA GPUs; add any NVIDIA GPU the
/// OS scan missed.
fn enrich_with_nvidia(gpus: &mut Vec<GpuDevice>) {
    let nv = crate::nvidia::devices();
    let mut used = vec![false; nv.len()];
    for gpu in gpus.iter_mut().filter(|g| g.is_nvidia()) {
        let found = nv.iter().enumerate().find(|(i, d)| {
            !used[*i]
                && (gpu.device_id.is_some_and(|id| id == d.device_id) || gpu.matches_name(&d.name))
        });
        if let Some((i, d)) = found {
            used[i] = true;
            gpu.nvml_index = Some(d.index);
            gpu.vendor_id = Some(d.vendor_id);
            if d.device_id != 0 {
                gpu.device_id = Some(d.device_id);
            }
            if d.vram_bytes.is_some() {
                gpu.vram_bytes = d.vram_bytes;
            }
        }
    }
    for (i, d) in nv.iter().enumerate() {
        if !used[i] {
            gpus.push(GpuDevice {
                name: d.name.clone(),
                vram_bytes: d.vram_bytes,
                kind: GpuKind::Discrete,
                vendor_id: Some(d.vendor_id),
                device_id: Some(d.device_id),
                nvml_index: Some(d.index),
                sysfs_device: None,
            });
        }
    }
}

/// Use the graphics API's own view of each adapter to fill in the
/// discrete/integrated flag, and as a last resort to find GPUs at all.
fn enrich_with_wgpu(gpus: &mut Vec<GpuDevice>) {
    let adapters = crate::stress::list_adapters();
    // On Windows the OS list is authoritative; elsewhere (or if the OS scan
    // failed) add adapters it missed — wgpu's names are good.
    let add_unmatched = gpus.is_empty() || !cfg!(windows);
    for info in &adapters {
        let kind = match info.device_type {
            wgpu::DeviceType::DiscreteGpu => GpuKind::Discrete,
            wgpu::DeviceType::IntegratedGpu => GpuKind::Integrated,
            _ => continue,
        };
        let matched = gpus.iter_mut().find(|g| {
            (g.vendor_id == Some(info.vendor) && g.device_id == Some(info.device))
                || g.matches_name(&info.name)
        });
        match matched {
            Some(gpu) => {
                gpu.kind = kind;
                if gpu.vendor_id.is_none() {
                    gpu.vendor_id = Some(info.vendor);
                    gpu.device_id = Some(info.device);
                }
            }
            None if add_unmatched => {
                let already = gpus.iter().any(|g| {
                    g.vendor_id == Some(info.vendor) && g.device_id == Some(info.device)
                });
                if !already {
                    gpus.push(GpuDevice {
                        name: clean_adapter_name(&info.name),
                        vram_bytes: None,
                        kind,
                        vendor_id: Some(info.vendor),
                        device_id: Some(info.device),
                        nvml_index: None,
                        sysfs_device: None,
                    });
                }
            }
            None => {}
        }
    }
}

/// Mesa appends the driver and chip: "AMD Radeon RX 7900 XTX (RADV NAVI31)".
pub fn clean_adapter_name(name: &str) -> String {
    match name.find(" (") {
        Some(pos) if name.ends_with(')') => name[..pos].trim().to_string(),
        _ => name.trim().to_string(),
    }
}

// ─── Windows ────────────────────────────────────────────────────────

#[cfg(windows)]
fn detect_os() -> Vec<GpuDevice> {
    use serde::Deserialize;
    use wmi::{COMLibrary, WMIConnection};

    #[derive(Deserialize, Debug)]
    #[serde(rename_all = "PascalCase")]
    struct VideoController {
        name: Option<String>,
        adapter_r_a_m: Option<u64>,
        #[serde(rename = "PNPDeviceID")]
        pnp_device_id: Option<String>,
    }

    let Ok(com) = COMLibrary::new() else { return Vec::new() };
    let Ok(wmi) = WMIConnection::new(com) else { return Vec::new() };
    let controllers: Vec<VideoController> = wmi
        .raw_query("SELECT Name, AdapterRAM, PNPDeviceID FROM Win32_VideoController")
        .unwrap_or_default();

    let registry = registry_adapters();
    let mut gpus = Vec::new();
    for vc in controllers {
        let Some(name) = vc.name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()) else {
            continue;
        };
        let pnp = vc.pnp_device_id.unwrap_or_default().to_uppercase();
        // Virtual adapters (Remote Desktop, Parsec, Basic Display…) aren't on PCI.
        if !pnp.starts_with("PCI\\") {
            continue;
        }
        let vendor_id = pci_id(&pnp, "VEN_");
        let device_id = pci_id(&pnp, "DEV_");

        // AdapterRAM is 32-bit and overflows at 4 GB; the driver's registry
        // entry has the real size.
        let vram_bytes = registry
            .iter()
            .find(|r| {
                (vendor_id.is_some() && r.vendor_id == vendor_id && r.device_id == device_id)
                    || r.name.eq_ignore_ascii_case(&name)
            })
            .and_then(|r| r.vram_bytes)
            .or_else(|| vc.adapter_r_a_m.filter(|b| *b > 0 && *b < 0xFFFF_FFFF));

        gpus.push(GpuDevice {
            name,
            vram_bytes,
            kind: GpuKind::Unknown,
            vendor_id,
            device_id,
            nvml_index: None,
            sysfs_device: None,
        });
    }
    gpus
}

/// Parse "VEN_10DE" / "DEV_2704" out of a PNP device ID.
#[cfg(windows)]
fn pci_id(pnp: &str, key: &str) -> Option<u32> {
    let start = pnp.find(key)? + key.len();
    u32::from_str_radix(pnp.get(start..start + 4)?, 16).ok()
}

#[cfg(windows)]
struct RegistryAdapter {
    name: String,
    vendor_id: Option<u32>,
    device_id: Option<u32>,
    vram_bytes: Option<u64>,
}

/// Read the display adapter class key, where each driver records its
/// "HardwareInformation.qwMemorySize" — the accurate VRAM figure.
#[cfg(windows)]
fn registry_adapters() -> Vec<RegistryAdapter> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    };

    const CLASS_KEY: &str =
        r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";

    let mut out = Vec::new();
    unsafe {
        let mut class: HKEY = std::ptr::null_mut();
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, wide(CLASS_KEY).as_ptr(), 0, KEY_READ, &mut class)
            != ERROR_SUCCESS
        {
            return out;
        }
        for index in 0..64u32 {
            let mut buf = [0u16; 256];
            let mut len = buf.len() as u32;
            let status = RegEnumKeyExW(
                class,
                index,
                buf.as_mut_ptr(),
                &mut len,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            if status != ERROR_SUCCESS {
                break;
            }
            let subkey = String::from_utf16_lossy(&buf[..len as usize]);
            if !subkey.chars().all(|c| c.is_ascii_digit()) {
                continue; // "Properties", "Configuration"
            }
            let Some(name) = reg_string(class, &subkey, "DriverDesc") else { continue };
            let matching = reg_string(class, &subkey, "MatchingDeviceId")
                .unwrap_or_default()
                .to_uppercase();
            let vram_bytes = reg_u64(class, &subkey, "HardwareInformation.qwMemorySize")
                .or_else(|| reg_u64(class, &subkey, "HardwareInformation.MemorySize"))
                .filter(|b| *b > 0);
            out.push(RegistryAdapter {
                name,
                vendor_id: pci_id(&matching, "VEN_"),
                device_id: pci_id(&matching, "DEV_"),
                vram_bytes,
            });
        }
        RegCloseKey(class);
    }
    out
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
unsafe fn reg_raw(
    key: windows_sys::Win32::System::Registry::HKEY,
    subkey: &str,
    value: &str,
) -> Option<(u32, Vec<u8>)> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{RegGetValueW, RRF_RT_ANY};

    let mut kind = 0u32;
    let mut size = 0u32;
    let sub = wide(subkey);
    let val = wide(value);
    if RegGetValueW(key, sub.as_ptr(), val.as_ptr(), RRF_RT_ANY, &mut kind, std::ptr::null_mut(), &mut size)
        != ERROR_SUCCESS
        || size == 0
    {
        return None;
    }
    let mut data = vec![0u8; size as usize];
    if RegGetValueW(
        key,
        sub.as_ptr(),
        val.as_ptr(),
        RRF_RT_ANY,
        &mut kind,
        data.as_mut_ptr() as *mut std::ffi::c_void,
        &mut size,
    ) != ERROR_SUCCESS
    {
        return None;
    }
    data.truncate(size as usize);
    Some((kind, data))
}

#[cfg(windows)]
unsafe fn reg_string(
    key: windows_sys::Win32::System::Registry::HKEY,
    subkey: &str,
    value: &str,
) -> Option<String> {
    use windows_sys::Win32::System::Registry::{REG_MULTI_SZ, REG_SZ};
    let (kind, data) = reg_raw(key, subkey, value)?;
    if kind != REG_SZ && kind != REG_MULTI_SZ {
        return None;
    }
    let units: Vec<u16> = data
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|u| *u != 0)
        .collect();
    Some(String::from_utf16_lossy(&units).trim().to_string()).filter(|s| !s.is_empty())
}

#[cfg(windows)]
unsafe fn reg_u64(
    key: windows_sys::Win32::System::Registry::HKEY,
    subkey: &str,
    value: &str,
) -> Option<u64> {
    let (_, data) = reg_raw(key, subkey, value)?;
    match data.len() {
        8 => Some(u64::from_le_bytes(data[..8].try_into().ok()?)),
        4 => Some(u32::from_le_bytes(data[..4].try_into().ok()?) as u64),
        _ => None,
    }
}

// ─── Linux ──────────────────────────────────────────────────────────

#[cfg(target_os = "linux")]
fn detect_os() -> Vec<GpuDevice> {
    use std::fs;

    let read_hex = |path: PathBuf| -> Option<u32> {
        let text = fs::read_to_string(path).ok()?;
        u32::from_str_radix(text.trim().trim_start_matches("0x"), 16).ok()
    };

    let mut gpus = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/bus/pci/devices") else { return gpus };
    for entry in entries.flatten() {
        let dir = entry.path();
        // PCI class 0x03xxxx = display controller (VGA, 3D, other)
        let Some(class) = read_hex(dir.join("class")) else { continue };
        if class >> 16 != 0x03 {
            continue;
        }
        let vendor_id = read_hex(dir.join("vendor"));
        let device_id = read_hex(dir.join("device"));
        let slot = entry.file_name().to_string_lossy().to_string();
        let name = lspci_name(&slot).unwrap_or_else(|| {
            format!("GPU {:04x}:{:04x}", vendor_id.unwrap_or(0), device_id.unwrap_or(0))
        });
        let vram_bytes = fs::read_to_string(dir.join("mem_info_vram_total"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|b| *b > 0);
        gpus.push(GpuDevice {
            name,
            vram_bytes,
            kind: GpuKind::Unknown,
            vendor_id,
            device_id,
            nvml_index: None,
            sysfs_device: Some(dir),
        });
    }
    // Prefer the graphics API's marketing names ("AMD Radeon RX 7900 XTX")
    // over lspci's chip names ("Navi 31 [Radeon RX 7900 XT/7900 XTX]").
    for info in crate::stress::list_adapters() {
        if let Some(gpu) = gpus
            .iter_mut()
            .find(|g| g.vendor_id == Some(info.vendor) && g.device_id == Some(info.device))
        {
            if !gpu.is_nvidia() {
                gpu.name = clean_adapter_name(&info.name);
            }
        }
    }
    gpus
}

/// `lspci -mm -s <slot>` → "Advanced Micro Devices, Inc. [AMD/ATI]" "Navi 31 [...]"
#[cfg(target_os = "linux")]
fn lspci_name(slot: &str) -> Option<String> {
    let output = std::process::Command::new("lspci")
        .args(["-mm", "-s", slot])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<&str> = text.split('"').filter(|s| !s.trim().is_empty()).collect();
    // fields: slot, class, vendor, device, ...
    let vendor = fields.get(2)?.split_whitespace().next().unwrap_or("");
    let device = fields.get(3)?;
    let device = match (device.find('['), device.rfind(']')) {
        (Some(a), Some(b)) if b > a => &device[a + 1..b],
        _ => device,
    };
    Some(format!("{} {}", vendor, device).trim().to_string())
}

// ─── macOS ──────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn detect_os() -> Vec<GpuDevice> {
    let Ok(output) = std::process::Command::new("system_profiler")
        .args(["SPDisplaysDataType", "-json"])
        .output()
    else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return Vec::new();
    };
    let Some(displays) = json.get("SPDisplaysDataType").and_then(|d| d.as_array()) else {
        return Vec::new();
    };
    displays
        .iter()
        .filter_map(|d| {
            let name = d.get("sppci_model")?.as_str()?.trim().to_string();
            let vram_bytes = d
                .get("spdisplays_vram")
                .or_else(|| d.get("spdisplays_vram_shared"))
                .and_then(|v| v.as_str())
                .and_then(parse_size);
            Some(GpuDevice {
                kind: if name.starts_with("Apple") { GpuKind::Integrated } else { GpuKind::Unknown },
                name,
                vram_bytes,
                vendor_id: None,
                device_id: None,
                nvml_index: None,
                sysfs_device: None,
            })
        })
        .collect()
}

/// "8 GB" / "1536 MB" → bytes
#[cfg(target_os = "macos")]
fn parse_size(text: &str) -> Option<u64> {
    let mut parts = text.split_whitespace();
    let value: f64 = parts.next()?.parse().ok()?;
    let unit = parts.next().unwrap_or("MB").to_uppercase();
    let mult = if unit.starts_with('G') { 1024.0 * 1024.0 * 1024.0 } else { 1024.0 * 1024.0 };
    Some((value * mult) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(name: &str) -> GpuDevice {
        GpuDevice {
            name: name.into(),
            vram_bytes: None,
            kind: GpuKind::Unknown,
            vendor_id: None,
            device_id: None,
            nvml_index: None,
            sysfs_device: None,
        }
    }

    #[test]
    fn matches_names_across_tools() {
        let igpu = gpu("AMD Radeon(TM) Graphics");
        let dgpu = gpu("AMD Radeon RX 7900 XTX");
        assert!(igpu.matches_name("GPU [#1]: AMD Radeon Graphics:"));
        assert!(!igpu.matches_name("AMD Radeon RX 7900 XTX"));
        assert!(dgpu.matches_name("GPU [#0]: AMD Radeon RX 7900 XTX: "));
        assert!(!dgpu.matches_name("AMD Radeon(TM) Graphics"));
    }

    #[test]
    fn formats_vram_like_v1() {
        assert_eq!(format_vram(16 * 1024 * 1024 * 1024), "16 GB");
        assert_eq!(format_vram(512 * 1024 * 1024), "512 MB");
    }

    #[test]
    fn cleans_mesa_names() {
        assert_eq!(clean_adapter_name("AMD Radeon RX 7900 XTX (RADV NAVI31)"), "AMD Radeon RX 7900 XTX");
        assert_eq!(clean_adapter_name("NVIDIA GeForce RTX 4090"), "NVIDIA GeForce RTX 4090");
    }

    #[test]
    fn default_prefers_discrete_with_most_vram() {
        let mut a = gpu("Intel(R) UHD Graphics 770");
        a.kind = GpuKind::Integrated;
        let mut b = gpu("NVIDIA GeForce RTX 3060");
        b.kind = GpuKind::Discrete;
        b.vram_bytes = Some(12 << 30);
        let mut c = gpu("NVIDIA GeForce RTX 4090");
        c.kind = GpuKind::Discrete;
        c.vram_bytes = Some(24 << 30);
        assert_eq!(default_index(&[a, b, c]), 2);
    }
}
