use anyhow::Result;
use clap::Parser;
use colored::Colorize;
use std::io::{self, Write};
use std::time::Duration;

mod hardware;
mod stress;
mod temps;
mod submit;
mod lang;
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

    /// Language override: en, fr, es, de, pt (auto-detected from OS if omitted)
    #[arg(long)]
    lang: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Enable ANSI color support on Windows (required for admin/elevated consoles)
    #[cfg(windows)]
    enable_ansi_colors();

    let cli = Cli::parse();

    // Detect locale and load translations
    let locale = lang::detect_locale(cli.lang.as_deref());
    let lang = lang::Lang::new(&locale);

    // Validate test type if provided via CLI arg
    if let Some(ref test) = cli.test {
        if !["cpu", "gpu", "both"].contains(&test.as_str()) {
            eprintln!(
                "{} {}",
                "Error:".red().bold(),
                lang.invalid_test_type.replace("{}", test)
            );
            wait_for_exit(&lang);
            std::process::exit(1);
        }
    }

    // Validate cooling type if provided
    if let Some(ref ct) = cli.cooling_type {
        if !["air", "aio", "custom_loop", "stock", "passive", "other"].contains(&ct.as_str()) {
            eprintln!(
                "{} {}",
                "Error:".red().bold(),
                lang.invalid_cooling_type.replace("{}", ct)
            );
            wait_for_exit(&lang);
            std::process::exit(1);
        }
    }

    print_banner();

    // Step 1: Detect hardware
    println!("\n{}", format!("\u{25b8} {}", lang.detecting_hardware).cyan().bold());
    let hw = hardware::detect_hardware();
    print_hardware(&hw, &lang);

    // Detect device type (laptop vs desktop)
    let is_laptop = hw.is_laptop;
    if is_laptop {
        println!(
            "  {} {}",
            "\u{1f4bb}".normal(),
            lang.laptop_detected.cyan().bold()
        );
    }

    if cli.detect_only {
        wait_for_exit(&lang);
        return Ok(());
    }

    // Interactive prompts if test type / duration not provided via CLI args
    let test_type = match cli.test {
        Some(t) => t,
        None => prompt_test_type(&lang),
    };

    let duration_secs = match cli.duration {
        Some(d) => d,
        None => prompt_duration(&lang),
    };

    // Laptop: ask for laptop model, default cooling to stock
    // Desktop: ask for cooling type and cooler model
    let laptop_model: Option<String>;
    let cooling_type: Option<String>;
    let cooling_model: Option<String>;

    if is_laptop {
        laptop_model = prompt_laptop_model(&lang);
        cooling_type = cli.cooling_type.or(Some("stock".to_string()));
        cooling_model = cli.cooling_model;
    } else {
        laptop_model = None;
        cooling_type = match cli.cooling_type {
            Some(ct) => Some(ct),
            None => prompt_cooling_type(&lang),
        };
        cooling_model = match cli.cooling_model {
            Some(cm) => Some(cm),
            None => prompt_cooling_model(&lang),
        };
    }

    let ambient_temp = match cli.ambient_temp {
        Some(at) => Some(at),
        None => prompt_ambient_temp(&lang),
    };

    // Extract embedded LibreHardwareMonitor for accurate CPU die temps
    let lhm_dir: Option<std::path::PathBuf>;

    #[cfg(windows)]
    {
        if is_elevated() {
            println!(
                "\n  {} {}",
                "\u{25b8}".cyan(),
                lang.extracting_sensor
            );
            let (dir, pawnio_status) = lhm::ensure_extracted();
            lhm_dir = dir;
            if lhm_dir.is_some() {
                // Show PawnIO installation status
                match pawnio_status {
                    lhm::PawnIOStatus::Installed => {
                        println!(
                            "  {} {}",
                            "\u{2713}".green(),
                            lang.pawnio_installed
                        );
                    }
                    lhm::PawnIOStatus::AlreadyInstalled => {}
                    lhm::PawnIOStatus::Failed(ref e) => {
                        eprintln!(
                            "  {} {} {}",
                            "\u{26a0}".yellow(),
                            lang.pawnio_failed,
                            e
                        );
                    }
                    lhm::PawnIOStatus::InstallerMissing => {
                        eprintln!(
                            "  {} {}",
                            "\u{26a0}".yellow(),
                            lang.pawnio_failed
                        );
                    }
                }
                println!(
                    "  {} {}",
                    "\u{2713}".green(),
                    lang.sensor_ready
                );
            }
        } else {
            lhm_dir = None;
            println!(
                "\n  {} {}",
                "\u{26a0}".yellow().bold(),
                lang.requires_admin_msg
            );
            println!(
                "  {} {}",
                "\u{2192}".dimmed(),
                lang.right_click_msg
            );
            println!(
                "  {} {}\n",
                "\u{2192}".dimmed(),
                lang.continuing_board
            );
        }
    }

    #[cfg(not(windows))]
    {
        lhm_dir = None;
    }

    // Step 2: Read idle temperatures
    println!("{}", format!("\u{25b8} {} {}", "Reading", lang.idle).cyan().bold());
    let idle_temps = temps::read_temperatures_with_lhm(lhm_dir.as_ref());
    print_temps(&idle_temps, &lang.idle, &lang);

    // Step 3: Run stress test
    let duration = Duration::from_secs(duration_secs);
    let stress_msg = lang.running_stress
        .replacen("{}", &test_type.to_uppercase(), 1)
        .replacen("{}", &duration_secs.to_string(), 1);
    println!(
        "\n{} {}",
        "\u{25b8}".cyan().bold(),
        stress_msg.yellow().bold()
    );
    println!(
        "  {}",
        lang.press_ctrl_c.dimmed()
    );

    let stress_result = stress::run_stress_test(
        &test_type,
        duration,
        lhm_dir.as_ref(),
        lang.spawned_cpu_threads,
        lang.starting_gpu_stress,
        lang.webgpu_fallback,
        lang.complete,
    ).await;

    // Step 4: Read load temperatures (right after stress completes)
    println!("\n{}", format!("\u{25b8} {} {}", "Reading", lang.load).cyan().bold());
    // Small delay to let final temp readings stabilize — temps lag behind actual load
    tokio::time::sleep(Duration::from_millis(500)).await;
    let load_temps = temps::read_temperatures_with_lhm(lhm_dir.as_ref());
    print_temps(&load_temps, &lang.load, &lang);

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
    println!("\n{}", "\u{2501}".repeat(50).dimmed());
    println!("  {}", lang.results_summary.green().bold());
    println!("{}", "\u{2501}".repeat(50).dimmed());

    if let Some(idle_cpu) = idle_temps.cpu_temp {
        println!("  {} {:.1}\u{00b0}C", lang.cpu_idle_label, idle_cpu);
    }
    if let Some(load_cpu) = final_load_temps.cpu_temp {
        println!("  {} {:.1}\u{00b0}C", lang.cpu_load_label, load_cpu);
    }
    if let Some(idle_gpu) = idle_temps.gpu_temp {
        println!("  {} {:.1}\u{00b0}C", lang.gpu_idle_label, idle_gpu);
    }
    if let Some(load_gpu) = final_load_temps.gpu_temp {
        println!("  {} {:.1}\u{00b0}C", lang.gpu_load_label, load_gpu);
    }
    if let Some(cpu_usage) = stress_result.cpu_usage_max {
        println!("  {} {:.0}%", lang.cpu_max_usage, cpu_usage);
    }
    if let Some(gpu_usage) = stress_result.gpu_usage_max {
        println!("  {} {:.0}%", lang.gpu_max_usage, gpu_usage);
    }
    if let Some(ref ct) = cooling_type {
        let label = match ct.as_str() {
            "stock" => lang.label_stock,
            "air" => lang.label_air,
            "aio" => lang.label_aio,
            "custom_loop" => lang.label_custom_loop,
            "passive" => lang.label_passive,
            "other" => lang.label_other,
            _ => ct.as_str(),
        };
        println!("  {} {}", lang.cooling_label, label);
    }
    if let Some(ref lm) = laptop_model {
        println!("  {} {}", lang.laptop_label, lm);
    }
    if let Some(ref cm) = cooling_model {
        println!("  {} {}", lang.cooler_label, cm);
    }
    if let Some(at) = ambient_temp {
        println!("  {} {:.1}\u{00b0}C", lang.ambient_label, at);
    }
    println!("{}", "\u{2501}".repeat(50).dimmed());

    // Detect stale CPU temp (board sensor didn't change during stress)
    #[cfg(windows)]
    if let (Some(idle_cpu), Some(load_cpu)) = (idle_temps.cpu_temp, final_load_temps.cpu_temp) {
        if (load_cpu - idle_cpu).abs() < 3.0 && stress_result.cpu_usage_max.unwrap_or(0.0) > 80.0 {
            println!(
                "\n  {} {}",
                "\u{2139}".cyan(),
                lang.board_sensor_warning
            );
            println!(
                "  {} {}",
                "\u{2192}".dimmed(),
                lang.rerun_admin
            );
        }
    }

    // Step 6: Submit results
    if !cli.no_submit {
        println!("\n{}", format!("\u{25b8} {}", lang.submitting).cyan().bold());
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
            session_id: Some(generate_machine_id(&hw)),
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
                    "  {} {} {}",
                    "\u{2713}".green().bold(),
                    lang.submitted_view,
                    results_url.cyan()
                );
                open_browser(&results_url, &lang);
            }
            Err(e) => {
                // If production URL failed, try localhost automatically
                if cli.api_url == DEFAULT_API_URL {
                    println!(
                        "  {} {}",
                        "\u{26a0}".yellow(),
                        lang.prod_unreachable
                    );
                    match submit::submit_results(LOCAL_API_URL, &payload).await {
                        Ok(id) => {
                            let results_url = format!("{}/results/{}", LOCAL_SITE_URL, id);
                            println!(
                                "  {} {} {}",
                                "\u{2713}".green().bold(),
                                lang.submitted_local,
                                results_url.cyan()
                            );
                            open_browser(&results_url, &lang);
                        }
                        Err(e2) => {
                            eprintln!(
                                "  {} {} {}",
                                "\u{2717}".red().bold(),
                                lang.failed_submit,
                                e2
                            );
                            eprintln!("  {}", lang.not_saved_online.dimmed());
                        }
                    }
                } else {
                    eprintln!(
                        "  {} {} {}",
                        "\u{2717}".red().bold(),
                        lang.failed_submit,
                        e
                    );
                    eprintln!("  {}", lang.not_saved_online.dimmed());
                }
            }
        }
    } else {
        println!(
            "\n  {}",
            lang.skipping_submit.dimmed()
        );
    }

    println!("\n{}", lang.done.green().bold());
    wait_for_exit(&lang);
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
fn prompt_test_type(lang: &lang::Lang) -> String {
    println!("\n{}", format!("\u{25b8} {}", lang.what_to_test).cyan().bold());
    println!("  {} {}", "[1]".yellow(), lang.cpu_only);
    println!("  {} {}", "[2]".yellow(), lang.gpu_only);
    println!("  {} {}", "[3]".yellow(), lang.cpu_gpu_recommended);
    print!("\n  {} [{}]: ", lang.enter_choice_test, "3".yellow());
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
fn prompt_duration(lang: &lang::Lang) -> u64 {
    println!("\n{}", format!("\u{25b8} {}", lang.how_long_stress).cyan().bold());
    println!(
        "  {} {}",
        "\u{2139}".cyan(),
        lang.recommend_note
    );
    println!(
        "  {} {}",
        " ".normal(),
        lang.recommend_reason
    );
    println!();
    println!("  {} {}", "[1]".yellow(), lang.dur_60);
    println!("  {} {}", "[2]".yellow(), lang.dur_120);
    println!("  {} {}", "[3]".yellow(), lang.dur_180);
    println!("  {} {}", "[4]".yellow(), lang.dur_300);
    println!("  {} {}", "[5]".yellow(), lang.dur_custom);
    print!("\n  {} [{}]: ", lang.enter_choice_duration, "2".yellow());
    io::stdout().flush().ok();

    let mut input = String::new();
    io::stdin().read_line(&mut input).ok();
    match input.trim() {
        "1" => 60,
        "3" => 180,
        "4" => 300,
        "5" => {
            print!("  {} ", lang.enter_duration_secs);
            io::stdout().flush().ok();
            let mut custom = String::new();
            io::stdin().read_line(&mut custom).ok();
            let secs = custom.trim().parse::<u64>().unwrap_or(120);
            if secs < 30 {
                println!(
                    "  {} {}",
                    "\u{26a0}".yellow(),
                    lang.min_duration_30
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
fn prompt_laptop_model(lang: &lang::Lang) -> Option<String> {
    println!("\n{}", format!("\u{25b8} {}", lang.laptop_model_prompt).cyan().bold());
    println!(
        "  {}",
        lang.laptop_model_examples.dimmed()
    );
    print!("  {} ", lang.laptop_model_input);
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
fn prompt_cooling_type(lang: &lang::Lang) -> Option<String> {
    println!("\n{}", format!("\u{25b8} {}", lang.cooling_type_prompt).cyan().bold());
    println!("  {} {}", "[1]".yellow(), lang.cooling_stock);
    println!("  {} {}", "[2]".yellow(), lang.cooling_air);
    println!("  {} {}", "[3]".yellow(), lang.cooling_aio);
    println!("  {} {}", "[4]".yellow(), lang.cooling_custom_loop);
    println!("  {} {}", "[5]".yellow(), lang.cooling_passive);
    println!("  {} {}", "[6]".yellow(), lang.cooling_other);
    println!("  {} {}", "[Enter]".dimmed(), lang.skip);
    print!("\n  {} ", lang.enter_choice_cooling);
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
fn prompt_cooling_model(lang: &lang::Lang) -> Option<String> {
    println!("\n{}", format!("\u{25b8} {}", lang.cooling_model_prompt).cyan().bold());
    println!(
        "  {}",
        lang.cooling_model_examples.dimmed()
    );
    print!("  {} ", lang.cooling_model_input);
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
fn prompt_ambient_temp(lang: &lang::Lang) -> Option<f64> {
    println!("\n{}", format!("\u{25b8} {}", lang.ambient_temp_prompt).cyan().bold());
    println!(
        "  {}",
        lang.ambient_temp_hint.dimmed()
    );
    print!("  {} ", lang.room_temp_input);
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
            println!("  {} {}", "\u{26a0}".yellow(), lang.cant_parse_temp);
            None
        }
    } else {
        // Celsius input
        if let Ok(c) = trimmed.parse::<f64>() {
            Some((c * 10.0).round() / 10.0)
        } else {
            println!("  {} {}", "\u{26a0}".yellow(), lang.cant_parse_temp);
            None
        }
    }
}

/// Auto-close after 30 seconds with countdown, or exit early with Ctrl-C
fn wait_for_exit(lang: &lang::Lang) {
    println!(
        "\n  {}",
        lang.window_closing.dimmed()
    );
    for remaining in (1..=30).rev() {
        let msg = lang.closing_in.replace("{}", &remaining.to_string());
        print!("\r  {}  ", msg);
        io::stdout().flush().ok();
        std::thread::sleep(Duration::from_secs(1));
    }
    println!("\r  {}", lang.goodbye.green());
}

/// Open the results URL in the user's default browser
fn open_browser(url: &str, lang: &lang::Lang) {
    println!(
        "  {} {}",
        "\u{2192}".dimmed(),
        lang.opening_browser
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

fn print_hardware(hw: &hardware::HardwareInfo, lang: &lang::Lang) {
    println!("  CPU:  {}", hw.cpu_model.as_deref().unwrap_or(lang.unknown).yellow());
    println!("  Cores: {}", hw.cpu_cores.map(|c| c.to_string()).unwrap_or(lang.unknown.to_string()));
    println!("  GPU:  {}", hw.gpu_model.as_deref().unwrap_or(lang.unknown).yellow());
    if let Some(ref vram) = hw.gpu_vram {
        println!("  VRAM: {}", vram);
    }
    println!("  OS:   {}", hw.os.as_deref().unwrap_or(lang.unknown));
}

/// Generate a deterministic machine ID from hostname + CPU model.
/// Same machine always produces the same ID = 1 contributor per machine.
fn generate_machine_id(hw: &hardware::HardwareInfo) -> String {
    let hostname = hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_default();
    let cpu = hw.cpu_model.as_deref().unwrap_or("");
    // Simple hash: djb2
    let input = format!("{}:{}", hostname, cpu);
    let mut hash: u64 = 5381;
    for b in input.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(b as u64);
    }
    format!("cli-{:016x}", hash)
}

fn print_temps(temps: &temps::TemperatureReading, label: &str, lang: &lang::Lang) {
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
        println!("  CPU {}: {}", label, lang.not_available.dimmed());
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
        println!("  GPU {}: {}", label, lang.not_available.dimmed());
    }
}
