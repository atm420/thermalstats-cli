use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};

#[derive(Debug, Clone)]
pub struct StressResult {
    pub cpu_usage_max: Option<f64>,
    pub gpu_usage_max: Option<f64>,
    pub cpu_temp_peak: Option<f64>,
    pub gpu_temp_peak: Option<f64>,
}

pub async fn run_stress_test(
    test_type: &str,
    duration: Duration,
    lhm_dir: Option<&std::path::PathBuf>,
    msg_spawned_threads: &str,
    msg_starting_gpu: &str,
    msg_webgpu_fallback: &str,
    msg_complete: &str,
) -> StressResult {
    let running = Arc::new(AtomicBool::new(true));
    let mut cpu_usage_max: Option<f64> = None;
    let mut gpu_usage_max: Option<f64> = None;
    let mut cpu_temp_peak: Option<f64> = None;
    let mut gpu_temp_peak: Option<f64> = None;

    // Clone lhm_dir for use in the monitor loop
    let lhm_dir_owned = lhm_dir.cloned();

    // Progress bar
    let pb = ProgressBar::new(duration.as_secs());
    pb.set_style(
        ProgressStyle::with_template(
            "  [{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len}s {msg}"
        )
        .unwrap()
        .progress_chars("██░"),
    );

    let start = Instant::now();

    // Start CPU stress threads
    let cpu_handles = if test_type == "cpu" || test_type == "both" {
        let running_clone = running.clone();
        Some(start_cpu_stress(running_clone, msg_spawned_threads))
    } else {
        None
    };

    // GPU stress: launch nvidia-smi powered CUDA burn via dedicated threads
    let gpu_handles = if test_type == "gpu" || test_type == "both" {
        let running_clone = running.clone();
        Some(start_gpu_stress(running_clone, msg_starting_gpu, msg_webgpu_fallback))
    } else {
        None
    };

    // Monitor loop — update progress bar, track usage AND temperatures
    let mut sys = sysinfo::System::new();
    while start.elapsed() < duration {
        tokio::time::sleep(Duration::from_secs(1)).await;
        pb.set_position(start.elapsed().as_secs().min(duration.as_secs()));

        // Read CPU usage
        sys.refresh_cpu_usage();
        let cpu_total: f64 = sys.cpus().iter().map(|c| c.cpu_usage() as f64).sum::<f64>()
            / sys.cpus().len() as f64;
        cpu_usage_max = Some(cpu_usage_max.unwrap_or(0.0_f64).max(cpu_total));

        // Read GPU usage (try nvidia-smi, then rocm-smi for AMD)
        if let Some(gpu) = read_gpu_usage() {
            gpu_usage_max = Some(gpu_usage_max.unwrap_or(0.0_f64).max(gpu));
        }

        // Read live temperatures
        let live_temps = crate::temps::read_temperatures_with_lhm(lhm_dir_owned.as_ref());
        if let Some(ct) = live_temps.cpu_temp {
            cpu_temp_peak = Some(cpu_temp_peak.unwrap_or(0.0_f64).max(ct));
        }
        if let Some(gt) = live_temps.gpu_temp {
            gpu_temp_peak = Some(gpu_temp_peak.unwrap_or(0.0_f64).max(gt));
        }

        // Build status message with temps
        let mut msg = format!("CPU: {:.0}%", cpu_total);
        if let Some(ct) = live_temps.cpu_temp {
            msg.push_str(&format!(" {:.0}°C", ct));
        }
        if let Some(gu) = gpu_usage_max {
            msg.push_str(&format!(" | GPU: {:.0}%", gu));
        }
        if let Some(gt) = live_temps.gpu_temp {
            msg.push_str(&format!(" {:.0}°C", gt));
        }

        pb.set_message(msg);
    }

    // Signal all threads to stop
    running.store(false, Ordering::SeqCst);
    pb.finish_with_message(msg_complete.green().to_string());

    // Wait for CPU threads to finish
    if let Some(handles) = cpu_handles {
        for h in handles {
            let _ = h.join();
        }
    }

    // Wait for GPU threads
    if let Some(handles) = gpu_handles {
        for h in handles {
            let _ = h.join();
        }
    }

    StressResult {
        cpu_usage_max,
        gpu_usage_max,
        cpu_temp_peak,
        gpu_temp_peak,
    }
}

/// Spawns one native thread per logical CPU core running heavy math + memory stress.
fn start_cpu_stress(running: Arc<AtomicBool>, msg_spawned: &str) -> Vec<std::thread::JoinHandle<()>> {
    let num_threads = num_cpus::get();
    let mut handles = Vec::with_capacity(num_threads);

    for thread_id in 0..num_threads {
        let running = running.clone();
        let handle = std::thread::spawn(move || {
            cpu_stress_worker(thread_id, &running);
        });
        handles.push(handle);
    }

    let msg = msg_spawned.replace("{}", &num_threads.to_string());
    println!(
        "  {} {}",
        "\u{2713}".green(),
        msg
    );

    handles
}

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
        if sink == f64::INFINITY && iteration == 0 {
            println!("{}", sink);
        }
    }
}

/// GPU stress: runs heavy computation on the actual GPU using wgpu (WebGPU).
/// Maps to Vulkan on Windows/Linux, Metal on macOS, DX12 as fallback.
/// Falls back to CPU-based FP stress if no GPU is available.
fn start_gpu_stress(running: Arc<AtomicBool>, msg_starting: &str, msg_fallback: &str) -> Vec<std::thread::JoinHandle<()>> {
    let mut handles = Vec::new();

    match init_wgpu_stress() {
        Ok(gpu_context) => {
            let msg = msg_starting.replace("{}", &gpu_context.device_name);
            println!(
                "  {} {}",
                "\u{2713}".green(),
                msg
            );

            let running_clone = running.clone();
            let handle = std::thread::spawn(move || {
                gpu_wgpu_stress_worker(&running_clone, gpu_context);
            });
            handles.push(handle);
        }
        Err(e) => {
            let msg = msg_fallback.replace("{}", &e.to_string());
            println!(
                "  {} {}",
                "\u{26a0}".yellow(),
                msg
            );

            let running_clone = running.clone();
            let handle = std::thread::spawn(move || {
                gpu_stress_worker_fallback(&running_clone);
            });
            handles.push(handle);
        }
    }

    handles
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

struct WgpuComputeContext {
    device_name: String,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    bind_group: wgpu::BindGroup,
    params_buffer: wgpu::Buffer,
    work_groups: u32,
}

/// Initialize wgpu with a compute pipeline for GPU stress
fn init_wgpu_stress() -> Result<WgpuComputeContext, String> {
    use wgpu::*;
    use wgpu::util::DeviceExt;

    let instance = Instance::new(&InstanceDescriptor {
        backends: Backends::VULKAN | Backends::METAL,
        ..Default::default()
    });

    let adapter = pollster::block_on(instance.request_adapter(&RequestAdapterOptions {
        power_preference: PowerPreference::HighPerformance,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .map_err(|e| format!("No GPU adapter found: {}", e))?;

    let device_name = adapter.get_info().name;

    let (device, queue) = pollster::block_on(adapter.request_device(
        &DeviceDescriptor {
            label: Some("thermalstats-stress"),
            required_features: Features::empty(),
            required_limits: Limits::default(),
            memory_hints: MemoryHints::Performance,
            trace: wgpu::Trace::Off,
        },
    ))
    .map_err(|e| format!("Device request failed: {}", e))?;

    let shader_module: ShaderModule = device.create_shader_module(ShaderModuleDescriptor {
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

    // Initialize data buffer
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
            BindGroupEntry {
                binding: 0,
                resource: data_buffer.as_entire_binding(),
            },
            BindGroupEntry {
                binding: 1,
                resource: params_buffer.as_entire_binding(),
            },
        ],
    });

    // 256 threads per workgroup
    let work_groups = (work_size as u32 + 255) / 256;

    Ok(WgpuComputeContext {
        device_name,
        device,
        queue,
        pipeline,
        bind_group,
        params_buffer,
        work_groups,
    })
}

/// Run the wgpu stress compute shader in a loop until stopped
fn gpu_wgpu_stress_worker(running: &AtomicBool, ctx: WgpuComputeContext) {
    let mut iteration = 0u32;
    let start = std::time::Instant::now();

    while running.load(Ordering::Relaxed) {
        let elapsed = start.elapsed().as_secs_f32();
        let params = [elapsed, iteration as f32, 0.0f32, 0.0f32];
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
            // Dispatch multiple times per submission for sustained load
            for _ in 0..16 {
                pass.dispatch_workgroups(ctx.work_groups, 1, 1);
            }
        }

        ctx.queue.submit(std::iter::once(encoder.finish()));
        ctx.device.poll(wgpu::PollType::Wait).ok();

        iteration = iteration.wrapping_add(1);
    }
}

/// Fallback GPU stress — pure CPU FP stress when no GPU is available
fn gpu_stress_worker_fallback(running: &AtomicBool) {
    const SIZE: usize = 4 * 1024 * 1024;
    let mut buf_a = vec![1.0001f64; SIZE];
    let mut buf_b = vec![0.9999f64; SIZE];

    let mut iteration = 0u64;

    while running.load(Ordering::Relaxed) {
        let alpha = f64::sin(iteration as f64 * 0.0001) * 0.5 + 1.0;
        for i in 0..SIZE {
            buf_a[i] = buf_a[i].mul_add(alpha, buf_b[i]);
            buf_b[i] = f64::sin(buf_a[i] * 0.0001) * f64::cos(buf_b[i] * 0.0001) + 0.5;
            if i & 0xFFFF == 0 {
                buf_a[i] = buf_a[i].fract() + 1.0;
                buf_b[i] = buf_b[i].fract() + 1.0;
            }
        }
        iteration += 1;
    }
}

/// Read GPU usage percentage — tries nvidia-smi first, then rocm-smi for AMD GPUs
fn read_gpu_usage() -> Option<f64> {
    // Try NVIDIA first
    let output = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=utilization.gpu", "--format=csv,noheader,nounits"])
        .output()
        .ok();

    if let Some(output) = output {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(val) = stdout.trim().lines().next().and_then(|l| l.trim().parse::<f64>().ok()) {
                return Some(val);
            }
        }
    }

    // Try AMD rocm-smi
    let output = std::process::Command::new("rocm-smi")
        .args(["--showuse"])
        .output()
        .ok();

    if let Some(output) = output {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            // rocm-smi outputs lines like "GPU[0] : GPU use (%): 98"
            for line in stdout.lines() {
                let lower = line.to_lowercase();
                if lower.contains("gpu use") {
                    if let Some(pct_str) = line.split(':').last() {
                        if let Ok(val) = pct_str.trim().parse::<f64>() {
                            return Some(val);
                        }
                    }
                }
            }
        }
    }

    None
}
