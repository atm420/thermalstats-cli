use anyhow::Result;
use clap::Parser;
use colored::Colorize;
use std::io::{self, Write};
use std::time::Duration;

mod hardware;
mod stress;
mod temps;
mod submit;
#[cfg(windows)]
mod lhm;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_API_URL: &str = "https://thermalstats.com/api/submissions";
const LOCAL_API_URL: &str = "http://localhost:3000/api/submissions";
const SITE_URL: &str = "https://thermalstats.com";
const LOCAL_SITE_URL: &str = "http://localhost:3000";

#[derive(Parser, Debug)]
#[command(
    name = "thermalstats",
    version,
    about = "ThermalStats CLI — stress test your hardware and submit real temperature data",
    long_about = "Detects your hardware, runs CPU/GPU stress tests, reads real temperatures\nvia system APIs, and submits results to ThermalStats for community comparison."
)]
struct Cli {
    /// Test type: cpu, gpu, or both (interactive if omitted)
    #[arg(short, long)]
    test: Option<String>,

    /// Stress test duration in seconds (interactive if omitted)
    #[arg(short, long)]
    duration: Option<u64>,

    /// API endpoint URL (override for local dev)
    #[arg(long, default_value = DEFAULT_API_URL)]
    api_url: String,

    /// Skip submitting results (just display locally)
    #[arg(long)]
    no_submit: bool,

    /// Show detected hardware and exit
    #[arg(long)]
    detect_only: bool,

    /// Cooling type: stock, air, aio, custom_loop, passive, other
    #[arg(long)]
    cooling_type: Option<String>,

    /// Cooling model (e.g. "Noctua NH-D15")
    #[arg(long)]
    cooling_model: Option<String>,

    /// Ambient room temperature in °C
    #[arg(long)]
    ambient_temp: Option<f64>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Enable ANSI color support on Windows (required for admin/elevated consoles)
    #[cfg(windows)]
    enable_ansi_colors();

    let cli = Cli::parse();

    // Validate test type if provided via CLI arg
    if let Some(ref test) = cli.test {
        if !["cpu", "gpu", "both"].contains(&test.as_str()) {
            eprintln!(
                "{} Invalid test type '{}'. Use: cpu, gpu, or both",
                "Error:".red().bold(),
                test
            );
            wait_for_exit();
            std::process::exit(1);
        }
    }

    // Validate cooling type if provided
    if let Some(ref ct) = cli.cooling_type {
        if !["air", "aio", "custom_loop", "stock", "passive", "other"].contains(&ct.as_str()) {
            eprintln!(
                "{} Invalid cooling type '{}'. Use: stock, air, aio, custom_loop, passive, or other",
                "Error:".red().bold(),
                ct
            );
            wait_for_exit();
            std::process::exit(1);
        }
    }

    print_banner();

    // Step 1: Detect hardware
    println!("\n{}", "▸ Detecting hardware...".cyan().bold());
    let hw = hardware::detect_hardware();
    print_hardware(&hw);

    // Detect device type (laptop vs desktop)
    let is_laptop = hw.is_laptop;
    if is_laptop {
        println!(
            "  {} {}",
            "💻".normal(),
            "Laptop detected".cyan().bold()
        );
    }

    if cli.detect_only {
        wait_for_exit();
        return Ok(());
    }

    // Interactive prompts if test type / duration not provided via CLI args
    let test_type = match cli.test {
        Some(t) => t,
        None => prompt_test_type(),
    };

    let duration_secs = match cli.duration {
        Some(d) => d,
        None => prompt_duration(),
    };

    // Laptop: ask for laptop model, default cooling to stock
    // Desktop: ask for cooling type and cooler model
    let laptop_model: Option<String>;
    let cooling_type: Option<String>;
    let cooling_model: Option<String>;

    if is_laptop {
        laptop_model = prompt_laptop_model();
        cooling_type = cli.cooling_type.or(Some("stock".to_string()));
        cooling_model = cli.cooling_model;
    } else {
        laptop_model = None;
        cooling_type = match cli.cooling_type {
            Some(ct) => Some(ct),
            None => prompt_cooling_type(),
        };
        cooling_model = match cli.cooling_model {
            Some(cm) => Some(cm),
            None => prompt_cooling_model(),
        };
    }

    let ambient_temp = match cli.ambient_temp {
        Some(at) => Some(at),
        None => prompt_ambient_temp(),
    };

    // Extract embedded LibreHardwareMonitor for accurate CPU die temps
    let lhm_dir: Option<std::path::PathBuf>;

    #[cfg(windows)]
    {
        if is_elevated() {
            println!(
                "\n  {} Extracting sensor library...",
                "▸".cyan()
            );
            lhm_dir = lhm::ensure_extracted();
            if lhm_dir.is_some() {
                println!(
                    "  {} Sensor library ready (accurate CPU die temps enabled)",
                    "✓".green()
                );
            }
        } else {
            lhm_dir = None;
            println!(
                "\n  {} This tool requires {} for accurate CPU temperatures.",
                "⚠".yellow().bold(),
                "Run as Administrator".cyan().bold()
            );
            println!(
                "  {} Right-click your terminal → {}",
                "→".dimmed(),
                "Run as administrator".cyan()
            );
            println!(
                "  {} Continuing with board-level sensors (less accurate)...\n",
                "→".dimmed()
            );
        }
    }

    #[cfg(not(windows))]
    {
        lhm_dir = None;
    }

    // Step 2: Read idle temperatures
    println!("{}", "▸ Reading idle temperatures...".cyan().bold());
    let idle_temps = temps::read_temperatures_with_lhm(lhm_dir.as_ref());
    print_temps(&idle_temps, "Idle");

    // Step 3: Run stress test
    let duration = Duration::from_secs(duration_secs);
    println!(
        "\n{} Running {} stress test for {}s...",
        "▸".cyan().bold(),
        test_type.to_uppercase().yellow().bold(),
        duration_secs
    );
    println!(
        "  {}",
        "Press Ctrl+C to stop early.".dimmed()
    );

    let stress_result = stress::run_stress_test(&test_type, duration, lhm_dir.as_ref()).await;

    // Step 4: Read load temperatures (right after stress completes)
    println!("\n{}", "▸ Reading load temperatures...".cyan().bold());
    // Small delay to let final temp readings stabilize — temps lag behind actual load
    tokio::time::sleep(Duration::from_millis(500)).await;
    let load_temps = temps::read_temperatures_with_lhm(lhm_dir.as_ref());
    print_temps(&load_temps, "Load");

    // Use peak temps captured during stress if higher than post-stress reading
    let final_load_temps = temps::TemperatureReading {
        cpu_temp: match (load_temps.cpu_temp, stress_result.cpu_temp_peak) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        },
        gpu_temp: match (load_temps.gpu_temp, stress_result.gpu_temp_peak) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        },
    };

    // Step 5: Display results
    println!("\n{}", "━".repeat(50).dimmed());
    println!("{}", "  RESULTS SUMMARY".green().bold());
    println!("{}", "━".repeat(50).dimmed());

    if let Some(idle_cpu) = idle_temps.cpu_temp {
        println!("  CPU Idle:  {:.1}°C", idle_cpu);
    }
    if let Some(load_cpu) = final_load_temps.cpu_temp {
        println!("  CPU Load:  {:.1}°C", load_cpu);
    }
    if let Some(idle_gpu) = idle_temps.gpu_temp {
        println!("  GPU Idle:  {:.1}°C", idle_gpu);
    }
    if let Some(load_gpu) = final_load_temps.gpu_temp {
        println!("  GPU Load:  {:.1}°C", load_gpu);
    }
    if let Some(cpu_usage) = stress_result.cpu_usage_max {
        println!("  CPU Max Usage: {:.0}%", cpu_usage);
    }
    if let Some(gpu_usage) = stress_result.gpu_usage_max {
        println!("  GPU Max Usage: {:.0}%", gpu_usage);
    }
    if let Some(ref ct) = cooling_type {
        let label = match ct.as_str() {
            "stock" => "Stock Cooler",
            "air" => "Aftermarket Air",
            "aio" => "AIO Liquid",
            "custom_loop" => "Custom Loop",
            "passive" => "Passive / Fanless",
            "other" => "Other",
            _ => ct.as_str(),
        };
        println!("  Cooling: {}", label);
    }
    if let Some(ref lm) = laptop_model {
        println!("  Laptop:  {}", lm);
    }
    if let Some(ref cm) = cooling_model {
        println!("  Cooler:  {}", cm);
    }
    if let Some(at) = ambient_temp {
        println!("  Ambient: {:.1}°C", at);
    }
    println!("{}", "━".repeat(50).dimmed());

    // Detect stale CPU temp (board sensor didn't change during stress)
    #[cfg(windows)]
    if let (Some(idle_cpu), Some(load_cpu)) = (idle_temps.cpu_temp, final_load_temps.cpu_temp) {
        if (load_cpu - idle_cpu).abs() < 3.0 && stress_result.cpu_usage_max.unwrap_or(0.0) > 80.0 {
            println!(
                "\n  {} CPU temp didn't change — reading is from a {} sensor.",
                "ℹ".cyan(),
                "board-level".yellow()
            );
            println!(
                "  {} Re-run as {} for accurate CPU die temps.",
                "→".dimmed(),
                "Administrator".cyan().bold()
            );
        }
    }

    // Step 6: Submit results
    if !cli.no_submit {
        println!("\n{}", "▸ Submitting results...".cyan().bold());
        let payload = submit::SubmissionPayload {
            test_type: test_type.clone(),
            stress_method: "cli_tool".to_string(),
            cpu_model: hw.cpu_model.clone(),
            cpu_cores: hw.cpu_cores,
            cpu_threads: None,
            gpu_model: hw.gpu_model.clone(),
            gpu_vram: hw.gpu_vram.clone(),
            os: hw.os.clone(),
            device_type: Some(if is_laptop { "laptop" } else { "desktop" }.to_string()),
            laptop_model: laptop_model.clone(),
            cooling_type: cooling_type.clone(),
            cooling_model: cooling_model.clone(),
            ambient_temp: ambient_temp,
            cpu_temp_idle: idle_temps.cpu_temp,
            cpu_temp_load: final_load_temps.cpu_temp,
            gpu_temp_idle: idle_temps.gpu_temp,
            gpu_temp_load: final_load_temps.gpu_temp,
            cpu_usage_max: stress_result.cpu_usage_max,
            gpu_usage_max: stress_result.gpu_usage_max,
            test_duration: Some(duration_secs as i64),
            cli_version: Some(VERSION.to_string()),
        };

        match submit::submit_results(&cli.api_url, &payload).await {
            Ok(id) => {
                let base_url = if cli.api_url == DEFAULT_API_URL {
                    SITE_URL
                } else {
                    cli.api_url.trim_end_matches("/api/submissions")
                };
                let results_url = format!("{}/results/{}", base_url, id);
                println!(
                    "  {} Submitted! View at: {}",
                    "✓".green().bold(),
                    results_url.cyan()
                );
                open_browser(&results_url);
            }
            Err(e) => {
                // If production URL failed, try localhost automatically
                if cli.api_url == DEFAULT_API_URL {
                    println!(
                        "  {} Production API unreachable, trying localhost...",
                        "⚠".yellow()
                    );
                    match submit::submit_results(LOCAL_API_URL, &payload).await {
                        Ok(id) => {
                            let results_url = format!("{}/results/{}", LOCAL_SITE_URL, id);
                            println!(
                                "  {} Submitted to local server! View at: {}",
                                "✓".green().bold(),
                                results_url.cyan()
                            );
                            open_browser(&results_url);
                        }
                        Err(e2) => {
                            eprintln!(
                                "  {} Failed to submit: {}",
                                "✗".red().bold(),
                                e2
                            );
                            eprintln!("  {}", "Results were displayed above but not saved online.".dimmed());
                        }
                    }
                } else {
                    eprintln!(
                        "  {} Failed to submit: {}",
                        "✗".red().bold(),
                        e
                    );
                    eprintln!("  {}", "Results were displayed above but not saved online.".dimmed());
                }
            }
        }
    } else {
        println!(
            "\n  {}",
            "Skipping submission (--no-submit flag).".dimmed()
        );
    }

    println!("\n{}", "Done!".green().bold());
    wait_for_exit();
    Ok(())
}

#[cfg(windows)]
fn is_elevated() -> bool {
    use std::process::Command;
    let output = Command::new("net")
        .args(["session"])
        .output();
    matches!(output, Ok(o) if o.status.success())
}

/// Enable ANSI escape code processing on Windows consoles.
/// When running as administrator, Windows opens a fresh console that
/// doesn't have ENABLE_VIRTUAL_TERMINAL_PROCESSING enabled by default,
/// causing raw escape codes like ←[33m to be printed instead of colors.
#[cfg(windows)]
fn enable_ansi_colors() {
    use std::os::windows::io::AsRawHandle;

    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    unsafe {
        let handle = std::io::stdout().as_raw_handle();
        let mut mode: u32 = 0;
        // GetConsoleMode / SetConsoleMode from kernel32
        extern "system" {
            fn GetConsoleMode(h: *mut std::ffi::c_void, mode: *mut u32) -> i32;
            fn SetConsoleMode(h: *mut std::ffi::c_void, mode: u32) -> i32;
        }
        if GetConsoleMode(handle as *mut _, &mut mode) != 0 {
            let _ = SetConsoleMode(handle as *mut _, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        }
    }
}

/// Prompt user to choose test type interactively
fn prompt_test_type() -> String {
    println!("\n{}", "▸ What would you like to test?".cyan().bold());
    println!("  {} CPU only", "[1]".yellow());
    println!("  {} GPU only", "[2]".yellow());
    println!("  {} CPU + GPU (recommended)", "[3]".yellow());
    print!("\n  Enter choice (1/2/3) [{}]: ", "3".yellow());
    io::stdout().flush().ok();

    let mut input = String::new();
    io::stdin().read_line(&mut input).ok();
    match input.trim() {
        "1" => "cpu".to_string(),
        "2" => "gpu".to_string(),
        _ => "both".to_string(),
    }
}

/// Prompt user for test duration interactively
fn prompt_duration() -> u64 {
    println!("\n{}", "▸ How long should the stress test run?".cyan().bold());
    println!(
        "  {} We recommend at least {} so cooling systems have time to",
        "ℹ".cyan(),
        "120 seconds".yellow()
    );
    println!(
        "  {} fully engage and give realistic sustained temperatures.",
        " ".normal()
    );
    println!();
    println!("  {}  60 seconds  (quick)", "[1]".yellow());
    println!("  {} 120 seconds  (recommended)", "[2]".yellow());
    println!("  {} 180 seconds  (thorough)", "[3]".yellow());
    println!("  {} 300 seconds  (extended)", "[4]".yellow());
    println!("  {} Custom", "[5]".yellow());
    print!("\n  Enter choice (1-5) [{}]: ", "2".yellow());
    io::stdout().flush().ok();

    let mut input = String::new();
    io::stdin().read_line(&mut input).ok();
    match input.trim() {
        "1" => 60,
        "3" => 180,
        "4" => 300,
        "5" => {
            print!("  Enter duration in seconds: ");
            io::stdout().flush().ok();
            let mut custom = String::new();
            io::stdin().read_line(&mut custom).ok();
            let secs = custom.trim().parse::<u64>().unwrap_or(120);
            if secs < 30 {
                println!(
                    "  {} Minimum duration is 30 seconds, using 30s.",
                    "⚠".yellow()
                );
                30
            } else {
                secs
            }
        }
        _ => 120, // default: recommended
    }
}

/// Prompt user for laptop model name (free text, optional)
fn prompt_laptop_model() -> Option<String> {
    println!("\n{}", "\u{25b8} What laptop model do you have? (optional)".cyan().bold());
    println!(
        "  {}",
        "e.g. ASUS ROG Zephyrus G14, Dell XPS 15, Lenovo Legion 5 Pro".dimmed()
    );
    print!("  Laptop model (Enter to skip): ");
    io::stdout().flush().ok();

    let mut input = String::new();
    io::stdin().read_line(&mut input).ok();
    let trimmed = input.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Prompt user to choose cooling system type
fn prompt_cooling_type() -> Option<String> {
    println!("\n{}", "\u{25b8} What type of CPU cooling do you use?".cyan().bold());
    println!("  {} Stock Cooler", "[1]".yellow());
    println!("  {} Aftermarket Air Cooler", "[2]".yellow());
    println!("  {} AIO Liquid Cooler", "[3]".yellow());
    println!("  {} Custom Water Loop", "[4]".yellow());
    println!("  {} Passive / Fanless", "[5]".yellow());
    println!("  {} Other", "[6]".yellow());
    println!("  {} Skip", "[Enter]".dimmed());
    print!("\n  Enter choice (1-6) or Enter to skip: ");
    io::stdout().flush().ok();

    let mut input = String::new();
    io::stdin().read_line(&mut input).ok();
    match input.trim() {
        "1" => Some("stock".to_string()),
        "2" => Some("air".to_string()),
        "3" => Some("aio".to_string()),
        "4" => Some("custom_loop".to_string()),
        "5" => Some("passive".to_string()),
        "6" => Some("other".to_string()),
        _ => None,
    }
}

/// Prompt user for cooler model name (free text, optional)
fn prompt_cooling_model() -> Option<String> {
    println!("\n{}", "\u{25b8} What cooler model do you have? (optional)".cyan().bold());
    println!(
        "  {}",
        "e.g. Noctua NH-D15, Corsair H150i, be quiet! Dark Rock Pro 4".dimmed()
    );
    print!("  Cooler model (Enter to skip): ");
    io::stdout().flush().ok();

    let mut input = String::new();
    io::stdin().read_line(&mut input).ok();
    let trimmed = input.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Prompt user for ambient room temperature (optional, accepts C or F)
fn prompt_ambient_temp() -> Option<f64> {
    println!("\n{}", "\u{25b8} What\u{2019}s your ambient room temperature? (optional)".cyan().bold());
    println!(
        "  {}",
        "Enter a number in \u{00b0}C or add 'f' for Fahrenheit (e.g. 22, 72f)".dimmed()
    );
    print!("  Room temp (Enter to skip): ");
    io::stdout().flush().ok();

    let mut input = String::new();
    io::stdin().read_line(&mut input).ok();
    let trimmed = input.trim().to_lowercase();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(f_str) = trimmed.strip_suffix('f') {
        // Fahrenheit input — convert to Celsius
        if let Ok(f) = f_str.trim().parse::<f64>() {
            let celsius = (f - 32.0) * 5.0 / 9.0;
            let rounded = (celsius * 10.0).round() / 10.0;
            println!(
                "  {} {:.1}\u{00b0}F \u{2192} {:.1}\u{00b0}C",
                "\u{2192}".dimmed(),
                f,
                rounded
            );
            Some(rounded)
        } else {
            println!("  {} Couldn't parse temperature, skipping.", "\u{26a0}".yellow());
            None
        }
    } else {
        // Celsius input
        if let Ok(c) = trimmed.parse::<f64>() {
            Some((c * 10.0).round() / 10.0)
        } else {
            println!("  {} Couldn't parse temperature, skipping.", "\u{26a0}".yellow());
            None
        }
    }
}

/// Auto-close after 30 seconds with countdown, or exit early with Ctrl-C
fn wait_for_exit() {
    println!(
        "\n  {}",
        "Window will close in 30 seconds. Press Ctrl+C to exit now.".dimmed()
    );
    for remaining in (1..=30).rev() {
        print!("\r  Closing in {}s...  ", remaining);
        io::stdout().flush().ok();
        std::thread::sleep(Duration::from_secs(1));
    }
    println!("\r  {}", "Goodbye!".green());
}

/// Open the results URL in the user's default browser
fn open_browser(url: &str) {
    println!(
        "  {} Opening results in your browser...",
        "→".dimmed()
    );

    #[cfg(windows)]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn();
    }

    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open")
            .arg(url)
            .spawn();
    }

    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open")
            .arg(url)
            .spawn();
    }
}

fn print_banner() {
    println!(
        "{}",
        r#"
  _____ _                            _ ____  _        _
 |_   _| |__   ___ _ __ _ __ ___   / / ___|| |_ __ _| |_ ___
   | | | '_ \ / _ \ '__| '_ ` _ \ / /\___ \| __/ _` | __/ __|
   | | | | | |  __/ |  | | | | | / /  ___) | || (_| | |_\__ \
   |_| |_| |_|\___|_|  |_| |_| /_/  |____/ \__\__,_|\__|___/
"#
        .cyan()
    );
    println!(
        "  {} v{}\n",
        "ThermalStats CLI".bold(),
        VERSION
    );
}

fn print_hardware(hw: &hardware::HardwareInfo) {
    println!("  CPU:  {}", hw.cpu_model.as_deref().unwrap_or("Unknown").yellow());
    println!("  Cores: {}", hw.cpu_cores.map(|c| c.to_string()).unwrap_or("Unknown".to_string()));
    println!("  GPU:  {}", hw.gpu_model.as_deref().unwrap_or("Unknown").yellow());
    if let Some(ref vram) = hw.gpu_vram {
        println!("  VRAM: {}", vram);
    }
    println!("  OS:   {}", hw.os.as_deref().unwrap_or("Unknown"));
}

fn print_temps(temps: &temps::TemperatureReading, label: &str) {
    if let Some(cpu) = temps.cpu_temp {
        let color = if cpu > 85.0 {
            "red"
        } else if cpu > 70.0 {
            "yellow"
        } else {
            "green"
        };
        let formatted = format!("{:.1}°C", cpu);
        let colored = match color {
            "red" => formatted.red(),
            "yellow" => formatted.yellow(),
            _ => formatted.green(),
        };
        println!("  CPU {}: {}", label, colored);
    } else {
        println!("  CPU {}: {}", label, "Not available".dimmed());
    }
    if let Some(gpu) = temps.gpu_temp {
        let color = if gpu > 85.0 {
            "red"
        } else if gpu > 70.0 {
            "yellow"
        } else {
            "green"
        };
        let formatted = format!("{:.1}°C", gpu);
        let colored = match color {
            "red" => formatted.red(),
            "yellow" => formatted.yellow(),
            _ => formatted.green(),
        };
        println!("  GPU {}: {}", label, colored);
    } else {
        println!("  GPU {}: {}", label, "Not available".dimmed());
    }
}
