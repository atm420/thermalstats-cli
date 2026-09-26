use clap::Parser;
use std::io::IsTerminal;
use std::time::Duration;

mod api;
mod app;
mod diagnostics;
mod engine;
mod gpus;
mod hardware;
mod lang;
mod nvidia;
mod plain;
mod platform;
mod sensors;
mod series;
mod settings;
mod setup;
mod stress;
mod temps;
mod ui;
#[cfg(windows)]
mod afterburner;
#[cfg(windows)]
mod aida64;
#[cfg(windows)]
mod coretemp;
#[cfg(windows)]
mod hwinfo;
#[cfg(windows)]
mod lhm;

use app::{App, LaunchOptions};
use engine::TestKind;

#[derive(Parser, Debug)]
#[command(
    name = "thermalstats",
    version,
    about = "ThermalStats — stress test your hardware and submit real temperature data",
    long_about = "Detects your hardware, runs CPU/GPU stress tests, reads real temperatures\nvia system APIs, and submits results to ThermalStats for community comparison.\n\nRun without options for the interactive interface; options pre-fill it."
)]
struct Cli {
    /// Test type: cpu, gpu, both, or debug (diagnostics, only needed if temperatures aren't detected)
    #[arg(short, long)]
    test: Option<String>,

    /// Stress test duration in seconds (30-3600; only tests of 60+ seconds are submitted)
    #[arg(short, long)]
    duration: Option<u64>,

    /// API endpoint URL (override for local dev)
    #[arg(long, default_value = api::DEFAULT_API_URL)]
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

    /// Language override: en, fr, es, de, pt, tr, ru, ko, ar (auto-detected from OS if omitted)
    #[arg(long)]
    lang: Option<String>,

    /// Plain text output instead of the full-screen interface
    /// (used automatically when the output isn't a terminal)
    #[arg(long)]
    plain: bool,

    /// Simulated temperatures, for trying the interface; never submits
    #[arg(long, hide = true)]
    demo: bool,
}

fn main() {
    let cli = Cli::parse();
    platform::prepare_process();
    platform::raise_thread_priority();

    let saved_lang = settings::Settings::load().lang;
    let locale = lang::detect_locale(cli.lang.as_deref().or(saved_lang.as_deref()));

    let diagnostics = cli.test.as_deref() == Some("debug");
    let test = match cli.test.as_deref() {
        None | Some("debug") => None,
        Some(t) => match TestKind::parse(t) {
            Some(kind) => Some(kind),
            None => exit_with_error(&format!("Invalid test type '{}'. Use: cpu, gpu, both or debug", t)),
        },
    };
    if let Some(ct) = &cli.cooling_type {
        if !["air", "aio", "custom_loop", "stock", "passive", "other"].contains(&ct.as_str()) {
            exit_with_error(&format!(
                "Invalid cooling type '{}'. Use: stock, air, aio, custom_loop, passive, or other",
                ct
            ));
        }
    }
    let duration = cli.duration.map(|d| d.clamp(30, 3600));

    if cli.detect_only {
        plain::detect_only();
        return;
    }

    let opts = LaunchOptions {
        api_url: cli.api_url,
        no_submit: cli.no_submit,
        demo: cli.demo,
        test,
        duration,
        cooling_type: cli.cooling_type,
        cooling_model: cli.cooling_model,
        ambient_temp: cli.ambient_temp,
        diagnostics,
    };

    let interactive = std::io::stdout().is_terminal() && std::io::stdin().is_terminal();
    if cli.plain || !interactive {
        let code = plain::run(&locale, &opts);
        platform::restore_process();
        std::process::exit(code);
    }

    let outcome = run_interface(locale.clone(), opts.clone());
    platform::restore_process();
    match outcome {
        Ok(farewell) => {
            let t = lang::Lang::new(&locale);
            println!("{}", t.bye);
            if let Some(url) = farewell {
                println!("{}", lang::fill(t.bye_results, &[("url", &url)]));
            }
        }
        // Some consoles can't do raw mode or the alternate screen: carry on in
        // plain mode. (Only when nothing has started yet — never run twice.)
        Err(UiError::Start(e)) => {
            eprintln!("The interactive interface couldn't start ({}). Continuing in plain mode.\n", e);
            let code = plain::run(&locale, &opts);
            platform::restore_process();
            std::process::exit(code);
        }
        Err(UiError::Lost(e)) => {
            eprintln!("The terminal stopped responding ({}).", e);
            platform::pause_if_own_console("Press Enter to close.");
            std::process::exit(1);
        }
    }
}

enum UiError {
    /// The terminal couldn't be set up.
    Start(std::io::Error),
    /// Drawing or input failed after the interface was running.
    Lost(std::io::Error),
}

fn exit_with_error(message: &str) -> ! {
    eprintln!("Error: {}", message);
    platform::pause_if_own_console("Press Enter to close.");
    std::process::exit(2);
}

/// Run the full-screen interface. Returns a URL to print after it closes.
fn run_interface(locale: String, opts: LaunchOptions) -> Result<Option<String>, UiError> {
    use ratatui::crossterm::event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    };
    use ratatui::crossterm::execute;

    let mut terminal = ratatui::try_init().map_err(UiError::Start)?;
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    install_panic_hook();

    let mut app = App::new(locale, opts);
    let result = (|| -> std::io::Result<()> {
        loop {
            terminal.draw(|f| ui::draw(f, &mut app))?;
            // ~15 frames per second keeps the timer and animations smooth.
            if event::poll(Duration::from_millis(66))? {
                loop {
                    match event::read()? {
                        Event::Key(key) => app.on_key(key),
                        Event::Mouse(mouse) => app.on_mouse(mouse),
                        Event::Paste(text) => app.on_paste(&text),
                        _ => {}
                    }
                    if app.quit || !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
            app.tick();
            if app.quit {
                return Ok(());
            }
        }
    })();

    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    if let Some(hub) = &app.hub {
        hub.stop();
    }
    result.map(|_| app.farewell_url.clone()).map_err(UiError::Lost)
}

/// ratatui's hook restores the terminal on panic. Only do that for the
/// interface thread: worker threads catch their own panics, and restoring
/// (or printing) from them would wreck a screen that is still running.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() == Some("main") {
            let _ = ratatui::crossterm::execute!(std::io::stdout(), ratatui::crossterm::event::DisableMouseCapture);
            previous(info);
            eprintln!("\nThermalStats hit an unexpected problem. Please report it at https://thermalstats.com/contact");
            platform::pause_if_own_console("Press Enter to close.");
        }
    }));
}
