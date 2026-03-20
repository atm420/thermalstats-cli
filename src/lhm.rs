/// Embedded LibreHardwareMonitor integration (Windows only).
/// Extracts a bundled ThermalReader helper + LHM library to %APPDATA%\ThermalStats\lhm\
/// and uses it to read accurate CPU die temperatures via kernel-mode MSR access.

use std::path::PathBuf;
use colored::Colorize;

const LHM_VERSION: &str = "0.9.6.1";

// The LHM bundle zip is embedded at compile time
const LHM_BUNDLE: &[u8] = include_bytes!("../lhm/lhm-bundle.zip");

/// Get the extraction directory: %APPDATA%\ThermalStats\lhm\
fn lhm_dir() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(PathBuf::from(appdata).join("ThermalStats").join("lhm"))
}

/// Ensure LHM files are extracted and PawnIO driver is available.
/// Returns the directory path if successful.
pub fn ensure_extracted() -> Option<PathBuf> {
    let dir = lhm_dir()?;
    let version_file = dir.join(".version");

    // Check if already extracted with correct version
    if version_file.exists() {
        if let Ok(v) = std::fs::read_to_string(&version_file) {
            if v.trim() == LHM_VERSION {
                ensure_pawnio_driver(&dir);
                return Some(dir);
            }
        }
    }

    // Extract the bundle
    if let Err(e) = extract_bundle(&dir) {
        eprintln!("  Failed to extract sensor library: {}", e);
        return None;
    }

    // Install PawnIO kernel driver silently (required for CPU MSR access)
    ensure_pawnio_driver(&dir);

    // Write version marker
    let _ = std::fs::write(&version_file, LHM_VERSION);

    Some(dir)
}

/// Install or start the PawnIO kernel driver silently.
/// Uses pnputil for silent driver installation (no UI prompts).
fn ensure_pawnio_driver(dir: &PathBuf) {
    // Check if PawnIO service is already running
    let check = std::process::Command::new("sc.exe")
        .args(["query", "PawnIO"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();

    if let Ok(output) = &check {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.contains("RUNNING") {
            return; // Driver already active
        }
        // Service exists but not running — try to start it
        if output.status.success() {
            let start = std::process::Command::new("sc.exe")
                .args(["start", "PawnIO"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output();
            if let Ok(o) = start {
                if o.status.success() {
                    return;
                }
            }
        }
    }

    // Service doesn't exist — install driver using pnputil (silent, no UI)
    let inf_path = dir.join("pawnio").join("pawnio.inf");
    if !inf_path.exists() {
        return;
    }

    let install = std::process::Command::new("pnputil.exe")
        .args(["/add-driver", &inf_path.to_string_lossy(), "/install"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output();

    if install.is_ok() {
        // Start the service after installation
        let _ = std::process::Command::new("sc.exe")
            .args(["start", "PawnIO"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output();
    }
}

fn extract_bundle(dir: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{Cursor, Read};

    // Create directory
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
        let allowed_ext = [".exe", ".dll", ".config", ".sys", ".inf", ".cat"];
        if !allowed_ext.iter().any(|ext| name.to_lowercase().ends_with(ext)) {
            continue;
        }

        let outpath = dir.join(&name);
        // Create parent directories for nested files (e.g. pawnio/PawnIO.sys)
        if let Some(parent) = outpath.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut outfile = std::fs::File::create(&outpath)?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf)?;
        std::io::Write::write_all(&mut outfile, &buf)?;
    }

    Ok(())
}

/// Temperature reading from the embedded LHM helper
#[derive(Debug)]
#[allow(dead_code)]
pub struct LhmReading {
    pub cpu_temp: Option<f64>,
    pub gpu_temp: Option<f64>,
}

/// Run ThermalReader.exe and parse its JSON output.
/// Returns None if the helper isn't available or fails.
pub fn read_temps(lhm_dir: &PathBuf) -> Option<LhmReading> {
    let reader_exe = lhm_dir.join("ThermalReader.exe");
    if !reader_exe.exists() {
        return None;
    }

    let mut cmd = std::process::Command::new(&reader_exe);
    cmd.current_dir(lhm_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // Hide the console window on Windows
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("  {} Failed to run sensor reader: {}", "\u{26a0}".yellow(), e);
            return None;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !stderr.is_empty() {
            eprintln!("  {} Sensor reader error: {}", "\u{26a0}".yellow(), stderr.trim());
        }
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = match serde_json::from_str(stdout.trim()) {
        Ok(v) => v,
        Err(_) => {
            return None;
        }
    };

    let reading = LhmReading {
        cpu_temp: json.get("cpu").and_then(|v| v.as_f64()),
        gpu_temp: json.get("gpu").and_then(|v| v.as_f64()),
    };

    // Show diagnostic info if CPU was detected but temp is null
    if reading.cpu_temp.is_none() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("DIAG:") {
            eprintln!("  {} CPU detected but temperature unavailable — try running as Administrator", "\u{26a0}".yellow());
        }
    }

    Some(reading)
}
