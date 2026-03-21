/// Embedded LibreHardwareMonitor integration (Windows only).
/// Extracts a bundled ThermalReader helper + LHM library to %APPDATA%\ThermalStats\lhm\
/// and uses it to read accurate CPU die temperatures via kernel-mode MSR access.
///
/// PawnIO (https://pawnio.eu) is a WHQL-signed kernel driver required by
/// LibreHardwareMonitor for CPU MSR access. The official PawnIO installer is
/// bundled and redistributed with permission from the developer.

use std::path::PathBuf;
use colored::Colorize;

const LHM_VERSION: &str = "0.9.6.2";

// The LHM bundle zip is embedded at compile time
const LHM_BUNDLE: &[u8] = include_bytes!("../lhm/lhm-bundle.zip");

/// Result of PawnIO driver setup
#[derive(Debug, PartialEq)]
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

/// Get the extraction directory: %APPDATA%\ThermalStats\lhm\
fn lhm_dir() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(PathBuf::from(appdata).join("ThermalStats").join("lhm"))
}

/// Ensure LHM files are extracted and PawnIO driver is available.
/// Returns (directory path, PawnIO status) — directory is None if extraction failed.
pub fn ensure_extracted() -> (Option<PathBuf>, PawnIOStatus) {
    let dir = match lhm_dir() {
        Some(d) => d,
        None => return (None, PawnIOStatus::Failed("Could not determine app data directory".into())),
    };
    let version_file = dir.join(".version");

    // Check if already extracted with correct version
    let needs_extract = if version_file.exists() {
        match std::fs::read_to_string(&version_file) {
            Ok(v) if v.trim() == LHM_VERSION => false,
            _ => true,
        }
    } else {
        true
    };

    if needs_extract {
        if let Err(e) = extract_bundle(&dir) {
            eprintln!("  Failed to extract sensor library: {}", e);
            return (None, PawnIOStatus::Failed(e.to_string()));
        }
        let _ = std::fs::write(&version_file, LHM_VERSION);
    }

    // Clean up old manually-installed PawnIO driver (from pre-1.0.3 versions)
    cleanup_old_pawnio_driver(&dir);

    // Ensure PawnIO driver is available via the official installer
    let pawnio_status = ensure_pawnio(&dir);

    (Some(dir), pawnio_status)
}

/// Check if PawnIO is already installed by looking for its uninstall entry
/// in the registry (the official installer writes to Add/Remove Programs).
fn is_pawnio_installed() -> bool {
    // Check 64-bit uninstall registry
    let paths = [
        r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO",
        r"HKLM\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO",
    ];

    for path in &paths {
        let check = std::process::Command::new("reg.exe")
            .args(["query", path])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output();

        if let Ok(output) = check {
            if output.status.success() {
                return true;
            }
        }
    }

    false
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
    let in_registry = is_pawnio_installed();
    if !in_registry {
        let sc_check = std::process::Command::new("sc.exe")
            .args(["query", "PawnIO"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output();

        if let Ok(output) = sc_check {
            if output.status.success() {
                // Old manual service exists — stop and remove it
                let _ = std::process::Command::new("sc.exe")
                    .args(["stop", "PawnIO"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
                let _ = std::process::Command::new("sc.exe")
                    .args(["delete", "PawnIO"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
        }
    }
}

/// Ensure PawnIO is installed using the official redistributable installer.
/// The installer is bundled with permission from the PawnIO developer and
/// supports silent installation via the -install -silent flags.
fn ensure_pawnio(dir: &PathBuf) -> PawnIOStatus {
    // Check if PawnIO is already installed (official installer in Add/Remove Programs)
    if is_pawnio_installed() {
        return PawnIOStatus::AlreadyInstalled;
    }

    // Look for the bundled official installer
    let installer = dir.join("PawnIO_setup.exe");
    if !installer.exists() {
        return PawnIOStatus::InstallerMissing;
    }

    // Run the official PawnIO installer in silent mode
    println!(
        "  {} Installing PawnIO driver (pawnio.eu)...",
        "\u{25b8}".cyan()
    );
    let result = std::process::Command::new(&installer)
        .args(["-install", "-silent"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
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
        let allowed_ext = [".exe", ".dll", ".config"];
        if !allowed_ext.iter().any(|ext| name.to_lowercase().ends_with(ext)) {
            continue;
        }

        let outpath = dir.join(&name);
        // Create parent directories for nested files
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
