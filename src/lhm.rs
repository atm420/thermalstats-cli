/// Embedded LibreHardwareMonitor integration (Windows only).
/// Extracts a bundled ThermalReader helper + LHM library to %APPDATA%\ThermalStats\lhm\
/// and uses it to read accurate CPU die temperatures via kernel-mode MSR access.

use std::path::PathBuf;

const LHM_VERSION: &str = "0.9.6+pawnio";

// The LHM bundle zip is embedded at compile time
const LHM_BUNDLE: &[u8] = include_bytes!("../lhm/lhm-bundle.zip");

/// Get the extraction directory: %APPDATA%\ThermalStats\lhm\
fn lhm_dir() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(PathBuf::from(appdata).join("ThermalStats").join("lhm"))
}

/// Ensure LHM files are extracted and PawnIO driver is installed.
/// Returns the directory path if successful.
pub fn ensure_extracted() -> Option<PathBuf> {
    let dir = lhm_dir()?;
    let version_file = dir.join(".version");

    // Check if already extracted with correct version
    if version_file.exists() {
        if let Ok(v) = std::fs::read_to_string(&version_file) {
            if v.trim() == LHM_VERSION {
                // Already up to date — ensure driver is installed
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

    // Install PawnIO kernel driver (required for CPU MSR access)
    ensure_pawnio_driver(&dir);

    // Write version marker
    let _ = std::fs::write(&version_file, LHM_VERSION);

    Some(dir)
}

/// Install the PawnIO kernel driver if not already running.
/// This is required for LibreHardwareMonitor to read CPU die temperatures via MSR.
fn ensure_pawnio_driver(dir: &PathBuf) {
    let setup_exe = dir.join("PawnIO_setup.exe");
    if !setup_exe.exists() {
        return;
    }

    // Check if PawnIO service is already running
    let check = std::process::Command::new("sc.exe")
        .args(["query", "PawnIO"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output();

    if let Ok(output) = check {
        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.contains("RUNNING") {
            return; // Driver already active
        }
    }

    // Install the driver silently
    let mut cmd = std::process::Command::new(&setup_exe);
    cmd.current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    }

    let _ = cmd.output();
}

fn extract_bundle(dir: &PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::{Cursor, Read};

    // Create directory
    std::fs::create_dir_all(dir)?;

    let cursor = Cursor::new(LHM_BUNDLE);
    let mut archive = zip::ZipArchive::new(cursor)?;

    for i in 0..archive.len() {
        let mut file = archive.by_index(i)?;

        // Validate: only allow expected file extensions and no path traversal
        let name = file.name().to_string();
        if name.contains("..") || name.starts_with('/') || name.starts_with('\\') {
            continue;
        }
        let allowed_ext = [".exe", ".dll"];
        if !allowed_ext.iter().any(|ext| name.to_lowercase().ends_with(ext)) {
            continue;
        }

        let outpath = dir.join(&name);
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

    let output = cmd.output().ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;

    Some(LhmReading {
        cpu_temp: json.get("cpu").and_then(|v| v.as_f64()),
        gpu_temp: json.get("gpu").and_then(|v| v.as_f64()),
    })
}
