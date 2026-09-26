//! Plain-text mode: same engine, line-by-line output, no prompts. Used with
//! --plain, when the output isn't a terminal (scripts, CI, redirected logs),
//! or if the full-screen interface can't start.

use crate::api::{self, SubmissionPayload};
use crate::app::{machine_id, App, LaunchOptions, MIN_SUBMIT_SECS, VERSION};
use crate::engine::{self, Phase, Session, TestKind, TestPlan};
use crate::sensors::{Probe, SensorHub};
use colored::Colorize;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub fn detect_only() {
    let hw = crate::hardware::detect_hardware();
    println!("{} v{}\n", "ThermalStats".bold(), VERSION);
    print_hardware(&hw);
    crate::platform::pause_if_own_console("Press Enter to close.");
}

fn print_hardware(hw: &crate::hardware::HardwareInfo) {
    println!("  CPU:    {}", hw.cpu_model.as_deref().unwrap_or("Unknown").yellow());
    if let (Some(c), Some(t)) = (hw.cpu_cores, hw.cpu_threads) {
        println!("  Cores:  {} ({} threads)", c, t);
    }
    let default = crate::gpus::default_index(&hw.gpus);
    for (i, g) in hw.gpus.iter().enumerate() {
        println!(
            "  GPU {}:  {}{}{}",
            i + 1,
            g.name.yellow(),
            g.vram().map(|v| format!(" ({})", v)).unwrap_or_default(),
            if i == default && hw.gpus.len() > 1 { "  [tested]" } else { "" }
        );
    }
    if hw.gpus.is_empty() {
        println!("  GPU:    Unknown");
    }
    println!("  OS:     {}", hw.os.as_deref().unwrap_or("Unknown"));
    println!("  Type:   {}", if hw.is_laptop { "Laptop" } else { "Desktop" });
}

fn temp(v: Option<f64>) -> String {
    v.map(|t| format!("{:.1}°C", t)).unwrap_or_else(|| "--".into())
}

/// Run a test (or diagnostics) and return the process exit code.
pub fn run(locale: &str, opts: &LaunchOptions) -> i32 {
    let code = run_inner(locale, opts);
    crate::platform::pause_if_own_console("Press Enter to close.");
    code
}

fn run_inner(locale: &str, opts: &LaunchOptions) -> i32 {
    println!("{} v{}\n", "ThermalStats".bold(), VERSION);
    println!("{}", "Detecting hardware and sensors...".cyan());
    let boot = crate::setup::run(|_| {});
    let hw = boot.hw;
    let setup = boot.setup;
    print_hardware(&hw);

    let gpu_index = crate::gpus::default_index(&hw.gpus);
    let gpu = hw.gpus.get(gpu_index).cloned();
    let hub = Arc::new(SensorHub::start(setup.lhm_dir.clone(), gpu.clone(), hw.gpus.len(), opts.demo));
    let site = api::site_root(&opts.api_url);

    // Wait for the first readings (the LHM helper can take a few seconds).
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let settled = hub.with(|r| r.cpu_temp.probe != Probe::Searching && (gpu.is_none() || r.gpu_temp.probe != Probe::Searching));
        if settled || Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    hub.with(|r| {
        for (name, ch) in [("CPU", &r.cpu_temp), ("GPU", &r.gpu_temp)] {
            match ch.latest() {
                Some(s) => println!("  {} temperature: {} via {}", name, temp(Some(s.value)).green(), ch.source.as_deref().unwrap_or("?")),
                None if name == "GPU" && gpu.is_none() => {}
                None => println!("  {} temperature: {}", name, "not available".red()),
            }
        }
    });

    if opts.diagnostics {
        return run_diagnostics(&hw, &setup, gpu_index, gpu, &hub, &site, locale);
    }

    let mut kind = opts.test.unwrap_or(TestKind::Both);
    if gpu.is_none() && kind.gpu() {
        kind = TestKind::Cpu;
    }
    let duration = Duration::from_secs(opts.duration.unwrap_or(120));
    let plan = TestPlan { kind, duration, gpu: if kind.gpu() { gpu.clone() } else { None } };
    let quick = App::is_quick(duration.as_secs());

    println!(
        "\n{} {} stress test for {}s. Your PC may feel slow and fans may get loud; that's normal.",
        "▸".cyan(),
        kind.as_str().to_uppercase(),
        duration.as_secs()
    );
    if quick {
        println!(
            "  {}",
            format!("Quick test: only tests of {} seconds or longer are submitted.", MIN_SUBMIT_SECS).yellow()
        );
    }
    let session = Session::start(plan.clone(), hub.clone());
    let mut last_print = Instant::now() - Duration::from_secs(60);
    let progress = loop {
        let p = session.progress();
        if p.phase == Phase::Finished {
            break p;
        }
        if last_print.elapsed() >= Duration::from_secs(10) && p.phase == Phase::Running {
            last_print = Instant::now();
            let elapsed = p.started.map(|s| s.elapsed().min(duration)).unwrap_or_default();
            let mut line = format!("  [{:>4}s / {}s]", elapsed.as_secs(), duration.as_secs());
            if kind.cpu() {
                line.push_str(&format!("  CPU {} (peak {}) {:.0}%", temp(p.cpu.current), temp(p.cpu.peak), p.cpu.usage_now.unwrap_or(0.0)));
            }
            if kind.gpu() {
                line.push_str(&format!("  GPU {} (peak {}) {:.0}%", temp(p.gpu.current), temp(p.gpu.peak), p.gpu.usage_now.unwrap_or(0.0)));
            }
            println!("{}", line);
        }
        std::thread::sleep(Duration::from_millis(250));
    };

    println!("\n{}", "Results".green().bold());
    if kind.cpu() {
        println!("  CPU  idle {}  peak {}  max load {}", temp(progress.cpu.idle), temp(progress.cpu.peak),
            progress.cpu.usage_max.map(|u| format!("{:.0}%", u)).unwrap_or("--".into()));
    }
    if kind.gpu() {
        println!("  GPU  idle {}  peak {}  max load {}", temp(progress.gpu.idle), temp(progress.gpu.peak),
            progress.gpu.usage_max.map(|u| format!("{:.0}%", u)).unwrap_or("--".into()));
    }
    for w in &progress.warnings {
        println!("  {} {:?}", "⚠".yellow(), w);
    }

    let verdict = engine::verdict(&plan, &progress);
    let Some(submit_kind) = verdict.kind else {
        println!("\n  Not submitted: no temperature was read correctly under load.");
        return 1;
    };
    // Demo results are simulated: they may only go to a local dev server.
    let local = site.contains("://localhost") || site.contains("://127.0.0.1");
    if opts.no_submit || (opts.demo && !local) {
        println!("\n  {}", "Skipping submission.".dimmed());
        return 0;
    }
    if quick {
        println!(
            "\n  Not submitted: this was a quick test. Run with --duration {} or longer to submit your result.",
            MIN_SUBMIT_SECS
        );
        return 0;
    }

    let laptop = hw.is_laptop;
    let payload = SubmissionPayload {
        test_type: submit_kind.as_str().into(),
        stress_method: "cli_tool".into(),
        cpu_model: hw.cpu_model.clone(),
        cpu_cores: hw.cpu_cores,
        cpu_threads: hw.cpu_threads,
        gpu_model: gpu.as_ref().map(|g| g.name.clone()),
        gpu_vram: gpu.as_ref().and_then(|g| g.vram()),
        os: hw.os.clone(),
        device_type: Some(if laptop { "laptop" } else { "desktop" }.into()),
        laptop_model: None,
        cooling_type: opts.cooling_type.clone().or(if laptop { Some("stock".into()) } else { None }),
        cooling_model: opts.cooling_model.clone(),
        ambient_temp: opts.ambient_temp.filter(|a| (0.0..=60.0).contains(a)),
        cpu_temp_idle: submit_kind.cpu().then_some(progress.cpu.idle).flatten(),
        cpu_temp_load: submit_kind.cpu().then_some(progress.cpu.peak).flatten(),
        gpu_temp_idle: submit_kind.gpu().then_some(progress.gpu.idle).flatten(),
        gpu_temp_load: submit_kind.gpu().then_some(progress.gpu.peak).flatten(),
        cpu_usage_max: submit_kind.cpu().then_some(progress.cpu.usage_max).flatten(),
        gpu_usage_max: submit_kind.gpu().then_some(progress.gpu.usage_max).flatten(),
        test_duration: Some(duration.as_secs() as i64),
        cli_version: Some(VERSION.into()),
        session_id: Some(machine_id(&hw)),
        series: progress.series.as_ref().map(|s| s.only(submit_kind.cpu(), submit_kind.gpu())),
        // Before/after re-tests are offered in the interactive interface only.
        baseline_id: None,
        change_type: None,
    };
    println!("\n{}", "Submitting results...".cyan());
    match api::submit_results(&site, &payload) {
        Ok(api::Submitted { id, .. }) => {
            let url = api::page_url(&site, locale, &format!("/results/{}", id));
            println!("  {} {}", "✓ Submitted! View at:".green(), url.cyan());
            crate::platform::open_url(&url);
            0
        }
        Err(e) => {
            eprintln!("  {} {}", "✗ Couldn't submit:".red(), e);
            1
        }
    }
}

fn run_diagnostics(
    hw: &crate::hardware::HardwareInfo,
    setup: &crate::setup::SensorSetup,
    gpu_index: usize,
    gpu: Option<crate::gpus::GpuDevice>,
    hub: &Arc<SensorHub>,
    site: &str,
    locale: &str,
) -> i32 {
    println!("\n{}", "Diagnostics (results are not submitted)".yellow().bold());
    println!(
        "  {}",
        "Only needed if temperatures aren't detected. To test your PC and submit a result, run thermalstats --test both.".dimmed()
    );
    let mut log: Vec<String> = Vec::new();
    crate::diagnostics::collect(hw, setup, Some(gpu_index), &mut |line| {
        println!("  {}", line);
        log.push(line);
    });

    let plan = TestPlan {
        kind: TestKind::Both,
        duration: Duration::from_secs(crate::diagnostics::STRESS_SECONDS),
        gpu,
    };
    println!("{}", "Running the 30-second stress test...".cyan());
    let session = Session::start(plan.clone(), hub.clone());
    let progress = loop {
        let p = session.progress();
        if p.phase == Phase::Finished {
            break p;
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    crate::diagnostics::summarize(&plan, &progress, &mut |line| {
        println!("  {}", line);
        log.push(line);
    });

    let payload = api::DebugLogPayload {
        log: log.join("\n"),
        cpu_model: hw.cpu_model.clone(),
        gpu_model: hw.gpus.get(gpu_index).map(|g| g.name.clone()),
        os: hw.os.clone(),
        cli_version: Some(VERSION.into()),
    };
    match api::submit_debug_log(site, &payload) {
        Ok(id) => {
            let url = api::page_url(site, locale, &format!("/debug/{}", id));
            println!("\n  {} {}", "✓ Diagnostic log uploaded:".green(), url.cyan());
            crate::platform::open_url(&url);
            0
        }
        Err(e) => {
            eprintln!("\n  {} {}", "✗ Couldn't upload the log:".red(), e);
            1
        }
    }
}
