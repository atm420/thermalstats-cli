//! One stress-test run, driven by its own thread on a fixed schedule:
//! idle baseline → stress → a short cool-down window for lagging sensors.
//!
//! The finish time is fixed when the stress starts and never moves: sensor
//! reads happen on other threads and are only *collected* here, by
//! timestamp. So the progress bar can always be drawn from the clock alone.

use crate::gpus::GpuDevice;
use crate::sensors::{Channel, SensorHub};
use crate::series::{PartSamples, Series};
use crate::stress::{GpuStress, StressRun, StressStatus};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Temperatures rise within a second or two of load, but some sensors
/// report a little late — keep collecting briefly after the stop.
const SENSOR_LAG: Duration = Duration::from_millis(1500);
/// Idle temperature = median of the readings in this window before the start.
const IDLE_WINDOW: Duration = Duration::from_secs(10);
/// Checks for a sensor that doesn't react to load wait this long.
const FLAT_CHECK_AFTER: Duration = Duration::from_secs(30);
/// Idle temperatures above these suggest background load or a system that
/// hasn't cooled down yet — the test should start from idle.
pub const IDLE_WARM: f64 = 56.0;
pub const IDLE_HOT: f64 = 65.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleLevel {
    Warm,
    Hot,
}

pub fn idle_level(celsius: f64) -> Option<IdleLevel> {
    if celsius > IDLE_HOT {
        Some(IdleLevel::Hot)
    } else if celsius > IDLE_WARM {
        Some(IdleLevel::Warm)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestKind {
    Both,
    Cpu,
    Gpu,
}

impl TestKind {
    pub fn cpu(self) -> bool {
        matches!(self, TestKind::Both | TestKind::Cpu)
    }
    pub fn gpu(self) -> bool {
        matches!(self, TestKind::Both | TestKind::Gpu)
    }
    /// Value used by the API and the command line.
    pub fn as_str(self) -> &'static str {
        match self {
            TestKind::Both => "both",
            TestKind::Cpu => "cpu",
            TestKind::Gpu => "gpu",
        }
    }
    pub fn parse(s: &str) -> Option<TestKind> {
        match s {
            "both" => Some(TestKind::Both),
            "cpu" => Some(TestKind::Cpu),
            "gpu" => Some(TestKind::Gpu),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TestPlan {
    pub kind: TestKind,
    pub duration: Duration,
    pub gpu: Option<GpuDevice>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Measuring the idle baseline and starting the workers.
    Starting,
    Running,
    /// Workers stopping; still collecting late sensor readings.
    Stopping,
    Finished,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Warning {
    /// Temperature barely moved under full load — likely a motherboard sensor.
    CpuTempFlat,
    GpuTempFlat,
    CpuSensorMissing,
    GpuSensorMissing,
    GpuStressFailed(String),
    /// GPU stress is running but the GPU reports low utilisation.
    GpuLoadLow(f64),
    /// The CPU got hot enough that it is probably throttling.
    CpuVeryHot(f64),
    GpuVeryHot(f64),
    /// Already warm/hot before the stress started (idle temperature).
    CpuIdleWarm(f64),
    CpuIdleHot(f64),
    GpuIdleWarm(f64),
    GpuIdleHot(f64),
}

/// Temperatures and load for one component (CPU or GPU).
#[derive(Debug, Clone, Default)]
pub struct Part {
    pub idle: Option<f64>,
    pub current: Option<f64>,
    pub peak: Option<f64>,
    /// Lowest reading while under load (for the flat-sensor check).
    pub low: Option<f64>,
    pub usage_now: Option<f64>,
    pub usage_max: Option<f64>,
}

impl Part {
    pub fn rise(&self) -> Option<f64> {
        Some(self.peak? - self.idle?)
    }

    /// Idle and peak both present and different — what the API requires.
    pub fn is_valid(&self) -> bool {
        match (self.idle, self.peak) {
            (Some(idle), Some(peak)) => peak > idle && idle > 0.0 && peak <= 125.0,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub phase: Phase,
    /// When the stress workers started / when the test will end.
    pub started: Option<Instant>,
    pub ends: Option<Instant>,
    pub ends_wall: Option<chrono::DateTime<chrono::Local>>,
    pub stopped: Option<Instant>,
    pub cpu: Part,
    pub gpu: Part,
    pub stress: StressStatus,
    pub warnings: Vec<Warning>,
    /// Stopped by the user before the planned end.
    pub stopped_early: bool,
    /// The whole run's temperature/load curve, set when the test finishes.
    pub series: Option<Series>,
}

pub struct Session {
    pub plan: TestPlan,
    progress: Arc<Mutex<Progress>>,
    stop: Arc<AtomicBool>,
}

impl Session {
    pub fn start(plan: TestPlan, hub: Arc<SensorHub>) -> Session {
        let progress = Arc::new(Mutex::new(Progress {
            phase: Phase::Starting,
            started: None,
            ends: None,
            ends_wall: None,
            stopped: None,
            cpu: Part::default(),
            gpu: Part::default(),
            stress: StressStatus { cpu_threads: 0, gpu: GpuStress::Off },
            warnings: Vec::new(),
            stopped_early: false,
            series: None,
        }));
        let stop = Arc::new(AtomicBool::new(false));

        {
            let plan = plan.clone();
            let progress = progress.clone();
            let stop = stop.clone();
            let _ = std::thread::Builder::new()
                .name("test-controller".into())
                .spawn(move || {
                    crate::platform::raise_thread_priority();
                    run(&plan, &hub, &progress, &stop);
                });
        }

        Session { plan, progress, stop }
    }

    pub fn progress(&self) -> Progress {
        self.progress.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Stop early. The controller winds down on its next tick.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
impl Session {
    /// A session frozen at `progress`, with no controller thread (screenshots).
    pub fn fixed(plan: TestPlan, progress: Progress) -> Session {
        Session { plan, progress: Arc::new(Mutex::new(progress)), stop: Arc::new(AtomicBool::new(false)) }
    }
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(values[values.len() / 2])
}

fn idle_of(channel: &Channel, before: Instant) -> Option<f64> {
    let from = before.checked_sub(IDLE_WINDOW).unwrap_or(before);
    let recent: Vec<f64> = channel
        .samples
        .iter()
        .filter(|s| s.at >= from && s.at <= before)
        .map(|s| s.value)
        .collect();
    // Nothing recent: fall back to the last reading before the start.
    median(recent).or_else(|| {
        channel.samples.iter().rev().find(|s| s.at <= before).map(|s| s.value)
    })
}

/// Fold the samples taken in [from, until] into peak/low/usage figures.
fn collect(part: &mut Part, temp: &Channel, usage: &Channel, from: Instant, until: Instant) {
    let temps: Vec<f64> = temp.since(from).filter(|s| s.at <= until).map(|s| s.value).collect();
    if let Some(peak) = temps.iter().copied().reduce(f64::max) {
        part.peak = Some(peak);
    }
    if let Some(low) = temps.iter().copied().reduce(f64::min) {
        part.low = Some(low);
    }
    part.current = temp.latest().map(|s| s.value);
    let usages: Vec<f64> = usage.since(from).filter(|s| s.at <= until).map(|s| s.value).collect();
    if let Some(max) = usages.iter().copied().reduce(f64::max) {
        part.usage_max = Some(max);
    }
    part.usage_now = usage.latest().map(|s| s.value);
}

fn push_once(warnings: &mut Vec<Warning>, w: Warning) {
    let same_kind = |a: &Warning| std::mem::discriminant(a) == std::mem::discriminant(&w);
    if let Some(existing) = warnings.iter_mut().find(|a| same_kind(a)) {
        *existing = w; // keep the latest figure
    } else {
        warnings.push(w);
    }
}

fn run(plan: &TestPlan, hub: &SensorHub, progress: &Mutex<Progress>, stop: &AtomicBool) {
    let lock = || progress.lock().unwrap_or_else(|e| e.into_inner());

    // ── Idle baseline ──
    // The sensors have been sampling since the app opened, so there is
    // normally a window of idle readings already. If a sensor is still
    // warming up, give it a few seconds.
    let wait_until = Instant::now() + Duration::from_secs(4);
    loop {
        let ready = hub.with(|r| {
            (!plan.kind.cpu() || r.cpu_temp.latest().is_some() || r.cpu_temp.probe == crate::sensors::Probe::Missing)
                && (!plan.kind.gpu() || r.gpu_temp.latest().is_some() || r.gpu_temp.probe == crate::sensors::Probe::Missing)
        });
        if ready || Instant::now() >= wait_until || stop.load(Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let now = Instant::now();
    let (cpu_idle, gpu_idle) = hub.with(|r| (idle_of(&r.cpu_temp, now), idle_of(&r.gpu_temp, now)));
    {
        let mut p = lock();
        p.cpu.idle = cpu_idle;
        p.gpu.idle = gpu_idle;
        if plan.kind.cpu() && cpu_idle.is_none() {
            p.warnings.push(Warning::CpuSensorMissing);
        }
        if plan.kind.gpu() && gpu_idle.is_none() {
            p.warnings.push(Warning::GpuSensorMissing);
        }
        if plan.kind.cpu() {
            match cpu_idle.and_then(|t| idle_level(t).map(|l| (l, t))) {
                Some((IdleLevel::Hot, t)) => p.warnings.push(Warning::CpuIdleHot(t)),
                Some((IdleLevel::Warm, t)) => p.warnings.push(Warning::CpuIdleWarm(t)),
                None => {}
            }
        }
        if plan.kind.gpu() {
            match gpu_idle.and_then(|t| idle_level(t).map(|l| (l, t))) {
                Some((IdleLevel::Hot, t)) => p.warnings.push(Warning::GpuIdleHot(t)),
                Some((IdleLevel::Warm, t)) => p.warnings.push(Warning::GpuIdleWarm(t)),
                None => {}
            }
        }
    }

    if stop.load(Ordering::SeqCst) {
        let mut p = lock();
        p.stopped_early = true;
        p.phase = Phase::Finished;
        return;
    }

    // ── Stress ──
    let stress = StressRun::start(plan.kind.cpu(), plan.gpu.clone(), plan.kind.gpu());
    let started = Instant::now();
    let ends = started + plan.duration;
    {
        let mut p = lock();
        p.phase = Phase::Running;
        p.started = Some(started);
        p.ends = Some(ends);
        p.ends_wall = chrono::Local::now().checked_add_signed(
            chrono::Duration::from_std(plan.duration).unwrap_or_default(),
        );
    }

    let tick = Duration::from_millis(250);
    let mut stopped_early = false;
    loop {
        if stop.load(Ordering::SeqCst) {
            stopped_early = true;
            break;
        }
        let now = Instant::now();
        if now >= ends {
            break;
        }

        let status = stress.status();
        let mut p = lock();
        hub.with(|r| {
            collect(&mut p.cpu, &r.cpu_temp, &r.cpu_usage, started, now);
            collect(&mut p.gpu, &r.gpu_temp, &r.gpu_usage, started, now);
        });
        update_warnings(&mut p, plan, &status, now - started);
        p.stress = status;
        drop(p);

        std::thread::sleep(tick.min(ends.saturating_duration_since(Instant::now())));
    }

    // ── Stop and collect late readings ──
    let stopped = Instant::now();
    {
        let mut p = lock();
        p.phase = Phase::Stopping;
        p.stopped = Some(stopped);
        p.stopped_early = stopped_early;
    }
    let final_status = stress.stop(Duration::from_secs(5));
    let settle = stopped + SENSOR_LAG;
    let now = Instant::now();
    if now < settle {
        std::thread::sleep(settle - now);
    }

    let mut p = lock();
    let until = Instant::now();
    hub.with(|r| {
        collect(&mut p.cpu, &r.cpu_temp, &r.cpu_usage, started, until);
        collect(&mut p.gpu, &r.gpu_temp, &r.gpu_usage, started, until);
        p.series = Series::build(
            plan.kind.cpu().then_some(PartSamples { temp: &r.cpu_temp.samples, usage: &r.cpu_usage.samples }),
            plan.kind.gpu().then_some(PartSamples { temp: &r.gpu_temp.samples, usage: &r.gpu_usage.samples }),
            started,
            stopped,
        );
    });
    let ran = stopped - started;
    update_warnings(&mut p, plan, &final_status, ran);
    p.stress = final_status;
    p.phase = Phase::Finished;
}

fn update_warnings(p: &mut Progress, plan: &TestPlan, status: &StressStatus, elapsed: Duration) {
    if plan.kind.cpu() {
        if let Some(peak) = p.cpu.peak {
            if peak >= 95.0 {
                push_once(&mut p.warnings, Warning::CpuVeryHot(peak));
            }
        }
        // Full load for a while but the reading hasn't moved: that's not the die.
        if elapsed >= FLAT_CHECK_AFTER && p.cpu.usage_max.unwrap_or(0.0) > 80.0 {
            let rise = p.cpu.rise().unwrap_or(f64::MAX);
            let spread = match (p.cpu.peak, p.cpu.low) {
                (Some(hi), Some(lo)) => hi - lo,
                _ => f64::MAX,
            };
            if rise < 3.0 && spread < 3.0 {
                push_once(&mut p.warnings, Warning::CpuTempFlat);
            }
        }
    }

    if plan.kind.gpu() {
        match &status.gpu {
            GpuStress::Failed(reason) => {
                push_once(&mut p.warnings, Warning::GpuStressFailed(reason.clone()));
            }
            GpuStress::Running { .. } => {
                if elapsed >= FLAT_CHECK_AFTER {
                    if let Some(max) = p.gpu.usage_max {
                        if max < 50.0 {
                            push_once(&mut p.warnings, Warning::GpuLoadLow(max));
                        }
                    }
                    if p.gpu.rise().is_some_and(|r| r < 3.0) {
                        push_once(&mut p.warnings, Warning::GpuTempFlat);
                    }
                }
            }
            _ => {}
        }
        if let Some(peak) = p.gpu.peak {
            if peak >= 90.0 {
                push_once(&mut p.warnings, Warning::GpuVeryHot(peak));
            }
        }
    }
}

/// What can be submitted from a finished run.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub kind: Option<TestKind>,
    pub cpu_ok: bool,
    pub gpu_ok: bool,
}

pub fn verdict(plan: &TestPlan, p: &Progress) -> Verdict {
    let gpu_stressed = matches!(p.stress.gpu, GpuStress::Running { .. });
    let cpu_ok = plan.kind.cpu() && p.cpu.is_valid();
    let gpu_ok = plan.kind.gpu() && gpu_stressed && p.gpu.is_valid();
    let kind = if p.stopped_early || p.phase != Phase::Finished {
        None
    } else {
        match (cpu_ok, gpu_ok) {
            (true, true) => Some(TestKind::Both),
            (true, false) => Some(TestKind::Cpu),
            (false, true) => Some(TestKind::Gpu),
            (false, false) => None,
        }
    };
    Verdict { kind, cpu_ok, gpu_ok }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_picks_middle() {
        assert_eq!(median(vec![3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(vec![]), None);
    }

    #[test]
    fn part_validity_matches_api_rules() {
        let mut p = Part { idle: Some(40.0), peak: Some(80.0), ..Default::default() };
        assert!(p.is_valid());
        p.peak = Some(40.0); // unchanged → rejected by the API
        assert!(!p.is_valid());
        p.peak = Some(130.0); // above the API maximum
        assert!(!p.is_valid());
    }

    #[test]
    fn idle_thresholds() {
        assert_eq!(idle_level(56.0), None);
        assert_eq!(idle_level(56.5), Some(IdleLevel::Warm));
        assert_eq!(idle_level(65.0), Some(IdleLevel::Warm));
        assert_eq!(idle_level(65.1), Some(IdleLevel::Hot));
    }

    #[test]
    fn warnings_are_not_duplicated() {
        let mut w = vec![Warning::CpuVeryHot(95.0)];
        push_once(&mut w, Warning::CpuVeryHot(97.0));
        assert_eq!(w, vec![Warning::CpuVeryHot(97.0)]);
    }
}
