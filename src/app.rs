//! Interface state: screens, forms, background tasks and input handling.
//! Rendering lives in `ui`; this module never draws.
//!
//! Everything slow (hardware detection, network calls, the test itself)
//! runs on worker threads and reports back through `Task` slots or shared
//! state, so the event loop always keeps drawing.

use crate::api::{self, ApiError, CompareRequest, Comparison, FeedbackPayload, SubmissionPayload};
use crate::engine::{self, Phase, Progress, Session, TestKind, TestPlan, Verdict};
use crate::gpus::GpuDevice;
use crate::hardware::HardwareInfo;
use crate::lang::{fill, Lang, LANGUAGES};
use crate::sensors::SensorHub;
use crate::settings::Settings;
use crate::setup::{Boot, BootStep, DriverState, SensorSetup};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const TOAST_FOR: Duration = Duration::from_secs(5);

pub const DURATIONS: [u64; 4] = [60, 120, 180, 300];
pub const TEST_KINDS: [TestKind; 3] = [TestKind::Both, TestKind::Cpu, TestKind::Gpu];
/// API values for the cooling choices, in display order ("" = skip).
pub const COOLING: [&str; 7] = ["stock", "air", "aio", "custom_loop", "passive", "other", ""];

/// Settings passed on the command line.
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    pub api_url: String,
    pub no_submit: bool,
    pub demo: bool,
    pub test: Option<TestKind>,
    pub duration: Option<u64>,
    pub cooling_type: Option<String>,
    pub cooling_model: Option<String>,
    pub ambient_temp: Option<f64>,
    pub diagnostics: bool,
}

// ─── Background tasks ──────────────────────────────────────────────

/// A value computed on a worker thread; `take` returns it once ready.
pub struct Task<T>(Arc<Mutex<Option<T>>>);

impl<T: Send + 'static> Task<T> {
    /// Run `f` on a new thread. If it panics, the task yields `fallback`.
    pub fn spawn(name: &str, fallback: T, f: impl FnOnce() -> T + Send + 'static) -> Self {
        let slot = Arc::new(Mutex::new(None));
        let out = slot.clone();
        let _ = std::thread::Builder::new().name(name.into()).spawn(move || {
            let value = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(fallback);
            *out.lock().unwrap_or_else(|e| e.into_inner()) = Some(value);
        });
        Task(slot)
    }

    pub fn take(&self) -> Option<T> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

// ─── Text input ────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct TextInput {
    pub value: String,
    /// Cursor position in characters.
    pub cursor: usize,
    pub max_chars: usize,
}

impl TextInput {
    pub fn new(value: &str, max_chars: usize) -> Self {
        let value: String = value.chars().take(max_chars).collect();
        TextInput { cursor: value.chars().count(), value, max_chars }
    }

    fn byte_index(&self, char_index: usize) -> usize {
        self.value.char_indices().nth(char_index).map(|(i, _)| i).unwrap_or(self.value.len())
    }

    pub fn insert(&mut self, c: char) {
        if c.is_control() || self.value.chars().count() >= self.max_chars {
            return;
        }
        let at = self.byte_index(self.cursor);
        self.value.insert(at, c);
        self.cursor += 1;
    }

    pub fn insert_str(&mut self, s: &str) {
        for c in s.chars() {
            self.insert(if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c });
        }
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let at = self.byte_index(self.cursor);
        self.value.remove(at);
    }

    pub fn delete(&mut self) {
        if self.cursor < self.value.chars().count() {
            let at = self.byte_index(self.cursor);
            self.value.remove(at);
        }
    }

    /// Handle an editing key; returns true if the key was used.
    pub fn handle(&mut self, key: &KeyEvent) -> bool {
        match key.code {
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => self.insert(c),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.value.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.value.chars().count(),
            _ => return false,
        }
        true
    }

    pub fn trimmed(&self) -> Option<String> {
        let t = self.value.trim();
        (!t.is_empty()).then(|| t.to_string())
    }
}

// ─── Test options form ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Test,
    Gpu,
    Duration,
    CustomSecs,
    Cooling,
    CoolerModel,
    LaptopModel,
    Ambient,
}

impl Field {
    pub fn is_text(self) -> bool {
        matches!(self, Field::CustomSecs | Field::CoolerModel | Field::LaptopModel | Field::Ambient)
    }
}

#[derive(Debug, Clone)]
pub struct Form {
    /// Index into TEST_KINDS
    pub test: usize,
    /// Index into DURATIONS, or DURATIONS.len() for "custom"
    pub duration: usize,
    pub custom_secs: TextInput,
    /// Index into COOLING
    pub cooling: usize,
    pub cooler_model: TextInput,
    pub laptop_model: TextInput,
    pub ambient: TextInput,
    pub focus: Field,
    pub error: Option<String>,
}

impl Form {
    fn new(settings: &Settings, opts: &LaunchOptions) -> Self {
        let test_kind = opts
            .test
            .or_else(|| settings.test_type.as_deref().and_then(TestKind::parse))
            .unwrap_or(TestKind::Both);
        let secs = opts.duration.or(settings.duration_secs).unwrap_or(120);
        let (duration, custom) = match DURATIONS.iter().position(|d| *d == secs) {
            Some(i) => (i, String::new()),
            None => (DURATIONS.len(), secs.to_string()),
        };
        let cooling_value = opts.cooling_type.clone().or(settings.cooling_type.clone()).unwrap_or_default();
        let cooling = COOLING.iter().position(|c| *c == cooling_value).unwrap_or(COOLING.len() - 1);
        let ambient = opts
            .ambient_temp
            .or(settings.ambient_temp)
            .map(|a| format!("{}", a))
            .unwrap_or_default();

        Form {
            test: TEST_KINDS.iter().position(|k| *k == test_kind).unwrap_or(0),
            duration,
            custom_secs: TextInput::new(&custom, 4),
            cooling,
            cooler_model: TextInput::new(
                opts.cooling_model.as_deref().or(settings.cooling_model.as_deref()).unwrap_or(""),
                120,
            ),
            laptop_model: TextInput::new(settings.laptop_model.as_deref().unwrap_or(""), 120),
            ambient: TextInput::new(&ambient, 8),
            focus: Field::Test,
            error: None,
        }
    }

    pub fn kind(&self) -> TestKind {
        TEST_KINDS[self.test]
    }

    /// Fields shown for this machine, in order.
    pub fn fields(&self, laptop: bool, gpu_count: usize, details_only: bool) -> Vec<Field> {
        let mut fields = Vec::new();
        if !details_only {
            fields.push(Field::Test);
            if gpu_count > 1 && self.kind().gpu() {
                fields.push(Field::Gpu);
            }
            fields.push(Field::Duration);
            if self.duration == DURATIONS.len() {
                fields.push(Field::CustomSecs);
            }
        }
        if laptop {
            fields.push(Field::LaptopModel);
        } else {
            fields.push(Field::Cooling);
            fields.push(Field::CoolerModel);
        }
        fields.push(Field::Ambient);
        fields
    }

    pub fn duration_secs(&self) -> Option<u64> {
        match DURATIONS.get(self.duration) {
            Some(d) => Some(*d),
            None => self.custom_secs.value.trim().parse().ok().filter(|s| (30..=3600).contains(s)),
        }
    }

    /// Room temperature in °C. `Err` if something was typed but can't be used.
    pub fn ambient_celsius(&self) -> Result<Option<f64>, ()> {
        let text = self.ambient.value.trim().to_lowercase().replace(',', ".");
        if text.is_empty() {
            return Ok(None);
        }
        let text = text.trim_end_matches("°c").trim_end_matches('c').trim();
        let celsius = if let Some(f) = text.strip_suffix("°f").or_else(|| text.strip_suffix('f')) {
            let f: f64 = f.trim().parse().map_err(|_| ())?;
            (f - 32.0) * 5.0 / 9.0
        } else {
            text.parse::<f64>().map_err(|_| ())?
        };
        let rounded = (celsius * 10.0).round() / 10.0;
        if (0.0..=60.0).contains(&rounded) {
            Ok(Some(rounded))
        } else {
            Err(())
        }
    }

    pub fn cooling_type(&self, laptop: bool, opts: &LaunchOptions) -> Option<String> {
        if laptop {
            // Laptops use their built-in cooling unless told otherwise.
            return opts.cooling_type.clone().or(Some("stock".into()));
        }
        let value = COOLING[self.cooling];
        (!value.is_empty()).then(|| value.to_string())
    }

    fn text_mut(&mut self, field: Field) -> Option<&mut TextInput> {
        match field {
            Field::CustomSecs => Some(&mut self.custom_secs),
            Field::CoolerModel => Some(&mut self.cooler_model),
            Field::LaptopModel => Some(&mut self.laptop_model),
            Field::Ambient => Some(&mut self.ambient),
            _ => None,
        }
    }

    fn choice_count(field: Field, gpu_count: usize) -> usize {
        match field {
            Field::Test => TEST_KINDS.len(),
            Field::Gpu => gpu_count,
            Field::Duration => DURATIONS.len() + 1,
            Field::Cooling => COOLING.len(),
            _ => 0,
        }
    }
}

// ─── Feedback ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FbField {
    Rating,
    Category,
    Message,
    Email,
    Include,
    Send,
}

const FB_FIELDS: [FbField; 6] =
    [FbField::Rating, FbField::Category, FbField::Message, FbField::Email, FbField::Include, FbField::Send];
pub const FB_CATEGORIES: [&str; 4] = ["bug", "idea", "praise", "other"];

pub enum FbState {
    Editing,
    Sending(Task<Result<(), ApiError>>),
    Sent,
    Failed(String),
}

pub struct FeedbackForm {
    pub rating: Option<u8>,
    pub category: usize,
    pub message: TextInput,
    pub email: TextInput,
    pub include_info: bool,
    pub focus: FbField,
    pub state: FbState,
    pub error: Option<String>,
    context: &'static str,
}

// ─── Results ───────────────────────────────────────────────────────

pub enum SubmitState {
    /// About to be sent (a completed test submits straight away).
    Ready,
    Sending(Task<Result<String, ApiError>>),
    Done { url: String },
    Failed { reason: String, retry: bool },
    /// Can't be submitted; the reason is shown.
    Blocked(String),
}

pub enum CompareState {
    Idle,
    Loading(Task<Result<Comparison, ApiError>>),
    Ready(Comparison),
    Failed,
}

pub struct ResultsState {
    pub plan: TestPlan,
    pub progress: Progress,
    pub verdict: Verdict,
    pub submit: SubmitState,
    pub compare: CompareState,
}

// ─── Diagnostics ───────────────────────────────────────────────────

pub enum DiagStage {
    Intro,
    Collecting(Task<()>),
    /// The 30 s stress run happens on the Running screen.
    Stress,
    Uploading(Task<Result<String, ApiError>>),
    Done { url: String },
    Failed(String),
}

pub struct DiagState {
    pub lines: Arc<Mutex<Vec<String>>>,
    pub stage: DiagStage,
    /// Lines scrolled up from the bottom.
    pub scroll: usize,
    pub saved: Option<PathBuf>,
}

// ─── Screens, dialogs, actions ─────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Boot,
    Home,
    Options,
    Confirm,
    Running,
    Results,
    Diagnostics,
}

pub enum Modal {
    Feedback(FeedbackForm),
    Help,
    Language { cursor: usize },
    StopTest,
    EditDetails(Form),
    Pawnio { removing: Option<Task<Result<(), String>>>, error: Option<String> },
}

/// Something the user can trigger by key or click.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Continue,
    Back,
    StartTest,
    StopTest,
    KeepRunning,
    ConfirmStop,
    Submit,
    EditDetails,
    SaveDetails,
    OpenResults,
    CopyLink,
    RunAgain,
    Feedback,
    Support,
    Help,
    Language,
    SetLanguage(usize),
    CloseModal,
    Quit,
    Diagnostics,
    StartDiagnostics,
    PrevGpu,
    NextGpu,
    Focus(Field),
    Choose(Field, usize),
    FbFocus(FbField),
    FbRating(u8),
    FbCategory(usize),
    FbToggleInclude,
    FbSend,
    KeepPawnio,
    UninstallPawnio,
}

pub struct App {
    pub t: Lang,
    pub locale: String,
    pub opts: LaunchOptions,
    pub site: String,
    pub settings: Settings,

    pub screen: Screen,
    pub boot_step: Arc<Mutex<BootStep>>,
    boot_task: Option<Task<Option<Boot>>>,
    pub hw: Option<HardwareInfo>,
    pub setup: Option<SensorSetup>,
    pub hub: Option<Arc<SensorHub>>,
    pub gpu_index: usize,
    pub on_battery: Option<bool>,
    machine_id: String,

    pub form: Form,
    pub session: Option<Session>,
    pub results: Option<ResultsState>,
    pub diag: Option<DiagState>,

    pub modal: Option<Modal>,
    pub toast: Option<(String, Instant)>,
    /// Click targets registered by the last draw, topmost last.
    pub hits: Vec<(Rect, Action)>,
    pub frame: u64,
    pub quit: bool,
    /// Shown in the terminal after the interface closes.
    pub farewell_url: Option<String>,
    pawnio_decided: bool,
}

impl App {
    pub fn new(locale: String, opts: LaunchOptions) -> Self {
        let settings = Settings::load();
        let form = Form::new(&settings, &opts);
        let boot_step = Arc::new(Mutex::new(BootStep::Hardware));
        let step = boot_step.clone();
        let boot_task = Task::spawn("boot", None, move || {
            Some(crate::setup::run(|s| *step.lock().unwrap_or_else(|e| e.into_inner()) = s))
        });

        App {
            t: Lang::new(&locale),
            site: api::site_root(&opts.api_url),
            locale,
            opts,
            settings,
            screen: Screen::Boot,
            boot_step,
            boot_task: Some(boot_task),
            hw: None,
            setup: None,
            hub: None,
            gpu_index: 0,
            on_battery: None,
            machine_id: String::new(),
            form,
            session: None,
            results: None,
            diag: None,
            modal: None,
            toast: None,
            hits: Vec::new(),
            frame: 0,
            quit: false,
            farewell_url: None,
            pawnio_decided: false,
        }
    }

    pub fn gpus(&self) -> &[GpuDevice] {
        self.hw.as_ref().map(|h| h.gpus.as_slice()).unwrap_or(&[])
    }

    pub fn selected_gpu(&self) -> Option<&GpuDevice> {
        self.gpus().get(self.gpu_index)
    }

    pub fn is_laptop(&self) -> bool {
        self.hw.as_ref().is_some_and(|h| h.is_laptop)
    }

    pub fn is_typing(&self) -> bool {
        match &self.modal {
            Some(Modal::Feedback(fb)) => matches!(fb.focus, FbField::Message | FbField::Email),
            Some(Modal::EditDetails(form)) => form.focus.is_text(),
            Some(_) => false,
            None => self.screen == Screen::Options && self.form.focus.is_text(),
        }
    }

    pub fn toast(&mut self, message: String) {
        self.toast = Some((message, Instant::now()));
    }

    pub fn format_duration(&self, secs: u64) -> String {
        if secs % 60 == 0 {
            fill(self.t.fmt_min, &[("n", &(secs / 60).to_string())])
        } else if secs > 60 {
            format!(
                "{} {}",
                fill(self.t.fmt_min, &[("n", &(secs / 60).to_string())]),
                fill(self.t.fmt_sec, &[("n", &(secs % 60).to_string())])
            )
        } else {
            fill(self.t.fmt_sec, &[("n", &secs.to_string())])
        }
    }

    pub fn kind_label(&self, kind: TestKind) -> &'static str {
        match kind {
            TestKind::Both => self.t.test_both,
            TestKind::Cpu => self.t.test_cpu,
            TestKind::Gpu => self.t.test_gpu,
        }
    }

    pub fn cooling_label(&self, value: &str) -> &'static str {
        match value {
            "stock" => self.t.cool_stock,
            "air" => self.t.cool_air,
            "aio" => self.t.cool_aio,
            "custom_loop" => self.t.cool_custom,
            "passive" => self.t.cool_passive,
            "other" => self.t.cool_other,
            _ => self.t.cool_skip,
        }
    }

    // ── Periodic updates ──

    pub fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);

        if self.toast.as_ref().is_some_and(|(_, at)| at.elapsed() > TOAST_FOR) {
            self.toast = None;
        }

        if let Some(task) = &self.boot_task {
            if let Some(boot) = task.take() {
                self.boot_task = None;
                self.finish_boot(boot);
            }
        }

        // Test finished?
        if self.screen == Screen::Running {
            if let Some(session) = &self.session {
                let progress = session.progress();
                if progress.phase == Phase::Finished {
                    let session = self.session.take().unwrap();
                    self.modal = None;
                    if self.diag.as_ref().is_some_and(|d| matches!(d.stage, DiagStage::Stress)) {
                        self.finish_diagnostics(&session.plan, &progress);
                    } else {
                        self.show_results(session.plan, progress);
                    }
                }
            }
        }

        self.tick_results();
        self.tick_feedback();
        self.tick_diagnostics();
        self.tick_pawnio();
    }

    fn finish_boot(&mut self, boot: Option<Boot>) {
        let Some(boot) = boot else {
            // Detection itself failed; carry on with empty hardware info.
            self.hw = Some(HardwareInfo {
                cpu_model: None,
                cpu_cores: None,
                cpu_threads: None,
                os: None,
                is_laptop: false,
                gpus: Vec::new(),
            });
            self.screen = Screen::Home;
            return;
        };
        let Boot { hw, setup } = boot;

        // Prefer the GPU picked last time, else the best dedicated card.
        self.gpu_index = self
            .settings
            .gpu_name
            .as_ref()
            .and_then(|name| hw.gpus.iter().position(|g| &g.name == name))
            .unwrap_or_else(|| crate::gpus::default_index(&hw.gpus));
        if hw.gpus.is_empty() && self.form.kind().gpu() {
            self.form.test = TEST_KINDS.iter().position(|k| *k == TestKind::Cpu).unwrap();
        }

        let hub = SensorHub::start(
            setup.lhm_dir.clone(),
            hw.gpus.get(self.gpu_index).cloned(),
            hw.gpus.len(),
            self.opts.demo,
        );
        self.machine_id = machine_id(&hw);
        self.on_battery = crate::platform::on_battery();
        self.hub = Some(Arc::new(hub));
        self.hw = Some(hw);
        self.setup = Some(setup);
        self.screen = if self.opts.diagnostics { Screen::Diagnostics } else { Screen::Home };
        if self.opts.diagnostics {
            self.open_diagnostics();
        }
    }

    fn tick_results(&mut self) {
        let Some(r) = &mut self.results else { return };

        if let SubmitState::Sending(task) = &r.submit {
            if let Some(result) = task.take() {
                r.submit = match result {
                    Ok(id) => SubmitState::Done { url: api::page_url(&self.site, &self.locale, &format!("/results/{}", id)) },
                    Err(ApiError::Connection(e)) => SubmitState::Failed { reason: e, retry: true },
                    // 429 = daily limit or duplicate: retrying won't help.
                    Err(ApiError::Rejected { status, message }) => {
                        SubmitState::Failed { reason: message, retry: status >= 500 }
                    }
                };
                if let SubmitState::Done { url } = &r.submit {
                    let url = url.clone();
                    self.farewell_url = Some(url.clone());
                    if crate::platform::open_url(&url) {
                        self.toast = Some((fill(self.t.toast_opened, &[("url", &url)]), Instant::now()));
                    }
                    self.start_compare();
                    return;
                }
            }
        }

        let r = self.results.as_mut().unwrap();
        if let CompareState::Loading(task) = &r.compare {
            if let Some(result) = task.take() {
                r.compare = match result {
                    Ok(c) => CompareState::Ready(c),
                    Err(_) => CompareState::Failed,
                };
            }
        }
    }

    fn tick_feedback(&mut self) {
        let Some(Modal::Feedback(fb)) = &mut self.modal else { return };
        if let FbState::Sending(task) = &fb.state {
            if let Some(result) = task.take() {
                fb.state = match result {
                    Ok(()) => FbState::Sent,
                    Err(e) => FbState::Failed(e.to_string()),
                };
            }
        }
    }

    fn tick_diagnostics(&mut self) {
        let Some(diag) = &mut self.diag else { return };
        match &diag.stage {
            DiagStage::Collecting(task) => {
                if task.take().is_some() {
                    diag.stage = DiagStage::Stress;
                    let plan = TestPlan {
                        kind: TestKind::Both,
                        duration: Duration::from_secs(crate::diagnostics::STRESS_SECONDS),
                        gpu: self.hw.as_ref().and_then(|h| h.gpus.get(self.gpu_index).cloned()),
                    };
                    if let Some(hub) = &self.hub {
                        self.session = Some(Session::start(plan, hub.clone()));
                        self.screen = Screen::Running;
                    }
                }
            }
            DiagStage::Uploading(task) => {
                if let Some(result) = task.take() {
                    diag.stage = match result {
                        Ok(id) => {
                            let url = api::page_url(&self.site, &self.locale, &format!("/debug/{}", id));
                            crate::platform::open_url(&url);
                            self.farewell_url = Some(url.clone());
                            DiagStage::Done { url }
                        }
                        Err(e) => DiagStage::Failed(e.to_string()),
                    };
                }
            }
            _ => {}
        }
    }

    fn tick_pawnio(&mut self) {
        let Some(Modal::Pawnio { removing, error }) = &mut self.modal else { return };
        if let Some(task) = removing {
            if let Some(result) = task.take() {
                *removing = None;
                match result {
                    Ok(()) => {
                        self.modal = None;
                        self.quit = true;
                    }
                    Err(e) => *error = Some(e),
                }
            }
        }
    }

    // ── Input ──

    pub fn on_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let hit = self
                    .hits
                    .iter()
                    .rev()
                    .find(|(r, _)| {
                        mouse.column >= r.x
                            && mouse.column < r.x + r.width
                            && mouse.row >= r.y
                            && mouse.row < r.y + r.height
                    })
                    .map(|(_, a)| a.clone());
                if let Some(action) = hit {
                    self.act(action);
                }
            }
            MouseEventKind::ScrollUp => {
                if let Some(d) = &mut self.diag {
                    d.scroll = d.scroll.saturating_add(3);
                }
            }
            MouseEventKind::ScrollDown => {
                if let Some(d) = &mut self.diag {
                    d.scroll = d.scroll.saturating_sub(3);
                }
            }
            _ => {}
        }
    }

    pub fn on_paste(&mut self, text: &str) {
        match &mut self.modal {
            Some(Modal::Feedback(fb)) => match fb.focus {
                FbField::Message => fb.message.insert_str(text),
                FbField::Email => fb.email.insert_str(text),
                _ => {}
            },
            Some(Modal::EditDetails(form)) => {
                let focus = form.focus;
                if let Some(input) = form.text_mut(focus) {
                    input.insert_str(text);
                }
            }
            Some(_) => {}
            None => {
                if self.screen == Screen::Options {
                    let focus = self.form.focus;
                    if let Some(input) = self.form.text_mut(focus) {
                        input.insert_str(text);
                    }
                }
            }
        }
    }

    pub fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            self.request_quit();
            return;
        }
        if self.modal.is_some() {
            self.modal_key(key);
            return;
        }
        if self.screen == Screen::Options && self.options_key(&key) {
            return;
        }
        if self.global_key(&key) {
            return;
        }

        let action = match (self.screen, key.code) {
            (Screen::Home, KeyCode::Enter) => Some(Action::Continue),
            (Screen::Home, KeyCode::Left) => Some(Action::PrevGpu),
            (Screen::Home, KeyCode::Right) => Some(Action::NextGpu),
            (Screen::Home, KeyCode::Char('d' | 'D')) => Some(Action::Diagnostics),
            (Screen::Options, KeyCode::Enter) => Some(Action::Continue),
            (Screen::Options, KeyCode::Esc) => Some(Action::Back),
            (Screen::Confirm, KeyCode::Enter) => Some(Action::StartTest),
            (Screen::Confirm, KeyCode::Esc) => Some(Action::Back),
            (Screen::Running, KeyCode::Esc | KeyCode::Char('q' | 'Q')) => Some(Action::StopTest),
            (Screen::Results, code) => self.results_key(code),
            (Screen::Diagnostics, code) => self.diagnostics_key(code),
            _ => None,
        };
        if let Some(action) = action {
            self.act(action);
        }
    }

    /// Shortcuts available on every screen (when not typing).
    fn global_key(&mut self, key: &KeyEvent) -> bool {
        if self.screen == Screen::Boot {
            if matches!(key.code, KeyCode::Char('q' | 'Q')) {
                self.act(Action::Quit);
            }
            return true;
        }
        let action = match key.code {
            KeyCode::Char('f' | 'F') => Action::Feedback,
            KeyCode::Char('s' | 'S') => Action::Support,
            KeyCode::Char('l' | 'L') => Action::Language,
            KeyCode::Char('?' | 'h' | 'H') | KeyCode::F(1) => Action::Help,
            KeyCode::Char('q' | 'Q') if self.screen != Screen::Running => Action::Quit,
            _ => return false,
        };
        self.act(action);
        true
    }

    fn results_key(&self, code: KeyCode) -> Option<Action> {
        let r = self.results.as_ref()?;
        let done = matches!(r.submit, SubmitState::Done { .. });
        match code {
            KeyCode::Enter => match &r.submit {
                SubmitState::Ready => Some(Action::Submit),
                SubmitState::Failed { retry: true, .. } => Some(Action::Submit),
                SubmitState::Done { .. } => Some(Action::OpenResults),
                _ => Some(Action::RunAgain),
            },
            KeyCode::Char('e' | 'E') if matches!(r.submit, SubmitState::Failed { .. }) => Some(Action::EditDetails),
            KeyCode::Char('o' | 'O') if done => Some(Action::OpenResults),
            KeyCode::Char('c' | 'C') if done => Some(Action::CopyLink),
            KeyCode::Char('r' | 'R') => match &r.submit {
                SubmitState::Failed { retry: true, .. } => Some(Action::Submit),
                SubmitState::Ready | SubmitState::Sending(_) => None,
                _ => Some(Action::RunAgain),
            },
            KeyCode::Esc => match &r.submit {
                SubmitState::Ready | SubmitState::Sending(_) => None,
                _ => Some(Action::RunAgain),
            },
            _ => None,
        }
    }

    fn diagnostics_key(&mut self, code: KeyCode) -> Option<Action> {
        let diag = self.diag.as_mut()?;
        let step = match code {
            KeyCode::Up => Some(1isize),
            KeyCode::Down => Some(-1),
            KeyCode::PageUp => Some(10),
            KeyCode::PageDown => Some(-10),
            _ => None,
        };
        if let Some(step) = step {
            diag.scroll = diag.scroll.saturating_add_signed(step);
            return None;
        }
        let stage = &diag.stage;
        match (stage, code) {
            (DiagStage::Intro, KeyCode::Enter) => Some(Action::StartDiagnostics),
            (DiagStage::Done { .. }, KeyCode::Enter | KeyCode::Char('o' | 'O')) => Some(Action::OpenResults),
            (DiagStage::Done { .. }, KeyCode::Char('c' | 'C')) => Some(Action::CopyLink),
            (DiagStage::Intro | DiagStage::Done { .. } | DiagStage::Failed(_), KeyCode::Esc) => Some(Action::Back),
            (DiagStage::Failed(_), KeyCode::Enter) => Some(Action::Back),
            _ => None,
        }
    }

    /// Options screen keys. Returns true if the key was consumed.
    fn options_key(&mut self, key: &KeyEvent) -> bool {
        let laptop = self.is_laptop();
        let gpu_count = self.gpus().len();
        let fields = self.form.fields(laptop, gpu_count, false);
        let consumed = edit_form(&mut self.form, &fields, key, gpu_count);
        if consumed {
            self.sync_form_gpu(key);
        }
        consumed
    }

    /// Left/Right on the GPU row changes the monitored GPU too.
    fn sync_form_gpu(&mut self, key: &KeyEvent) {
        if self.form.focus == Field::Gpu && matches!(key.code, KeyCode::Left | KeyCode::Right) {
            // edit_form doesn't own gpu_index; apply the step here.
            let n = self.gpus().len();
            if n > 1 {
                let next = if key.code == KeyCode::Left { (self.gpu_index + n - 1) % n } else { (self.gpu_index + 1) % n };
                self.select_gpu(next);
            }
        }
    }

    fn modal_key(&mut self, key: KeyEvent) {
        let Some(modal) = self.modal.take() else { return };
        let (keep, action) = match modal {
            Modal::Help => match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | 'Q' | '?') | KeyCode::F(1) => (None, None),
                _ => (Some(Modal::Help), None),
            },
            Modal::Language { cursor } => match key.code {
                KeyCode::Esc => (None, None),
                KeyCode::Up => (Some(Modal::Language { cursor: cursor.saturating_sub(1) }), None),
                KeyCode::Down => (Some(Modal::Language { cursor: (cursor + 1).min(LANGUAGES.len() - 1) }), None),
                KeyCode::Enter => (None, Some(Action::SetLanguage(cursor))),
                _ => (Some(Modal::Language { cursor }), None),
            },
            Modal::StopTest => match key.code {
                KeyCode::Char('s' | 'S') => (None, Some(Action::ConfirmStop)),
                KeyCode::Enter | KeyCode::Esc => (None, None),
                _ => (Some(Modal::StopTest), None),
            },
            Modal::EditDetails(mut form) => {
                let laptop = self.is_laptop();
                let fields = form.fields(laptop, 0, true);
                match key.code {
                    KeyCode::Esc => (None, None),
                    KeyCode::Enter => (Some(Modal::EditDetails(form)), Some(Action::SaveDetails)),
                    _ => {
                        edit_form(&mut form, &fields, &key, 0);
                        (Some(Modal::EditDetails(form)), None)
                    }
                }
            }
            Modal::Feedback(mut fb) => {
                let action = feedback_key(&mut fb, &key);
                let close = matches!(key.code, KeyCode::Esc)
                    || (matches!(fb.state, FbState::Sent | FbState::Failed(_)) && key.code == KeyCode::Enter);
                if close && !matches!(fb.state, FbState::Sending(_)) {
                    (None, None)
                } else {
                    (Some(Modal::Feedback(fb)), action)
                }
            }
            Modal::Pawnio { removing, error } => {
                if removing.is_some() {
                    (Some(Modal::Pawnio { removing, error }), None)
                } else if error.is_some() {
                    // Uninstall failed: any key closes and quits.
                    self.quit = true;
                    (None, None)
                } else {
                    match key.code {
                        KeyCode::Char('u' | 'U') => (Some(Modal::Pawnio { removing, error }), Some(Action::UninstallPawnio)),
                        KeyCode::Char('k' | 'K') | KeyCode::Enter | KeyCode::Char('q' | 'Q') => {
                            (Some(Modal::Pawnio { removing, error }), Some(Action::KeepPawnio))
                        }
                        KeyCode::Esc => (None, None),
                        _ => (Some(Modal::Pawnio { removing, error }), None),
                    }
                }
            }
        };
        self.modal = keep;
        if let Some(action) = action {
            self.act(action);
        }
    }

    // ── Actions ──

    pub fn act(&mut self, action: Action) {
        match action {
            Action::Continue => match self.screen {
                Screen::Home => {
                    self.form.focus = Field::Test;
                    self.form.error = None;
                    self.screen = Screen::Options;
                }
                Screen::Options => self.confirm_options(),
                _ => {}
            },
            Action::Back => {
                self.screen = match self.screen {
                    Screen::Options | Screen::Diagnostics => Screen::Home,
                    Screen::Confirm => Screen::Options,
                    other => other,
                };
            }
            Action::StartTest => self.start_test(),
            Action::StopTest => {
                if self.session.is_some() {
                    self.modal = Some(Modal::StopTest);
                }
            }
            Action::KeepRunning => self.modal = None,
            Action::ConfirmStop => {
                self.modal = None;
                if let Some(session) = &self.session {
                    session.request_stop();
                }
            }
            Action::Submit => self.submit(),
            Action::EditDetails => {
                let mut form = self.form.clone();
                form.focus = form.fields(self.is_laptop(), 0, true)[0];
                form.error = None;
                self.modal = Some(Modal::EditDetails(form));
            }
            Action::SaveDetails => {
                if let Some(Modal::EditDetails(form)) = &mut self.modal {
                    if form.ambient_celsius().is_err() {
                        form.error = Some(self.t.err_ambient.to_string());
                        return;
                    }
                    self.form = form.clone();
                    self.modal = None;
                    self.save_settings();
                }
            }
            Action::OpenResults => {
                let url = self.current_url();
                if let Some(url) = url {
                    self.open(&url);
                }
            }
            Action::CopyLink => {
                if let Some(url) = self.current_url() {
                    let msg = if crate::platform::copy_to_clipboard(&url) { self.t.copied } else { self.t.copy_failed };
                    self.toast(msg.to_string());
                }
            }
            Action::RunAgain => {
                self.results = None;
                self.screen = Screen::Options;
            }
            Action::Feedback => {
                let context = match self.screen {
                    Screen::Boot | Screen::Home => "home",
                    Screen::Options | Screen::Confirm => "options",
                    Screen::Running => "running",
                    Screen::Results => "results",
                    Screen::Diagnostics => "diagnostics",
                };
                self.modal = Some(Modal::Feedback(FeedbackForm {
                    rating: None,
                    category: 0,
                    message: TextInput::new("", 2000),
                    email: TextInput::new("", 200),
                    include_info: true,
                    focus: FbField::Rating,
                    state: FbState::Editing,
                    error: None,
                    context,
                }));
            }
            Action::Support => {
                let url = api::page_url(&self.site, &self.locale, "/support");
                self.open(&url);
            }
            Action::Help => self.modal = Some(Modal::Help),
            Action::Language => {
                let cursor = LANGUAGES.iter().position(|(c, _)| *c == self.locale).unwrap_or(0);
                self.modal = Some(Modal::Language { cursor });
            }
            Action::SetLanguage(i) => {
                if let Some((code, _)) = LANGUAGES.get(i) {
                    self.locale = code.to_string();
                    self.t = Lang::new(code);
                    self.settings.lang = Some(code.to_string());
                    self.settings.save();
                }
                self.modal = None;
            }
            Action::CloseModal => {
                if let Some(Modal::Feedback(fb)) = &self.modal {
                    if matches!(fb.state, FbState::Sending(_)) {
                        return;
                    }
                }
                self.modal = None;
            }
            Action::Quit => self.request_quit(),
            Action::Diagnostics => self.open_diagnostics(),
            Action::StartDiagnostics => self.start_diagnostics(),
            Action::PrevGpu | Action::NextGpu => {
                let n = self.gpus().len();
                if n > 1 {
                    let next = if action == Action::PrevGpu { (self.gpu_index + n - 1) % n } else { (self.gpu_index + 1) % n };
                    self.select_gpu(next);
                }
            }
            Action::Focus(field) => {
                if let Some(Modal::EditDetails(form)) = &mut self.modal {
                    form.focus = field;
                } else {
                    self.form.focus = field;
                }
            }
            Action::Choose(field, value) => {
                let target = match &mut self.modal {
                    Some(Modal::EditDetails(form)) => form,
                    _ => &mut self.form,
                };
                target.focus = field;
                match field {
                    Field::Test => target.test = value,
                    Field::Duration => target.duration = value,
                    Field::Cooling => target.cooling = value,
                    Field::Gpu => {
                        self.select_gpu(value);
                    }
                    _ => {}
                }
            }
            Action::FbFocus(field) => {
                if let Some(Modal::Feedback(fb)) = &mut self.modal {
                    fb.focus = field;
                }
            }
            Action::FbRating(r) => {
                if let Some(Modal::Feedback(fb)) = &mut self.modal {
                    fb.rating = Some(r);
                    fb.focus = FbField::Rating;
                }
            }
            Action::FbCategory(c) => {
                if let Some(Modal::Feedback(fb)) = &mut self.modal {
                    fb.category = c;
                    fb.focus = FbField::Category;
                }
            }
            Action::FbToggleInclude => {
                if let Some(Modal::Feedback(fb)) = &mut self.modal {
                    fb.include_info = !fb.include_info;
                    fb.focus = FbField::Include;
                }
            }
            Action::FbSend => self.send_feedback(),
            Action::KeepPawnio => {
                self.pawnio_decided = true;
                self.modal = None;
                self.quit = true;
            }
            Action::UninstallPawnio => {
                self.pawnio_decided = true;
                #[cfg(windows)]
                let task = Task::spawn("pawnio-uninstall", Err("unexpected error".into()), crate::lhm::uninstall_pawnio);
                #[cfg(not(windows))]
                let task = Task::spawn("pawnio-uninstall", Ok(()), || Ok(()));
                self.modal = Some(Modal::Pawnio { removing: Some(task), error: None });
            }
        }
    }

    fn open(&mut self, url: &str) {
        let msg = if crate::platform::open_url(url) {
            fill(self.t.toast_opened, &[("url", url)])
        } else {
            fill(self.t.toast_open_failed, &[("url", url)])
        };
        self.toast(msg);
    }

    fn current_url(&self) -> Option<String> {
        if self.screen == Screen::Diagnostics {
            if let Some(DiagState { stage: DiagStage::Done { url }, .. }) = &self.diag {
                return Some(url.clone());
            }
        }
        match &self.results.as_ref()?.submit {
            SubmitState::Done { url } => Some(url.clone()),
            _ => None,
        }
    }

    fn select_gpu(&mut self, index: usize) {
        if index >= self.gpus().len() || index == self.gpu_index {
            return;
        }
        self.gpu_index = index;
        let gpu = self.selected_gpu().cloned();
        if let Some(hub) = &self.hub {
            hub.select_gpu(gpu);
        }
    }

    fn request_quit(&mut self) {
        if self.session.is_some() {
            self.modal = Some(Modal::StopTest);
            return;
        }
        let installed_now = self.setup.as_ref().is_some_and(|s| s.driver == DriverState::InstalledNow);
        if installed_now && !self.pawnio_decided && cfg!(windows) {
            self.modal = Some(Modal::Pawnio { removing: None, error: None });
            return;
        }
        self.quit = true;
    }

    fn confirm_options(&mut self) {
        if self.form.duration_secs().is_none() {
            self.form.error = Some(self.t.err_duration.to_string());
            self.form.focus = Field::CustomSecs;
            return;
        }
        if self.form.ambient_celsius().is_err() {
            self.form.error = Some(self.t.err_ambient.to_string());
            self.form.focus = Field::Ambient;
            return;
        }
        if self.form.kind().gpu() && self.gpus().is_empty() {
            self.form.test = TEST_KINDS.iter().position(|k| *k == TestKind::Cpu).unwrap();
        }
        self.form.error = None;
        self.save_settings();
        self.screen = Screen::Confirm;
    }

    fn save_settings(&mut self) {
        let laptop = self.is_laptop();
        self.settings.test_type = Some(self.form.kind().as_str().to_string());
        self.settings.duration_secs = self.form.duration_secs();
        self.settings.gpu_name = self.selected_gpu().map(|g| g.name.clone());
        if laptop {
            self.settings.laptop_model = self.form.laptop_model.trimmed();
        } else {
            self.settings.cooling_type = Some(COOLING[self.form.cooling].to_string()).filter(|c| !c.is_empty());
            self.settings.cooling_model = self.form.cooler_model.trimmed();
        }
        self.settings.ambient_temp = self.form.ambient_celsius().ok().flatten();
        self.settings.save();
    }

    fn start_test(&mut self) {
        let Some(hub) = &self.hub else { return };
        let kind = self.form.kind();
        let plan = TestPlan {
            kind,
            duration: Duration::from_secs(self.form.duration_secs().unwrap_or(120)),
            gpu: if kind.gpu() { self.selected_gpu().cloned() } else { None },
        };
        self.results = None;
        self.session = Some(Session::start(plan, hub.clone()));
        self.screen = Screen::Running;
    }

    /// Whether a completed test will be submitted. Demo results are
    /// simulated, so they may only go to a local development server.
    pub fn submits(&self) -> bool {
        let local = self.site.contains("://localhost") || self.site.contains("://127.0.0.1");
        !self.opts.no_submit && (!self.opts.demo || local)
    }

    fn show_results(&mut self, plan: TestPlan, progress: Progress) {
        let verdict = engine::verdict(&plan, &progress);
        let local = self.site.contains("://localhost") || self.site.contains("://127.0.0.1");
        let blocked = if self.opts.demo && !local {
            Some(self.t.reason_demo)
        } else if self.opts.no_submit {
            Some(self.t.reason_no_submit)
        } else if progress.stopped_early {
            Some(self.t.reason_stopped)
        } else if verdict.kind.is_none() {
            Some(self.t.reason_no_temps)
        } else {
            None
        };
        let submit = match blocked {
            Some(reason) => SubmitState::Blocked(fill(self.t.not_submittable, &[("reason", reason)])),
            None => SubmitState::Ready,
        };
        self.results = Some(ResultsState { plan, progress, verdict, submit, compare: CompareState::Idle });
        self.screen = Screen::Results;
        // A completed, valid test is submitted right away.
        self.submit();
    }

    fn payload(&self, r: &ResultsState, kind: TestKind) -> SubmissionPayload {
        let hw = self.hw.as_ref();
        let laptop = self.is_laptop();
        // The tested GPU; for CPU-only tests the selected one, as v1 always sent
        // a GPU model. (GPU temperatures are only sent when the GPU was loaded.)
        let gpu = r.plan.gpu.as_ref().or_else(|| self.selected_gpu());
        let p = &r.progress;
        SubmissionPayload {
            test_type: kind.as_str().to_string(),
            stress_method: "cli_tool".to_string(),
            cpu_model: hw.and_then(|h| h.cpu_model.clone()),
            cpu_cores: hw.and_then(|h| h.cpu_cores),
            cpu_threads: hw.and_then(|h| h.cpu_threads),
            gpu_model: gpu.map(|g| g.name.clone()),
            gpu_vram: gpu.and_then(|g| g.vram()),
            os: hw.and_then(|h| h.os.clone()),
            device_type: Some(if laptop { "laptop" } else { "desktop" }.to_string()),
            laptop_model: if laptop { self.form.laptop_model.trimmed() } else { None },
            cooling_type: self.form.cooling_type(laptop, &self.opts),
            cooling_model: if laptop {
                self.opts.cooling_model.clone()
            } else {
                self.form.cooler_model.trimmed()
            },
            ambient_temp: self.form.ambient_celsius().ok().flatten(),
            cpu_temp_idle: kind.cpu().then_some(p.cpu.idle).flatten(),
            cpu_temp_load: kind.cpu().then_some(p.cpu.peak).flatten(),
            gpu_temp_idle: kind.gpu().then_some(p.gpu.idle).flatten(),
            gpu_temp_load: kind.gpu().then_some(p.gpu.peak).flatten(),
            cpu_usage_max: kind.cpu().then_some(p.cpu.usage_max).flatten().map(round1),
            gpu_usage_max: kind.gpu().then_some(p.gpu.usage_max).flatten().map(round1),
            test_duration: Some(r.plan.duration.as_secs() as i64),
            cli_version: Some(VERSION.to_string()),
            session_id: Some(self.machine_id.clone()),
        }
    }

    fn submit(&mut self) {
        let Some(r) = &self.results else { return };
        if !matches!(r.submit, SubmitState::Ready | SubmitState::Failed { retry: true, .. }) {
            return;
        }
        let Some(kind) = r.verdict.kind else { return };
        let payload = self.payload(r, kind);
        let site = self.site.clone();
        let task = Task::spawn("submit", Err(ApiError::Connection("unexpected error".into())), move || {
            api::submit_results(&site, &payload)
        });
        self.results.as_mut().unwrap().submit = SubmitState::Sending(task);
    }

    fn start_compare(&mut self) {
        let Some(r) = &self.results else { return };
        let Some(kind) = r.verdict.kind else { return };
        let payload = self.payload(r, kind);
        let request = CompareRequest {
            test_type: payload.test_type,
            cpu_model: payload.cpu_model,
            gpu_model: payload.gpu_model,
            cpu_temp_load: payload.cpu_temp_load,
            cpu_temp_idle: payload.cpu_temp_idle,
            gpu_temp_load: payload.gpu_temp_load,
            gpu_temp_idle: payload.gpu_temp_idle,
        };
        let site = self.site.clone();
        let task = Task::spawn("compare", Err(ApiError::Connection("unexpected error".into())), move || {
            api::compare(&site, &request)
        });
        self.results.as_mut().unwrap().compare = CompareState::Loading(task);
    }

    fn send_feedback(&mut self) {
        let empty = self.t.fb_empty;
        let diagnostics = self.feedback_diagnostics();
        let gpu_model = self.selected_gpu().map(|g| g.name.clone());
        let Some(Modal::Feedback(fb)) = &mut self.modal else { return };
        if matches!(fb.state, FbState::Sending(_) | FbState::Sent) {
            return;
        }
        let Some(message) = fb.message.trimmed() else {
            fb.error = Some(empty.to_string());
            fb.focus = FbField::Message;
            return;
        };
        fb.error = None;
        let hw = self.hw.as_ref();
        let payload = FeedbackPayload {
            rating: fb.rating,
            category: FB_CATEGORIES[fb.category].to_string(),
            message,
            email: fb.email.trimmed(),
            cli_version: VERSION.to_string(),
            os: hw.and_then(|h| h.os.clone()),
            cpu_model: hw.and_then(|h| h.cpu_model.clone()),
            gpu_model,
            locale: self.locale.clone(),
            context: fb.context.to_string(),
            diagnostics: if fb.include_info { Some(diagnostics) } else { None },
            session_id: Some(self.machine_id.clone()).filter(|s| !s.is_empty()),
        };
        let site = self.site.clone();
        fb.state = FbState::Sending(Task::spawn(
            "feedback",
            Err(ApiError::Connection("unexpected error".into())),
            move || api::send_feedback(&site, &payload),
        ));
    }

    /// Short system summary attached to feedback when the user allows it.
    fn feedback_diagnostics(&self) -> String {
        let mut lines = Vec::new();
        if let Some(hw) = &self.hw {
            lines.push(format!("Device: {}", if hw.is_laptop { "laptop" } else { "desktop" }));
            for (i, g) in hw.gpus.iter().enumerate() {
                lines.push(format!(
                    "GPU {}{}: {}{}",
                    i,
                    if i == self.gpu_index { " [selected]" } else { "" },
                    g.name,
                    g.vram().map(|v| format!(" ({})", v)).unwrap_or_default()
                ));
            }
        }
        if let Some(setup) = &self.setup {
            lines.push(format!(
                "Admin: {} | Monitoring app: {} | Driver: {:?}",
                setup.elevated,
                setup.monitoring_app.unwrap_or("none"),
                setup.driver
            ));
        }
        if let Some(hub) = &self.hub {
            hub.with(|r| {
                lines.push(format!(
                    "CPU sensor: {:?} {} | GPU sensor: {:?} {}",
                    r.cpu_temp.probe,
                    r.cpu_temp.source.as_deref().unwrap_or(""),
                    r.gpu_temp.probe,
                    r.gpu_temp.source.as_deref().unwrap_or("")
                ));
            });
        }
        if let Some(r) = &self.results {
            let p = &r.progress;
            lines.push(format!(
                "Last test: {} {}s | CPU idle {:?} peak {:?} | GPU idle {:?} peak {:?} | GPU stress {:?} | warnings {:?}",
                r.plan.kind.as_str(),
                r.plan.duration.as_secs(),
                p.cpu.idle,
                p.cpu.peak,
                p.gpu.idle,
                p.gpu.peak,
                p.stress.gpu,
                p.warnings
            ));
        }
        lines.join("\n")
    }

    // ── Diagnostics ──

    fn open_diagnostics(&mut self) {
        if !matches!(self.diag.as_ref().map(|d| &d.stage), Some(DiagStage::Collecting(_) | DiagStage::Stress | DiagStage::Uploading(_))) {
            self.diag = Some(DiagState {
                lines: Arc::new(Mutex::new(Vec::new())),
                stage: DiagStage::Intro,
                scroll: 0,
                saved: None,
            });
        }
        self.screen = Screen::Diagnostics;
    }

    fn start_diagnostics(&mut self) {
        let (Some(hw), Some(setup)) = (self.hw.clone(), self.setup.clone()) else { return };
        let Some(diag) = &mut self.diag else { return };
        let lines = diag.lines.clone();
        let gpu_index = self.gpu_index;
        diag.stage = DiagStage::Collecting(Task::spawn("diagnostics", (), move || {
            let mut emit = |line: String| lines.lock().unwrap_or_else(|e| e.into_inner()).push(line);
            crate::diagnostics::collect(&hw, &setup, Some(gpu_index), &mut emit);
        }));
    }

    fn finish_diagnostics(&mut self, plan: &TestPlan, progress: &Progress) {
        let hw = self.hw.clone();
        let gpu = self.selected_gpu().map(|g| g.name.clone());
        let site = self.site.clone();
        let Some(diag) = &mut self.diag else { return };
        {
            let mut lines = diag.lines.lock().unwrap_or_else(|e| e.into_inner());
            let mut emit = |line: String| lines.push(line);
            crate::diagnostics::summarize(plan, progress, &mut emit);
        }
        let log = diag.lines.lock().unwrap_or_else(|e| e.into_inner()).join("\n");

        // Keep a local copy in case the upload fails.
        if let Some(dir) = crate::platform::data_dir() {
            let path = dir.join(format!("diagnostics-{}.txt", chrono::Local::now().format("%Y%m%d-%H%M%S")));
            if std::fs::create_dir_all(&dir).is_ok() && std::fs::write(&path, &log).is_ok() {
                diag.saved = Some(path);
            }
        }

        let payload = api::DebugLogPayload {
            log,
            cpu_model: hw.as_ref().and_then(|h| h.cpu_model.clone()),
            gpu_model: gpu,
            os: hw.as_ref().and_then(|h| h.os.clone()),
            cli_version: Some(VERSION.to_string()),
        };
        diag.stage = DiagStage::Uploading(Task::spawn(
            "diagnostics-upload",
            Err(ApiError::Connection("unexpected error".into())),
            move || api::submit_debug_log(&site, &payload),
        ));
        self.screen = Screen::Diagnostics;
    }
}

/// Keyboard editing shared by the options screen and the edit dialog.
/// Returns true if the key was consumed.
fn edit_form(form: &mut Form, fields: &[Field], key: &KeyEvent, gpu_count: usize) -> bool {
    let pos = fields.iter().position(|f| *f == form.focus).unwrap_or(0);
    let focus = fields.get(pos).copied().unwrap_or(Field::Test);
    form.focus = focus;

    match key.code {
        KeyCode::Up | KeyCode::BackTab => {
            form.focus = fields[pos.saturating_sub(1)];
            return true;
        }
        KeyCode::Down | KeyCode::Tab => {
            form.focus = fields[(pos + 1).min(fields.len() - 1)];
            return true;
        }
        _ => {}
    }

    if focus.is_text() {
        // Numeric fields only take characters that can form a valid value.
        if let KeyCode::Char(c) = key.code {
            let allowed = match focus {
                Field::CustomSecs => c.is_ascii_digit(),
                Field::Ambient => c.is_ascii_digit() || ".,-°fFcC ".contains(c),
                _ => true,
            };
            if !allowed {
                return true;
            }
        }
        let input = form.text_mut(focus).unwrap();
        if input.handle(key) {
            form.error = None;
            return true;
        }
        return false;
    }

    let count = Form::choice_count(focus, gpu_count);
    if count == 0 {
        return false;
    }
    let step = |current: usize, left: bool| if left { (current + count - 1) % count } else { (current + 1) % count };
    match key.code {
        KeyCode::Left | KeyCode::Right => {
            let left = key.code == KeyCode::Left;
            match focus {
                Field::Test => form.test = step(form.test, left),
                Field::Duration => form.duration = step(form.duration, left),
                Field::Cooling => form.cooling = step(form.cooling, left),
                Field::Gpu => {} // applied by the caller
                _ => {}
            }
            form.error = None;
            true
        }
        _ => false,
    }
}

fn feedback_key(fb: &mut FeedbackForm, key: &KeyEvent) -> Option<Action> {
    if matches!(fb.state, FbState::Sending(_) | FbState::Sent) {
        return None;
    }
    let pos = FB_FIELDS.iter().position(|f| *f == fb.focus).unwrap_or(0);
    match key.code {
        KeyCode::Tab | KeyCode::Down => {
            fb.focus = FB_FIELDS[(pos + 1).min(FB_FIELDS.len() - 1)];
            return None;
        }
        KeyCode::BackTab | KeyCode::Up => {
            fb.focus = FB_FIELDS[pos.saturating_sub(1)];
            return None;
        }
        KeyCode::Enter => {
            if fb.focus == FbField::Send {
                return Some(Action::FbSend);
            }
            fb.focus = FB_FIELDS[(pos + 1).min(FB_FIELDS.len() - 1)];
            return None;
        }
        _ => {}
    }
    match fb.focus {
        FbField::Rating => match key.code {
            KeyCode::Char(c @ '1'..='5') => fb.rating = Some(c as u8 - b'0'),
            KeyCode::Left => fb.rating = Some(fb.rating.unwrap_or(1).saturating_sub(1).max(1)),
            KeyCode::Right => fb.rating = Some((fb.rating.unwrap_or(0) + 1).min(5)),
            _ => {}
        },
        FbField::Category => match key.code {
            KeyCode::Left => fb.category = (fb.category + FB_CATEGORIES.len() - 1) % FB_CATEGORIES.len(),
            KeyCode::Right => fb.category = (fb.category + 1) % FB_CATEGORIES.len(),
            _ => {}
        },
        // The message is edited at the end only; it wraps across lines.
        FbField::Message => match key.code {
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                fb.message.cursor = fb.message.value.chars().count();
                fb.message.insert(c);
                fb.error = None;
            }
            KeyCode::Backspace => {
                fb.message.cursor = fb.message.value.chars().count();
                fb.message.backspace();
            }
            _ => {}
        },
        FbField::Email => {
            fb.email.handle(key);
        }
        FbField::Include => {
            if matches!(key.code, KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right) {
                fb.include_info = !fb.include_info;
            }
        }
        FbField::Send => {}
    }
    None
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// Deterministic machine ID from hostname + CPU model (same as v1), so one
/// machine counts as one contributor and the daily limit applies per machine.
pub fn machine_id(hw: &HardwareInfo) -> String {
    let hostname = hostname::get().map(|h| h.to_string_lossy().to_string()).unwrap_or_default();
    let cpu = hw.cpu_model.as_deref().unwrap_or("");
    // Simple hash: djb2
    let input = format!("{}:{}", hostname, cpu);
    let mut hash: u64 = 5381;
    for b in input.bytes() {
        hash = hash.wrapping_mul(33).wrapping_add(b as u64);
    }
    format!("cli-{:016x}", hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> LaunchOptions {
        LaunchOptions {
            api_url: api::DEFAULT_API_URL.into(),
            no_submit: false,
            demo: false,
            test: None,
            duration: None,
            cooling_type: None,
            cooling_model: None,
            ambient_temp: None,
            diagnostics: false,
        }
    }

    #[test]
    fn parses_room_temperature() {
        let mut form = Form::new(&Settings::default(), &opts());
        form.ambient = TextInput::new("72f", 8);
        assert_eq!(form.ambient_celsius(), Ok(Some(22.2)));
        form.ambient = TextInput::new("22,5", 8);
        assert_eq!(form.ambient_celsius(), Ok(Some(22.5)));
        form.ambient = TextInput::new("", 8);
        assert_eq!(form.ambient_celsius(), Ok(None));
        form.ambient = TextInput::new("hot", 8);
        assert!(form.ambient_celsius().is_err());
        form.ambient = TextInput::new("95", 8);
        assert!(form.ambient_celsius().is_err());
    }

    #[test]
    fn custom_duration_bounds() {
        let mut form = Form::new(&Settings::default(), &opts());
        form.duration = DURATIONS.len();
        form.custom_secs = TextInput::new("29", 4);
        assert_eq!(form.duration_secs(), None);
        form.custom_secs = TextInput::new("90", 4);
        assert_eq!(form.duration_secs(), Some(90));
    }

    #[test]
    fn launch_flags_prefill_the_form() {
        let mut o = opts();
        o.duration = Some(180);
        o.cooling_type = Some("aio".into());
        let form = Form::new(&Settings::default(), &o);
        assert_eq!(form.duration_secs(), Some(180));
        assert_eq!(COOLING[form.cooling], "aio");
    }

    #[test]
    fn text_input_edits_at_cursor() {
        let mut t = TextInput::new("ac", 10);
        t.cursor = 1;
        t.insert('b');
        assert_eq!(t.value, "abc");
        t.backspace();
        assert_eq!(t.value, "ac");
    }
}
