//! NVIDIA GPUs: temperatures, usage and memory.
//!
//! Reads through NVML (the library behind `nvidia-smi`) in-process, so a
//! reading costs microseconds instead of spawning `nvidia-smi` every second.
//! `nvidia-smi` stays as a fallback for drivers where NVML can't be loaded.
//! Both report the same "GPU core" temperature, so results are comparable
//! with those submitted by earlier CLI versions.

#[derive(Debug, Clone)]
pub struct NvDevice {
    pub index: u32,
    pub name: String,
    pub vendor_id: u32,
    pub device_id: u32,
    pub vram_bytes: Option<u64>,
}

#[cfg(any(windows, target_os = "linux"))]
mod nvml {
    use super::NvDevice;
    use nvml_wrapper::enum_wrappers::device::TemperatureSensor;
    use nvml_wrapper::Nvml;
    use std::sync::OnceLock;

    static NVML: OnceLock<Option<Nvml>> = OnceLock::new();

    pub fn handle() -> Option<&'static Nvml> {
        NVML.get_or_init(|| {
            if let Ok(nvml) = Nvml::init() {
                return Some(nvml);
            }
            // Older Windows drivers kept nvml.dll outside the DLL search path.
            #[cfg(windows)]
            let fallback = r"C:\Program Files\NVIDIA Corporation\NVSMI\nvml.dll";
            #[cfg(target_os = "linux")]
            let fallback = "libnvidia-ml.so";
            Nvml::builder()
                .lib_path(std::ffi::OsStr::new(fallback))
                .init()
                .ok()
        })
        .as_ref()
    }

    pub fn devices() -> Option<Vec<NvDevice>> {
        let nvml = handle()?;
        let count = nvml.device_count().ok()?;
        let mut out = Vec::new();
        for index in 0..count {
            let Ok(device) = nvml.device_by_index(index) else { continue };
            let Ok(name) = device.name() else { continue };
            // pci_device_id packs the device ID in the high word, vendor in the low word.
            let (vendor_id, device_id) = device
                .pci_info()
                .map(|p| (p.pci_device_id & 0xFFFF, p.pci_device_id >> 16))
                .unwrap_or((0x10DE, 0));
            let vram_bytes = device.memory_info().ok().map(|m| m.total);
            out.push(NvDevice { index, name, vendor_id, device_id, vram_bytes });
        }
        Some(out)
    }

    pub fn temperature(index: u32) -> Option<f64> {
        let device = handle()?.device_by_index(index).ok()?;
        device.temperature(TemperatureSensor::Gpu).ok().map(|t| t as f64)
    }

    pub fn utilization(index: u32) -> Option<f64> {
        let device = handle()?.device_by_index(index).ok()?;
        device.utilization_rates().ok().map(|u| u.gpu as f64)
    }
}

/// All NVIDIA GPUs, in NVML index order. Empty when no NVIDIA driver is present.
pub fn devices() -> Vec<NvDevice> {
    #[cfg(any(windows, target_os = "linux"))]
    if let Some(devices) = nvml::devices() {
        return devices;
    }
    smi_devices()
}

/// GPU temperature for the NVIDIA GPU at `index`, plus a label for the UI.
///
/// Earlier CLI versions asked nvidia-smi for `temperature.gpu_hotspot` first
/// and fell back to the core temperature. Keep that order so submissions stay
/// comparable: the hotspot field is probed once, and when a driver doesn't
/// support it (the usual case) the core temperature is read through NVML.
pub fn temperature(index: u32) -> Option<(f64, &'static str)> {
    static HOTSPOT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let hotspot = *HOTSPOT.get_or_init(|| smi_query(index, "temperature.gpu_hotspot").is_some());
    if hotspot {
        if let Some(t) = smi_query(index, "temperature.gpu_hotspot").filter(|t| *t > 0.0 && *t < 150.0) {
            return Some((t, "nvidia-smi / GPU Hot Spot"));
        }
    }

    #[cfg(any(windows, target_os = "linux"))]
    if let Some(t) = nvml::temperature(index) {
        return Some((t, "NVIDIA driver (NVML) / GPU Core"));
    }
    smi_query(index, "temperature.gpu")
        .filter(|t| *t > 0.0 && *t < 150.0)
        .map(|t| (t, "nvidia-smi / GPU Core"))
}

/// GPU utilisation (%) for the NVIDIA GPU at `index`.
pub fn utilization(index: u32) -> Option<f64> {
    #[cfg(any(windows, target_os = "linux"))]
    if let Some(u) = nvml::utilization(index) {
        return Some(u);
    }
    smi_query(index, "utilization.gpu")
}

/// Whether readings come from NVML (true) or nvidia-smi (false).
pub fn uses_nvml() -> bool {
    #[cfg(any(windows, target_os = "linux"))]
    {
        nvml::handle().is_some()
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        false
    }
}

fn smi_command() -> std::process::Command {
    #[cfg(windows)]
    let mut cmd = crate::platform::hidden_command("nvidia-smi");
    #[cfg(not(windows))]
    let mut cmd = std::process::Command::new("nvidia-smi");
    cmd.stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    cmd
}

fn smi_query(index: u32, field: &str) -> Option<f64> {
    let output = smi_command()
        .args([
            "-i",
            &index.to_string(),
            &format!("--query-gpu={}", field),
            "--format=csv,noheader,nounits",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

fn smi_devices() -> Vec<NvDevice> {
    let Ok(output) = smi_command()
        .args([
            "--query-gpu=index,name,pci.device_id,memory.total",
            "--format=csv,noheader,nounits",
        ])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split(',').map(str::trim).collect();
            if parts.len() < 4 {
                return None;
            }
            let index = parts[0].parse().ok()?;
            // pci.device_id looks like 0x268410DE
            let pci = u32::from_str_radix(parts[2].trim_start_matches("0x"), 16).unwrap_or(0x10DE);
            let vram_bytes = parts[3].parse::<u64>().ok().map(|mib| mib * 1024 * 1024);
            Some(NvDevice {
                index,
                name: parts[1].to_string(),
                vendor_id: pci & 0xFFFF,
                device_id: pci >> 16,
                vram_bytes,
            })
        })
        .collect()
}
