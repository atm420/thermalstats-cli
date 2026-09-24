//! Stress workloads. The CPU and GPU kernels are the same as in earlier CLI
//! versions so temperatures stay comparable; what changed is how they are
//! scheduled, so the rest of the system stays usable:
//!
//! * CPU workers run below normal priority. They still take every idle cycle
//!   (so load and heat are unchanged) but yield to the interface, the sensor
//!   readers and the terminal.
//! * GPU work is submitted in short batches (~20 ms each, two in flight) so
//!   the desktop compositor can still draw between them, instead of one long
//!   batch that stalls the screen.

use crate::gpus::GpuDevice;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum GpuStress {
    Off,
    Starting,
    Running { adapter: String, backend: String },
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct StressStatus {
    pub cpu_threads: usize,
    pub gpu: GpuStress,
}

/// True while the GPU worker is submitting work (used by --demo's simulated sensor).
pub static GPU_ACTIVE: AtomicBool = AtomicBool::new(false);

pub struct StressRun {
    running: Arc<AtomicBool>,
    handles: Vec<JoinHandle<()>>,
    status: Arc<Mutex<StressStatus>>,
}

impl StressRun {
    /// Start the CPU workers (one per logical core) and/or the GPU worker.
    pub fn start(cpu: bool, gpu: Option<GpuDevice>, gpu_requested: bool) -> StressRun {
        let running = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(StressStatus {
            cpu_threads: 0,
            gpu: if gpu_requested { GpuStress::Starting } else { GpuStress::Off },
        }));
        let mut handles = Vec::new();

        if gpu_requested {
            let running = running.clone();
            let status = status.clone();
            if let Ok(handle) = std::thread::Builder::new()
                .name("stress-gpu".into())
                .spawn(move || gpu_worker(&running, gpu.as_ref(), &status))
            {
                handles.push(handle);
            }
        }

        if cpu {
            let threads = num_cpus::get();
            for thread_id in 0..threads {
                let running = running.clone();
                if let Ok(handle) = std::thread::Builder::new()
                    .name(format!("stress-cpu-{}", thread_id))
                    .spawn(move || {
                        crate::platform::lower_thread_priority();
                        cpu_stress_worker(thread_id, &running);
                    })
                {
                    handles.push(handle);
                }
            }
            lock(&status).cpu_threads = threads;
        }

        StressRun { running, handles, status }
    }

    pub fn status(&self) -> StressStatus {
        lock(&self.status).clone()
    }

    /// Signal all workers to stop and wait up to `timeout` for them to exit.
    /// Workers still busy after that are left to finish on their own.
    pub fn stop(mut self, timeout: Duration) -> StressStatus {
        self.running.store(false, Ordering::SeqCst);
        let deadline = Instant::now() + timeout;
        for handle in self.handles.drain(..) {
            while !handle.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
        lock(&self.status).clone()
    }
}

impl Drop for StressRun {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ─── CPU ────────────────────────────────────────────────────────────

/// Each worker runs a tight loop of heavy math + random memory access
/// to maximize CPU utilization and generate heat.
fn cpu_stress_worker(thread_id: usize, running: &AtomicBool) {
    // 8 MB buffer per thread for cache thrashing
    const BUF_SIZE: usize = 1024 * 1024;
    let mut buffer = vec![0.0f64; BUF_SIZE];

    // Initialize with non-trivial data
    for (i, val) in buffer.iter_mut().enumerate() {
        *val = (i as f64) * 1.0001 + (thread_id as f64) * 0.001;
    }

    let mut iteration: u64 = 0;

    while running.load(Ordering::Relaxed) {
        let batch_size = 200_000u64;
        let mut sink = 0.0f64;

        for i in 0..batch_size {
            let idx = iteration.wrapping_add(i);

            // Heavy trig chain
            let mut x = f64::sin(idx as f64 * 0.0001) * f64::cos(idx as f64 * 0.00013);
            x = f64::atan2(x, f64::sqrt(f64::abs(x) + 0.001));
            x += f64::tan(x * 0.1) * 0.01;
            let mut y = f64::exp(f64::sin(x)) * f64::ln(f64::abs(x) + 1.0);
            y = f64::powf(f64::abs(y), 0.7) * y.signum();
            x = f64::hypot(x, y) * f64::sin(y * std::f64::consts::TAU);

            // Integer hash chain
            let mut h = idx.wrapping_mul(2654435761);
            h = ((h >> 16) ^ h).wrapping_mul(0x45d9f3b);
            h = ((h >> 16) ^ h).wrapping_mul(0x45d9f3b);
            h = (h >> 16) ^ h;
            h = h.wrapping_mul(0x5bd1e995);
            h = h ^ (h >> 15);

            // Random-access memory writes (cache thrashing)
            let addr1 = (h as usize) & (BUF_SIZE - 1);
            let addr2 = ((h >> 10) as usize) & (BUF_SIZE - 1);
            let addr3 = ((h >> 20) as usize) & (BUF_SIZE - 1);
            buffer[addr1] = x + buffer[addr2];
            buffer[addr3] = y * buffer[addr1] + buffer[addr3] * 0.999;
            buffer[(addr1.wrapping_add(addr2)) & (BUF_SIZE - 1)] +=
                buffer[(addr2.wrapping_add(addr3)) & (BUF_SIZE - 1)] * 0.5;

            // Small matrix multiply every 64 iterations
            if i & 63 == 0 {
                let base = (h as usize) & (BUF_SIZE - 4096);
                for r in 0..16u64 {
                    let mut sum = 0.0f64;
                    for c in 0..16u64 {
                        let bidx = base + (r * 16 + c) as usize;
                        sum += buffer[bidx & (BUF_SIZE - 1)] * (c as f64 + 0.5);
                    }
                    buffer[(base + r as usize) & (BUF_SIZE - 1)] = sum * 0.0001;
                }
            }

            sink += x + y;
        }

        iteration = iteration.wrapping_add(batch_size);

        // Prevent dead-code elimination
        std::hint::black_box(sink);
    }
}

// ─── GPU ────────────────────────────────────────────────────────────

fn backends() -> wgpu::Backends {
    if cfg!(windows) {
        wgpu::Backends::VULKAN | wgpu::Backends::DX12
    } else if cfg!(target_os = "macos") {
        wgpu::Backends::METAL
    } else {
        wgpu::Backends::VULKAN
    }
}

fn instance() -> wgpu::Instance {
    wgpu::Instance::new(&wgpu::InstanceDescriptor {
        backends: backends(),
        ..Default::default()
    })
}

/// Every hardware graphics adapter the stress test could use.
pub fn list_adapters() -> Vec<wgpu::AdapterInfo> {
    instance()
        .enumerate_adapters(backends())
        .into_iter()
        .map(|a| a.get_info())
        .filter(|info| info.device_type != wgpu::DeviceType::Cpu)
        .collect()
}

/// The adapter for `gpu`: same PCI IDs, else same name. Vulkan is preferred
/// over DX12 because earlier versions stressed through Vulkan.
fn pick_adapter(instance: &wgpu::Instance, gpu: Option<&GpuDevice>) -> Result<wgpu::Adapter, String> {
    let adapters: Vec<wgpu::Adapter> = instance
        .enumerate_adapters(backends())
        .into_iter()
        .filter(|a| a.get_info().device_type != wgpu::DeviceType::Cpu)
        .collect();

    let Some(gpu) = gpu else {
        return pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .map_err(|e| format!("No GPU adapter found: {}", e));
    };

    let score = |info: &wgpu::AdapterInfo| -> i32 {
        let mut s = 0;
        if gpu.vendor_id == Some(info.vendor) && gpu.device_id == Some(info.device) {
            s += 100;
        } else if gpu.matches_name(&info.name) {
            s += 50;
        } else {
            return 0;
        }
        if info.backend == wgpu::Backend::Vulkan {
            s += 5;
        }
        s
    };

    let best = adapters
        .iter()
        .map(|a| (score(&a.get_info()), a))
        .filter(|(s, _)| *s > 0)
        .max_by_key(|(s, _)| *s)
        .map(|(_, a)| a.clone());

    match best {
        Some(adapter) => Ok(adapter),
        // A single GPU whose names differ between APIs: use it.
        None if adapters.len() == 1 => Ok(adapters[0].clone()),
        None => Err(format!("No Vulkan/DirectX driver found for {}", gpu.name)),
    }
}

// WGSL compute shader — heavy parallel workload adapted from the browser stress test.
// Each invocation runs multiple iterations of matrix-style multiply-accumulate
// and hash chain operations, designed to saturate GPU compute units.
const WGSL_STRESS_SHADER: &str = r#"
@group(0) @binding(0) var<storage, read_write> data: array<f32>;
@group(0) @binding(1) var<uniform> params: vec4<f32>; // x=time, y=iteration

fn hash(p: vec2<f32>) -> f32 {
    var p2 = fract(p * vec2<f32>(443.8975, 397.2973));
    p2 = p2 + dot(p2, p2.yx + 19.19);
    return fract(p2.x * p2.y);
}

fn heavy_compute(seed: f32, t: f32) -> f32 {
    var acc: f32 = seed;
    for (var i: u32 = 0u; i < 64u; i = i + 1u) {
        let fi = f32(i);
        let h = hash(vec2<f32>(acc + fi, t + fi * 0.1));
        let a = sin(acc * 1.7 + h * 6.283) * cos(fi * 0.1 + t);
        let b = cos(acc * 2.3 - h * 3.141) * sin(fi * 0.13 - t * 0.7);
        let c = sin(a * b + h) * cos(a - b);
        let d = fma(a, b, c) * fma(c, h, a);
        acc = fract(acc + d * 0.01 + a * b * 0.001);
        acc = fma(acc, 1.0001, sin(acc * 12.9898 + fi) * 0.0001);
        acc = fma(acc, 0.9999, cos(acc * 78.233 + t) * 0.0001);
    }
    return acc;
}

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let idx = gid.x;
    let size = arrayLength(&data);
    if (idx >= size) { return; }

    let t = params.x;
    let iter = params.y;
    let seed = data[idx] + f32(idx) * 0.0001 + iter * 0.001;

    var result = heavy_compute(seed, t);
    result = result + heavy_compute(result + t * 0.3, t * 1.3) * 0.5;
    result = result + heavy_compute(result * 0.7 + iter, t * 0.7) * 0.25;

    data[idx] = fract(result);
}
"#;

struct GpuContext {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    bind_group: wgpu::BindGroup,
    params_buffer: wgpu::Buffer,
    work_groups: u32,
}

fn init_gpu(gpu: Option<&GpuDevice>) -> Result<(GpuContext, wgpu::AdapterInfo), String> {
    use wgpu::util::DeviceExt;
    use wgpu::*;

    let instance = instance();
    let adapter = pick_adapter(&instance, gpu)?;
    let info = adapter.get_info();

    let (device, queue) = pollster::block_on(adapter.request_device(&DeviceDescriptor {
        label: Some("thermalstats-stress"),
        required_features: Features::empty(),
        required_limits: Limits::default(),
        memory_hints: MemoryHints::Performance,
        trace: wgpu::Trace::Off,
    }))
    .map_err(|e| format!("Device request failed: {}", e))?;

    let shader_module = device.create_shader_module(ShaderModuleDescriptor {
        label: Some("stress-shader"),
        source: ShaderSource::Wgsl(WGSL_STRESS_SHADER.into()),
    });

    let pipeline = device.create_compute_pipeline(&ComputePipelineDescriptor {
        label: Some("stress-pipeline"),
        layout: None,
        module: &shader_module,
        entry_point: Some("main"),
        compilation_options: Default::default(),
        cache: None,
    });

    // 4M elements (16 MB) — large enough to saturate GPU
    let work_size: u64 = 4 * 1024 * 1024;
    let init_data: Vec<f32> = (0..work_size).map(|i| (i as f32 * 0.0001).fract()).collect();

    let data_buffer = device.create_buffer_init(&util::BufferInitDescriptor {
        label: Some("data-buffer"),
        contents: bytemuck::cast_slice(&init_data),
        usage: BufferUsages::STORAGE | BufferUsages::COPY_DST,
    });

    let params_buffer = device.create_buffer(&BufferDescriptor {
        label: Some("params-buffer"),
        size: 16, // vec4<f32>
        usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let bind_group_layout = pipeline.get_bind_group_layout(0);
    let bind_group = device.create_bind_group(&BindGroupDescriptor {
        label: Some("stress-bind-group"),
        layout: &bind_group_layout,
        entries: &[
            BindGroupEntry { binding: 0, resource: data_buffer.as_entire_binding() },
            BindGroupEntry { binding: 1, resource: params_buffer.as_entire_binding() },
        ],
    });

    // 256 threads per workgroup
    let work_groups = (work_size as u32 + 255) / 256;

    Ok((GpuContext { device, queue, pipeline, bind_group, params_buffer, work_groups }, info))
}

fn gpu_worker(running: &AtomicBool, gpu: Option<&GpuDevice>, status: &Mutex<StressStatus>) {
    // A driver problem must end GPU stress, never the whole app.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_gpu(running, gpu, status)));
    let failure = match result {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(e),
        Err(_) => Some("The graphics driver reported an error".to_string()),
    };
    if let Some(reason) = failure {
        lock(status).gpu = GpuStress::Failed(reason);
    }
}

fn run_gpu(running: &AtomicBool, gpu: Option<&GpuDevice>, status: &Mutex<StressStatus>) -> Result<(), String> {
    let (ctx, info) = init_gpu(gpu)?;

    // wgpu panics on uncaptured errors by default; record them instead.
    let error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    {
        let on_error = error.clone();
        ctx.device.on_uncaptured_error(Box::new(move |e| {
            *lock(&on_error) = Some(e.to_string());
        }));
        let on_lost = error.clone();
        ctx.device.set_device_lost_callback(move |_, message| {
            *lock(&on_lost) = Some(format!("GPU device lost: {}", message));
        });
    }

    lock(status).gpu = GpuStress::Running {
        adapter: info.name.clone(),
        backend: format!("{:?}", info.backend),
    };
    GPU_ACTIVE.store(true, Ordering::Relaxed);
    let result = gpu_loop(running, &ctx, &error);
    GPU_ACTIVE.store(false, Ordering::Relaxed);
    result
}

fn gpu_loop(running: &AtomicBool, ctx: &GpuContext, error: &Mutex<Option<String>>) -> Result<(), String> {

    const TARGET_BATCH: Duration = Duration::from_millis(20);
    const MAX_DISPATCHES: u32 = 256;

    let start = Instant::now();
    let mut iteration = 0u32;
    // Start small and grow: a slow iGPU must not get a multi-second first batch.
    let mut dispatches: u32 = 1;
    let mut in_flight: VecDeque<(wgpu::SubmissionIndex, u32)> = VecDeque::new();
    let mut last_done = Instant::now();

    while running.load(Ordering::Relaxed) {
        if let Some(e) = lock(error).take() {
            return Err(e);
        }

        let params = [start.elapsed().as_secs_f32(), iteration as f32, 0.0f32, 0.0f32];
        ctx.queue.write_buffer(&ctx.params_buffer, 0, bytemuck::cast_slice(&params));

        let mut encoder = ctx.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("stress-encoder"),
        });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("stress-pass"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&ctx.pipeline);
            pass.set_bind_group(0, &ctx.bind_group, &[]);
            for _ in 0..dispatches {
                pass.dispatch_workgroups(ctx.work_groups, 1, 1);
            }
        }
        let index = ctx.queue.submit(std::iter::once(encoder.finish()));
        in_flight.push_back((index, dispatches));
        iteration = iteration.wrapping_add(1);

        // Keep two batches queued so the GPU never idles between them.
        if in_flight.len() >= 2 {
            let (oldest, count) = in_flight.pop_front().unwrap();
            ctx.device
                .poll(wgpu::PollType::WaitForSubmissionIndex(oldest))
                .map_err(|e| format!("GPU stopped responding: {}", e))?;

            // Batches run back to back, so the gap between completions is
            // roughly that batch's GPU time. Steer toward TARGET_BATCH.
            let now = Instant::now();
            let batch_time = now - last_done;
            last_done = now;
            let per_dispatch = batch_time.as_secs_f64() / count as f64;
            if per_dispatch > 0.0 {
                let ideal = (TARGET_BATCH.as_secs_f64() / per_dispatch).clamp(1.0, MAX_DISPATCHES as f64);
                // Move halfway toward the ideal to smooth out noise.
                dispatches = ((dispatches as f64 + ideal) / 2.0).round().max(1.0) as u32;
            }
        }
    }

    let _ = ctx.device.poll(wgpu::PollType::Wait);
    Ok(())
}
