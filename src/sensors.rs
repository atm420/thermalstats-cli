//! Live sensor readings, sampled on background threads.
//!
//! Each metric has its own thread, so a slow source (a sensor helper that
//! takes seconds to answer while the CPU is saturated) never delays the
//! others — or the interface, which only ever reads the latest values.
//! Every sample is timestamped, which lets the test controller attribute it
//! to the idle, load or cool-down window regardless of when it arrived.

use crate::gpus::GpuDevice;
use crate::temps::{self, Context};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

pub const INTERVAL: Duration = Duration::from_millis(1000);
/// One hour at 1 Hz — longer than the longest allowed test.
const HISTORY_LEN: usize = 3600;

#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub at: Instant,
    pub value: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// No reading yet, still within the start-up grace period.
    Searching,
    Found,
    /// No source produced a reading.
    Missing,
}

#[derive(Debug, Clone)]
pub struct Channel {
    pub samples: VecDeque<Sample>,
    pub source: Option<String>,
    pub probe: Probe,
    searching_since: Instant,
}

impl Channel {
    fn new() -> Self {
        Channel {
            samples: VecDeque::new(),
            source: None,
            probe: Probe::Searching,
            searching_since: Instant::now(),
        }
    }

    pub fn latest(&self) -> Option<Sample> {
        self.samples.back().copied()
    }

    /// A copy holding only the latest sample (cheap enough for every frame).
    pub fn clone_light(&self) -> Channel {
        Channel {
            samples: self.samples.back().copied().into_iter().collect(),
            source: self.source.clone(),
            probe: self.probe,
            searching_since: self.searching_since,
        }
    }

    /// Samples taken at or after `t`.
    pub fn since(&self, t: Instant) -> impl Iterator<Item = &Sample> {
        self.samples.iter().filter(move |s| s.at >= t)
    }

    fn push(&mut self, value: f64, source: Option<String>) {
        if self.samples.len() >= HISTORY_LEN {
            self.samples.pop_front();
        }
        self.samples.push_back(Sample { at: Instant::now(), value });
        if source.is_some() {
            self.source = source;
        }
        self.probe = Probe::Found;
    }

    fn miss(&mut self, grace: Duration) {
        if self.probe == Probe::Searching && self.searching_since.elapsed() >= grace {
            self.probe = Probe::Missing;
        }
    }

    fn reset(&mut self) {
        *self = Channel::new();
    }
}

#[derive(Debug, Clone)]
pub struct Readings {
    pub cpu_temp: Channel,
    pub gpu_temp: Channel,
    pub cpu_usage: Channel,
    pub gpu_usage: Channel,
}

pub struct SensorHub {
    readings: Arc<Mutex<Readings>>,
    gpu: Arc<RwLock<Option<GpuDevice>>>,
    gpu_generation: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    #[cfg(windows)]
    lhm: Option<Arc<crate::lhm::LhmStream>>,
}

/// Shared by the sampling threads.
#[derive(Clone)]
struct Shared {
    readings: Arc<Mutex<Readings>>,
    gpu: Arc<RwLock<Option<GpuDevice>>>,
    gpu_generation: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    #[cfg(windows)]
    lhm: Option<Arc<crate::lhm::LhmStream>>,
    multi_gpu: bool,
    grace: Duration,
    /// Simulated temperatures (hidden --demo flag; never submitted).
    demo: bool,
}

impl Shared {
    fn context(&self) -> Context {
        Context {
            #[cfg(windows)]
            lhm: self.lhm.as_ref().and_then(|s| s.latest()),
        }
    }
}

impl SensorHub {
    /// Start sampling. `lhm_dir` enables the embedded LibreHardwareMonitor
    /// helper (Windows); `gpu_count` > 1 makes GPU sources stricter about
    /// which card a reading belongs to; `demo` simulates temperatures.
    pub fn start(
        #[cfg_attr(not(windows), allow(unused_variables))] lhm_dir: Option<std::path::PathBuf>,
        gpu: Option<GpuDevice>,
        gpu_count: usize,
        demo: bool,
    ) -> Self {
        #[cfg(windows)]
        let lhm = lhm_dir.map(crate::lhm::LhmStream::start);

        // The helper needs a few seconds to load its driver on first run.
        #[cfg(windows)]
        let grace = if lhm.is_some() { Duration::from_secs(15) } else { Duration::from_secs(5) };
        #[cfg(not(windows))]
        let grace = Duration::from_secs(5);

        let hub = SensorHub {
            readings: Arc::new(Mutex::new(Readings {
                cpu_temp: Channel::new(),
                gpu_temp: Channel::new(),
                cpu_usage: Channel::new(),
                gpu_usage: Channel::new(),
            })),
            gpu: Arc::new(RwLock::new(gpu)),
            gpu_generation: Arc::new(AtomicU64::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
            #[cfg(windows)]
            lhm: lhm.clone(),
        };

        let shared = Shared {
            readings: hub.readings.clone(),
            gpu: hub.gpu.clone(),
            gpu_generation: hub.gpu_generation.clone(),
            stop: hub.stop.clone(),
            #[cfg(windows)]
            lhm,
            multi_gpu: gpu_count > 1,
            grace,
            demo,
        };

        spawn("sensor-cpu", shared.clone(), cpu_temp_loop);
        spawn("sensor-gpu", shared.clone(), gpu_temp_loop);
        spawn("sensor-usage", shared, usage_loop);
        hub
    }

    /// Run `f` with the current readings (no copy of the history).
    pub fn with<R>(&self, f: impl FnOnce(&Readings) -> R) -> R {
        let guard = self.readings.lock().unwrap_or_else(|e| e.into_inner());
        f(&guard)
    }

    /// Switch the monitored GPU; its readings start over.
    pub fn select_gpu(&self, gpu: Option<GpuDevice>) {
        if let Ok(mut slot) = self.gpu.write() {
            *slot = gpu;
        }
        self.gpu_generation.fetch_add(1, Ordering::SeqCst);
        let mut r = self.readings.lock().unwrap_or_else(|e| e.into_inner());
        r.gpu_temp.reset();
        r.gpu_usage.reset();
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        #[cfg(windows)]
        if let Some(lhm) = &self.lhm {
            lhm.stop();
        }
    }
}

impl Drop for SensorHub {
    fn drop(&mut self) {
        self.stop();
    }
}

fn spawn(name: &str, shared: Shared, body: fn(Shared)) {
    let _ = std::thread::Builder::new().name(name.into()).spawn(move || {
        crate::platform::raise_thread_priority();
        body(shared);
    });
}

/// Call `tick` every INTERVAL until stopped. Ticks never pile up: if one
/// takes longer than the interval, the next starts right after it.
fn every_interval(stop: &AtomicBool, mut tick: impl FnMut()) {
    let mut next = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        // A misbehaving source must not end the sampling thread.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut tick));
        next += INTERVAL;
        let now = Instant::now();
        if next < now {
            next = now;
        }
        while !stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            if now >= next {
                break;
            }
            std::thread::sleep((next - now).min(Duration::from_millis(50)));
        }
    }
}

fn cpu_temp_loop(shared: Shared) {
    let mut sim = Simulated::new(36.0);
    every_interval(&shared.stop.clone(), || {
        let reading = if shared.demo {
            let usage = shared.readings.lock().ok().and_then(|r| r.cpu_usage.latest()).map_or(0.0, |s| s.value);
            Some(sim.step(36.0 + 0.46 * usage, 6.0))
        } else {
            temps::read_cpu(&shared.context())
        };
        let mut r = shared.readings.lock().unwrap_or_else(|e| e.into_inner());
        match reading {
            Some(t) => r.cpu_temp.push(t.celsius, Some(t.source)),
            None => r.cpu_temp.miss(shared.grace),
        }
    });
}

fn gpu_temp_loop(shared: Shared) {
    let mut sim = Simulated::new(33.0);
    every_interval(&shared.stop.clone(), || {
        let generation = shared.gpu_generation.load(Ordering::SeqCst);
        let gpu = shared.gpu.read().ok().and_then(|g| g.clone());
        let reading = if shared.demo {
            gpu.as_ref().map(|_| {
                let busy = crate::stress::GPU_ACTIVE.load(Ordering::Relaxed);
                sim.step(if busy { 71.0 } else { 33.0 }, 8.0)
            })
        } else {
            gpu.as_ref().and_then(|g| temps::read_gpu(&shared.context(), g, shared.multi_gpu))
        };

        // Drop the reading if the user switched GPUs while it was taken.
        if shared.gpu_generation.load(Ordering::SeqCst) != generation {
            return;
        }
        let mut r = shared.readings.lock().unwrap_or_else(|e| e.into_inner());
        match reading {
            Some(t) => r.gpu_temp.push(t.celsius, Some(t.source)),
            None => r.gpu_temp.miss(shared.grace),
        }
    });
}

fn usage_loop(shared: Shared) {
    let mut sys = sysinfo::System::new();
    sys.refresh_cpu_usage();
    every_interval(&shared.stop.clone(), || {
        sys.refresh_cpu_usage();
        let cpu = sys.global_cpu_usage() as f64;

        let generation = shared.gpu_generation.load(Ordering::SeqCst);
        let gpu = shared.gpu.read().ok().and_then(|g| g.clone());
        let gpu_usage = if shared.demo {
            gpu.as_ref().map(|_| if crate::stress::GPU_ACTIVE.load(Ordering::Relaxed) { 98.0 } else { 2.0 })
        } else {
            gpu.as_ref().and_then(|g| temps::read_gpu_usage(&shared.context(), g, shared.multi_gpu))
        };

        let mut r = shared.readings.lock().unwrap_or_else(|e| e.into_inner());
        r.cpu_usage.push(cpu.clamp(0.0, 100.0), None);
        if shared.gpu_generation.load(Ordering::SeqCst) == generation {
            match gpu_usage {
                Some(u) => r.gpu_usage.push(u.clamp(0.0, 100.0), None),
                None => r.gpu_usage.miss(shared.grace),
            }
        }
    });
}

/// First-order approach to a target temperature, for --demo.
struct Simulated {
    value: f64,
    last: Instant,
    seed: u64,
}

impl Simulated {
    fn new(start: f64) -> Self {
        Simulated { value: start, last: Instant::now(), seed: 0x9E37_79B9_7F4A_7C15 }
    }

    fn step(&mut self, target: f64, tau_secs: f64) -> temps::TempReading {
        let dt = self.last.elapsed().as_secs_f64();
        self.last = Instant::now();
        self.value += (target - self.value) * (1.0 - (-dt / tau_secs).exp());
        // xorshift noise, ±0.3 °C
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        let noise = (self.seed % 600) as f64 / 1000.0 - 0.3;
        temps::TempReading {
            celsius: ((self.value + noise) * 10.0).round() / 10.0,
            source: "Demo (simulated)".into(),
        }
    }
}
