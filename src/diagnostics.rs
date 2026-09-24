//! Diagnostics ("debug mode"): probes every temperature source and the
//! system settings that commonly block them, then — after a 30 s stress run —
//! uploads the log so it can be reviewed at /debug/{id}. The log format
//! follows the v1 debug mode so existing admin tooling keeps working.

use crate::engine::{Progress, TestPlan};
use crate::hardware::HardwareInfo;
use crate::setup::SensorSetup;

pub const STRESS_SECONDS: u64 = 30;

fn fmt_temp(t: Option<f64>) -> String {
    t.map(|t| format!("{:.1}\u{00b0}C", t)).unwrap_or_else(|| "N/A".into())
}

/// Collect the static checks, calling `emit` for each log line as it's produced.
pub fn collect(hw: &HardwareInfo, setup: &SensorSetup, gpu_index: Option<usize>, emit: &mut dyn FnMut(String)) {
    let mut line = |s: String| emit(s);

    line(format!("ThermalStats CLI Debug Log — v{}", env!("CARGO_PKG_VERSION")));
    line(format!("Timestamp: {}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S")));
    line(String::new());

    line("=== HARDWARE DETECTION ===".into());
    line(format!("CPU Model: {}", hw.cpu_model.as_deref().unwrap_or("N/A")));
    line(format!("CPU Cores: {}", hw.cpu_cores.map(|c| c.to_string()).unwrap_or("N/A".into())));
    line(format!("CPU Threads: {}", hw.cpu_threads.map(|c| c.to_string()).unwrap_or("N/A".into())));
    if hw.gpus.is_empty() {
        line("GPU Model: N/A".into());
    }
    for (i, gpu) in hw.gpus.iter().enumerate() {
        line(format!(
            "GPU {}{}: {} | VRAM: {} | {:?} | PCI {:04x}:{:04x} | NVML index: {}",
            i,
            if Some(i) == gpu_index { " (selected)" } else { "" },
            gpu.name,
            gpu.vram().unwrap_or("N/A".into()),
            gpu.kind,
            gpu.vendor_id.unwrap_or(0),
            gpu.device_id.unwrap_or(0),
            gpu.nvml_index.map(|n| n.to_string()).unwrap_or("-".into()),
        ));
    }
    line(format!("OS: {}", hw.os.as_deref().unwrap_or("N/A")));
    line(format!("Device Type: {}", if hw.is_laptop { "Laptop" } else { "Desktop" }));
    if let Some(battery) = crate::platform::on_battery() {
        line(format!("Power: {}", if battery { "BATTERY" } else { "AC" }));
    }
    line(String::new());

    line("=== SENSOR SETUP ===".into());
    line(format!("Running as Administrator/root: {}", if setup.elevated { "YES" } else { "NO" }));
    line(format!("Monitoring app: {}", setup.monitoring_app.unwrap_or("none")));
    line(format!("HWiNFO shared memory disabled: {}", if setup.hwinfo_shared_memory_off { "YES" } else { "no" }));
    line(format!("Sensor driver (PawnIO): {:?}", setup.driver));
    line(format!(
        "LibreHardwareMonitor helper: {}",
        setup.lhm_dir.as_ref().map(|d| d.display().to_string()).unwrap_or("not used".into())
    ));
    line(format!(
        "NVIDIA driver interface: {}",
        if crate::nvidia::uses_nvml() { "NVML" } else { "nvidia-smi or none" }
    ));
    line(String::new());

    #[cfg(windows)]
    windows_checks(setup, &mut line);

    line("=== TEMPERATURE SOURCES ===".into());
    let ctx = crate::temps::Context {
        #[cfg(windows)]
        lhm: setup.lhm_dir.as_ref().and_then(|d| crate::lhm::read_once(d).0),
    };
    match crate::temps::read_cpu(&ctx) {
        Some(t) => line(format!("CPU: {:.1}\u{00b0}C via {}", t.celsius, t.source)),
        None => line("CPU: NOT AVAILABLE".into()),
    }
    let multi = hw.gpus.len() > 1;
    for (i, gpu) in hw.gpus.iter().enumerate() {
        let temp = crate::temps::read_gpu(&ctx, gpu, multi);
        let usage = crate::temps::read_gpu_usage(&ctx, gpu, multi);
        line(format!(
            "GPU {} ({}): {} | load: {}",
            i,
            gpu.name,
            temp.map(|t| format!("{:.1}\u{00b0}C via {}", t.celsius, t.source)).unwrap_or("NOT AVAILABLE".into()),
            usage.map(|u| format!("{:.0}%", u)).unwrap_or("N/A".into()),
        ));
    }
    line(String::new());

    line("=== GRAPHICS ADAPTERS (stress test) ===".into());
    for info in crate::stress::list_adapters() {
        line(format!(
            "{} | {:?} | {:?} | PCI {:04x}:{:04x} | driver: {} {}",
            info.name, info.backend, info.device_type, info.vendor, info.device, info.driver, info.driver_info
        ));
    }
    line(String::new());
}

#[cfg(windows)]
fn windows_checks(setup: &SensorSetup, line: &mut dyn FnMut(String)) {
    use crate::platform::hidden_command;
    use crate::{afterburner, aida64, coretemp, hwinfo};

    line("=== PAWNIO DRIVER STATUS ===".into());
    line(format!(
        "PawnIO registry entry: {}",
        if crate::lhm::is_pawnio_installed() { "FOUND" } else { "NOT FOUND" }
    ));
    match hidden_command("sc.exe").args(["query", "PawnIO"]).output() {
        Ok(o) if o.status.success() => {
            let out = String::from_utf8_lossy(&o.stdout);
            let state = out.lines().find(|l| l.contains("STATE")).unwrap_or("UNKNOWN").trim().to_string();
            line(format!("PawnIO kernel service: {}", state));
        }
        _ => line("PawnIO kernel service: NOT INSTALLED".into()),
    }
    line(String::new());

    if let Some(dir) = &setup.lhm_dir {
        line("=== LIBREHARDWAREMONITOR HELPER ===".into());
        if let Ok(entries) = std::fs::read_dir(dir) {
            let names: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
            line(format!("Files: {}", names.join(", ")));
        }
        let (sample, stderr) = crate::lhm::read_once(dir);
        match sample {
            Some(s) => {
                line(format!("CPU: {} ({})", fmt_temp(s.cpu_temp), s.cpu_sensor.unwrap_or_default()));
                for g in s.gpus {
                    line(format!(
                        "GPU {}: {} ({}) load {}",
                        g.name,
                        fmt_temp(g.temp),
                        g.sensor.unwrap_or_default(),
                        g.load.map(|l| format!("{:.0}%", l)).unwrap_or("N/A".into())
                    ));
                }
            }
            None => line("ThermalReader.exe: no reading".into()),
        }
        if !stderr.is_empty() {
            line(format!("Stderr: {}", stderr));
        }
        line(String::new());
    }

    line("=== MONITORING APPS (shared memory) ===".into());
    match hwinfo::check_status() {
        hwinfo::HwinfoStatus::SharedMemoryReadable => {
            line("HWiNFO: shared memory READABLE".into());
            if let Some(r) = hwinfo::read_temps() {
                line(format!("  CPU: {} ({})", fmt_temp(r.cpu_temp), r.cpu_source.unwrap_or_default()));
                line(format!("  GPU: {} ({})", fmt_temp(r.gpu_temp), r.gpu_source.unwrap_or_default()));
            } else {
                line("  read_temps() returned None (signature/layout validation failed)".into());
                for l in hwinfo::debug_raw_header().lines() {
                    line(format!("  {}", l));
                }
            }
            let dump = hwinfo::dump_temps();
            line(format!("  All HWiNFO temperature readings ({} entries):", dump.len()));
            for s in dump {
                line(format!("    [{}] {} = {:.1}\u{00b0}C", s.sensor_name, s.label, s.value));
            }
        }
        hwinfo::HwinfoStatus::ProcessRunningNoSharedMem => {
            line("HWiNFO: running but Shared Memory Support DISABLED".into())
        }
        hwinfo::HwinfoStatus::NotRunning => line("HWiNFO: not running".into()),
    }
    if afterburner::is_available() {
        match afterburner::read_temps() {
            Some(r) => line(format!(
                "MSI Afterburner: CPU {} ({}) | GPU {} ({})",
                fmt_temp(r.cpu_temp),
                r.cpu_source.unwrap_or_default(),
                fmt_temp(r.gpu_temp),
                r.gpu_source.unwrap_or_default()
            )),
            None => line("MSI Afterburner: mapping found, no temperatures parsed".into()),
        }
    } else {
        line("MSI Afterburner: not running".into());
    }
    if aida64::is_available() {
        match aida64::read_temps() {
            Some(r) => line(format!(
                "AIDA64: CPU {} ({}) | GPU {} ({})",
                fmt_temp(r.cpu_temp),
                r.cpu_source.unwrap_or_default(),
                fmt_temp(r.gpu_temp),
                r.gpu_source.unwrap_or_default()
            )),
            None => line("AIDA64: mapping found, no <temp> records parsed".into()),
        }
    } else {
        line("AIDA64: not running (or External Applications sharing disabled)".into());
    }
    if coretemp::is_available() {
        match coretemp::read_temps() {
            Some(r) => line(format!("Core Temp: CPU {} ({})", fmt_temp(r.cpu_temp), r.cpu_source.unwrap_or_default())),
            None => line("Core Temp: invalid struct layout".into()),
        }
    } else {
        line("Core Temp: not running".into());
    }
    line(String::new());

    line("=== WINDOWS TEMPERATURE APIS ===".into());
    line(format!("WMI MSAcpi_ThermalZoneTemperature: {}", crate::temps::debug_read_cpu_temp_wmi()));
    line(format!("WMI OHM/LHM namespace: {}", crate::temps::debug_read_cpu_temp_ohm()));
    line(format!("Performance counter thermal zones: {}", crate::temps::debug_read_cpu_temp_perfcounter()));
    line(String::new());

    line("=== POTENTIAL BLOCKING FACTORS ===".into());
    line("Active antivirus/security software:".into());
    match hidden_command("powershell")
        .args([
            "-NoProfile",
            "-Command",
            "Get-CimInstance -Namespace root/SecurityCenter2 -ClassName AntivirusProduct | Select-Object -ExpandProperty displayName",
        ])
        .output()
    {
        Ok(o) if o.status.success() => {
            let out = String::from_utf8_lossy(&o.stdout);
            let names: Vec<&str> = out.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
            if names.is_empty() {
                line("  None detected via SecurityCenter2".into());
            }
            for name in names {
                line(format!("  - {}", name));
            }
        }
        _ => line("  Could not query SecurityCenter2".into()),
    }
    match hidden_command("powershell")
        .args(["-NoProfile", "-Command", "(Get-MpPreference).EnableControlledFolderAccess"])
        .output()
    {
        Ok(o) if o.status.success() => {
            let val = String::from_utf8_lossy(&o.stdout).trim().to_lowercase();
            line(format!(
                "Controlled Folder Access: {}",
                if val == "1" || val == "true" { "ENABLED (may block sensor readers)" } else { "Disabled" }
            ));
        }
        _ => line("Controlled Folder Access: could not determine".into()),
    }
    line(String::new());
}

/// Append the stress-test results and a verdict for each subsystem.
pub fn summarize(plan: &TestPlan, p: &Progress, emit: &mut dyn FnMut(String)) {
    let mut line = |s: String| emit(s);
    line(format!("=== {}-SECOND DIAGNOSTIC STRESS TEST ===", STRESS_SECONDS));
    line(format!("Stress: {} | GPU worker: {:?} | CPU threads: {}", plan.kind.as_str(), p.stress.gpu, p.stress.cpu_threads));
    line(format!("Idle temps — CPU: {} | GPU: {}", fmt_temp(p.cpu.idle), fmt_temp(p.gpu.idle)));
    line(format!("Peak temps — CPU: {} | GPU: {}", fmt_temp(p.cpu.peak), fmt_temp(p.gpu.peak)));
    line(format!(
        "Max usage — CPU: {} | GPU: {}",
        p.cpu.usage_max.map(|u| format!("{:.1}%", u)).unwrap_or("N/A".into()),
        p.gpu.usage_max.map(|u| format!("{:.1}%", u)).unwrap_or("N/A".into()),
    ));
    for w in &p.warnings {
        line(format!("WARNING: {:?}", w));
    }
    line(String::new());

    line("=== VALIDATION SUMMARY ===".into());
    line(format!("CPU temperature sensor: {}", if p.cpu.is_valid() { "WORKING" } else { "ISSUE DETECTED" }));
    line(format!("GPU temperature sensor: {}", if p.gpu.idle.is_some() && p.gpu.peak.is_some() { "WORKING" } else { "ISSUE DETECTED" }));
    line(format!("CPU stress threads: {}", if p.cpu.usage_max.unwrap_or(0.0) > 50.0 { "WORKING" } else { "LOW USAGE" }));
    line(format!(
        "GPU stress: {}",
        match &p.stress.gpu {
            crate::stress::GpuStress::Running { .. } => "WORKING".to_string(),
            crate::stress::GpuStress::Failed(e) => format!("FAILED ({})", e),
            other => format!("{:?}", other),
        }
    ));
}
