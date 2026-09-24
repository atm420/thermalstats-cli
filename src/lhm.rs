/// Embedded LibreHardwareMonitor integration (Windows only).
/// Extracts a bundled ThermalReader helper + LHM library to %APPDATA%\ThermalStats\lhm2\
/// and uses it to read accurate CPU die temperatures via kernel-mode MSR access.
///
/// PawnIO (https://pawnio.eu) is a WHQL-signed kernel driver required by
/// LibreHardwareMonitor for CPU MSR access. The official PawnIO installer is
/// bundled and redistributed with permission from the developer.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Bumped whenever the bundle contents change, so existing installs re-extract.
const BUNDLE_VERSION: &str = "0.9.6.2-stream1";

// The LHM bundle zip is embedded at compile time
const LHM_BUNDLE: &[u8] = include_bytes!("../lhm/lhm-bundle.zip");

/// Result of PawnIO driver setup
#[derive(Debug, Clone, PartialEq)]
pub enum PawnIOStatus {
    /// PawnIO was already installed and running
    AlreadyInstalled,
    /// PawnIO was just installed by the official installer
    Installed,
    /// PawnIO installation failed
    Failed(String),
    /// PawnIO installer was not found in the bundle
    InstallerMissing,
}

/// Extraction directory: %APPDATA%\ThermalStats\lhm2\
/// (v1 of the CLI uses ...\lhm\ — keeping them apart means running both
/// versions never overwrites the other's ThermalReader.exe.)
pub fn dir() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(PathBuf::from(appdata).join("ThermalStats").join("lhm2"))
}

/// Ensure LHM files are extracted and PawnIO driver is available.
/// Returns (directory path, PawnIO status) — directory is None if extraction failed.
pub fn ensure_extracted() -> (Option<PathBuf>, PawnIOStatus) {
    let dir = match dir() {
        Some(d) => d,
        None => return (None, PawnIOStatus::Failed("Could not determine app data directory".into())),
    };
    let version_file = dir.join(".version");

    // Check if already extracted with correct version
    let needs_extract = match std::fs::read_to_string(&version_file) {
        Ok(v) => v.trim() != BUNDLE_VERSION,
        Err(_) => true,
    };

    if needs_extract {
        if let Err(e) = extract_bundle(&dir) {
            return (None, PawnIOStatus::Failed(format!("Failed to extract sensor library: {}", e)));
        }
        let _ = std::fs::write(&version_file, BUNDLE_VERSION);
    }

    // Clean up old manually-installed PawnIO driver (from pre-1.0.3 versions)
    cleanup_old_pawnio_driver(&dir);

    // Ensure PawnIO driver is available via the official installer
    let pawnio_status = ensure_pawnio(&dir);

    (Some(dir), pawnio_status)
}

/// Check if PawnIO is already installed by looking for its uninstall entry
/// in the registry (the official installer writes to Add/Remove Programs).
pub fn is_pawnio_installed() -> bool {
    let paths = [
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO",
        r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO",
    ];

    paths.iter().any(|path| {
        crate::platform::hidden_command("reg.exe")
            .args(["query", path])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

/// Clean up old PawnIO driver that was manually installed by pre-1.0.3 versions.
/// This removes the raw driver files and stops/deletes the manually-created service.
fn cleanup_old_pawnio_driver(dir: &PathBuf) {
    // Remove old pawnio/ subdirectory with raw .sys/.inf/.cat files
    let old_dir = dir.join("pawnio");
    if old_dir.exists() {
        let _ = std::fs::remove_dir_all(&old_dir);
    }

    // If PawnIO is running as a manually-installed service but NOT in Add/Remove Programs,
    // it was installed by an old version — stop and remove it
    if !is_pawnio_installed() {
        let service_exists = crate::platform::hidden_command("sc.exe")
            .args(["query", "PawnIO"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if service_exists {
            for action in ["stop", "delete"] {
                let _ = crate::platform::hidden_command("sc.exe")
                    .args([action, "PawnIO"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
}

/// Ensure PawnIO is installed using the official redistributable installer.
/// The installer is bundled with permission from the PawnIO developer and
/// supports silent installation via the -install -silent flags.
fn ensure_pawnio(dir: &PathBuf) -> PawnIOStatus {
    if is_pawnio_installed() {
        return PawnIOStatus::AlreadyInstalled;
    }

    let installer = dir.join("PawnIO_setup.exe");
    if !installer.exists() {
        return PawnIOStatus::InstallerMissing;
    }

    let result = crate::platform::hidden_command(&installer)
        .args(["-install", "-silent"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    match result {
        Ok(status) => {
            let code = status.code().unwrap_or(-1);
            // 0 = success, 3010 (ERROR_SUCCESS_REBOOT_REQUIRED) = success but reboot needed
            if code == 0 || code == 3010 {
                PawnIOStatus::Installed
            } else {
                PawnIOStatus::Failed(format!("PawnIO installer exited with code {}", code))
            }
        }
        Err(e) => PawnIOStatus::Failed(format!("Failed to run PawnIO installer: {}", e)),
    }
}

/// Uninstall PawnIO using the official installer's -uninstall -silent flags.
pub fn uninstall_pawnio() -> Result<(), String> {
    let dir = dir().ok_or("Could not determine app data directory")?;
    let installer = dir.join("PawnIO_setup.exe");
    if !installer.exists() {
        return Err("PawnIO installer not found".into());
    }

    let result = crate::platform::hidden_command(&installer)
        .args(["-uninstall", "-silent"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    match result {
        Ok(status) => {
            let code = status.code().unwrap_or(-1);
            if code == 0 || code == 3010 {
                Ok(())
            } else {
                Err(format!("Installer exited with code {}", code))
            }
        }
        Err(e) => Err(format!("Failed to run installer: {}", e)),
    }
}

fn extract_bundle(dir: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{Cursor, Read};

    std::fs::create_dir_all(dir)?;

    let cursor = Cursor::new(LHM_BUNDLE);
    let mut archive = zip::ZipArchive::new(cursor)?;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;

        // Validate: no path traversal
        let name = file.name().to_string();
        if name.contains("..") || name.starts_with('/') || name.starts_with('\\') {
            continue;
        }
        let allowed_ext = [".exe", ".dll", ".config"];
        if !allowed_ext.iter().any(|ext| name.to_lowercase().ends_with(ext)) {
            continue;
        }

        let outpath = dir.join(&name);
        if let Some(parent) = outpath.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        std::fs::write(&outpath, &buf)?;
    }

    Ok(())
}

// ─── Readings ───────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LhmGpu {
    pub name: String,
    pub temp: Option<f64>,
    pub sensor: Option<String>,
    pub load: Option<f64>,
}

/// One reading from ThermalReader.exe
#[derive(Debug, Clone)]
pub struct LhmSample {
    pub at: Instant,
    pub cpu_temp: Option<f64>,
    pub cpu_sensor: Option<String>,
    pub gpus: Vec<LhmGpu>,
}

impl LhmSample {
    /// Parse one JSON object. Understands both the streaming format (with a
    /// "gpus" array) and the original one-shot format ("gpu" + "gpuName").
    pub fn parse(line: &str) -> Option<Self> {
        let json: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
        let text = |v: Option<&serde_json::Value>| v.and_then(|s| s.as_str()).map(String::from);

        let mut gpus: Vec<LhmGpu> = json
            .get("gpus")
            .and_then(|g| g.as_array())
            .map(|list| {
                list.iter()
                    .map(|g| LhmGpu {
                        name: text(g.get("name")).unwrap_or_default(),
                        temp: g.get("temp").and_then(|v| v.as_f64()),
                        sensor: text(g.get("sensor")),
                        load: g.get("load").and_then(|v| v.as_f64()),
                    })
                    .collect()
            })
            .unwrap_or_default();

        if gpus.is_empty() {
            if let Some(temp) = json.get("gpu").and_then(|v| v.as_f64()) {
                gpus.push(LhmGpu {
                    name: text(json.get("gpuName")).unwrap_or_default(),
                    temp: Some(temp),
                    sensor: None,
                    load: None,
                });
            }
        }

        Some(LhmSample {
            at: Instant::now(),
            cpu_temp: json.get("cpu").and_then(|v| v.as_f64()),
            cpu_sensor: text(json.get("cpuSensor")),
            gpus,
        })
    }
}

fn reader_command(lhm_dir: &PathBuf) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const ABOVE_NORMAL_PRIORITY_CLASS: u32 = 0x0000_8000;

    let mut cmd = std::process::Command::new(lhm_dir.join("ThermalReader.exe"));
    cmd.current_dir(lhm_dir)
        .creation_flags(CREATE_NO_WINDOW | ABOVE_NORMAL_PRIORITY_CLASS);
    cmd
}

/// Run ThermalReader.exe once and parse its JSON output.
/// Returns (reading, stderr) — stderr carries DIAG/ERROR hints for debug mode.
pub fn read_once(lhm_dir: &PathBuf) -> (Option<LhmSample>, String) {
    if !lhm_dir.join("ThermalReader.exe").exists() {
        return (None, "ThermalReader.exe not found".into());
    }
    let output = reader_command(lhm_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output();
    match output {
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            if !o.status.success() {
                return (None, stderr);
            }
            (LhmSample::parse(&String::from_utf8_lossy(&o.stdout)), stderr)
        }
        Err(e) => (None, format!("Failed to run sensor reader: {}", e)),
    }
}

/// Keeps ThermalReader.exe running in `--stream` mode and holds its latest
/// reading. If the helper can't stream (an older build that prints once and
/// exits) or keeps failing, it falls back to launching it once per second.
pub struct LhmStream {
    latest: Arc<Mutex<Option<LhmSample>>>,
    child: Arc<Mutex<Option<Child>>>,
    stop: Arc<AtomicBool>,
}

impl LhmStream {
    pub fn start(lhm_dir: PathBuf) -> Arc<Self> {
        let stream = Arc::new(LhmStream {
            latest: Arc::new(Mutex::new(None)),
            child: Arc::new(Mutex::new(None)),
            stop: Arc::new(AtomicBool::new(false)),
        });

        let latest = stream.latest.clone();
        let child_slot = stream.child.clone();
        let stop = stream.stop.clone();
        let _ = std::thread::Builder::new()
            .name("sensor-lhm".into())
            .spawn(move || {
                crate::platform::raise_thread_priority();
                run_stream(&lhm_dir, &latest, &child_slot, &stop);
            });
        stream
    }

    /// The latest reading, if it is recent enough to trust.
    pub fn latest(&self) -> Option<LhmSample> {
        let sample = self.latest.lock().ok()?.clone()?;
        (sample.at.elapsed() < Duration::from_secs(6)).then_some(sample)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut slot) = self.child.lock() {
            if let Some(mut child) = slot.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

impl Drop for LhmStream {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_stream(
    lhm_dir: &PathBuf,
    latest: &Mutex<Option<LhmSample>>,
    child_slot: &Mutex<Option<Child>>,
    stop: &AtomicBool,
) {
    let mut failures = 0;
    while !stop.load(Ordering::SeqCst) && failures < 3 {
        let spawned = reader_command(lhm_dir)
            .args(["--stream", "1000"])
            // stdin stays open: the helper exits when we close it (or die)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(_) => break,
        };
        let stdout = child.stdout.take();
        if let Ok(mut slot) = child_slot.lock() {
            *slot = Some(child);
        }

        let started = Instant::now();
        let mut lines = 0;
        if let Some(stdout) = stdout {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Some(sample) = LhmSample::parse(&line) {
                    lines += 1;
                    if let Ok(mut slot) = latest.lock() {
                        *slot = Some(sample);
                    }
                }
                if stop.load(Ordering::SeqCst) {
                    break;
                }
            }
        }

        if let Ok(mut slot) = child_slot.lock() {
            if let Some(mut child) = slot.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        if stop.load(Ordering::SeqCst) {
            return;
        }
        // One line then exit = an older one-shot helper: poll it instead.
        if lines <= 1 && started.elapsed() < Duration::from_secs(10) {
            break;
        }
        failures += 1;
        std::thread::sleep(Duration::from_secs(1));
    }

    // One-shot fallback
    while !stop.load(Ordering::SeqCst) {
        let tick = Instant::now();
        if let (Some(sample), _) = read_once(lhm_dir) {
            if let Ok(mut slot) = latest.lock() {
                *slot = Some(sample);
            }
        }
        let wait = Duration::from_secs(1).saturating_sub(tick.elapsed());
        std::thread::sleep(wait.max(Duration::from_millis(200)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_stream_format() {
        let s = LhmSample::parse(r#"{"cpu":71.5,"gpu":60.0,"cpuName":"AMD Ryzen 7 7800X3D","gpuName":"AMD Radeon RX 7900 XTX","cpuSensor":"Core (Tctl/Tdie)","gpus":[{"name":"AMD Radeon(TM) Graphics","type":"GpuAmd","temp":45.0,"sensor":"GPU Core","load":2.0},{"name":"AMD Radeon RX 7900 XTX","type":"GpuAmd","temp":60.0,"sensor":"GPU Hot Spot","load":99.0}]}"#).unwrap();
        assert_eq!(s.cpu_temp, Some(71.5));
        assert_eq!(s.cpu_sensor.as_deref(), Some("Core (Tctl/Tdie)"));
        assert_eq!(s.gpus.len(), 2);
        assert_eq!(s.gpus[1].load, Some(99.0));
    }

    #[test]
    fn parses_v1_format() {
        let s = LhmSample::parse(r#"{"cpu":55.0,"gpu":null,"cpuName":"Intel Core i7","gpuName":null}"#).unwrap();
        assert_eq!(s.cpu_temp, Some(55.0));
        assert!(s.gpus.is_empty());
    }
}
