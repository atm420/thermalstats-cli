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

pub async fn run_stress_test(test_type: &str, duration: Duration, lhm_dir: Option<&std::path::PathBuf>) -> StressResult {
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
        Some(start_cpu_stress(running_clone))
    } else {
        None
    };

    // GPU stress: launch nvidia-smi powered CUDA burn via dedicated threads
    let gpu_handles = if test_type == "gpu" || test_type == "both" {
        let running_clone = running.clone();
        Some(start_gpu_stress(running_clone))
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
    pb.finish_with_message("Complete!".green().to_string());

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
fn start_cpu_stress(running: Arc<AtomicBool>) -> Vec<std::thread::JoinHandle<()>> {
    let num_threads = num_cpus::get();
    let mut handles = Vec::with_capacity(num_threads);

    for thread_id in 0..num_threads {
        let running = running.clone();
        let handle = std::thread::spawn(move || {
            cpu_stress_worker(thread_id, &running);
        });
        handles.push(handle);
    }

    println!(
        "  {} Spawned {} CPU stress threads",
        "✓".green(),
        num_threads.to_string().yellow()
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

/// GPU stress: runs heavy computation on the actual GPU using OpenCL.
/// OpenCL.dll ships with NVIDIA, AMD, and Intel GPU drivers — no extra install needed.
/// Falls back to memory-bandwidth CPU stress if no GPU compute is available.
/// On macOS, OpenCL is deprecated — always uses CPU-based fallback.
fn start_gpu_stress(running: Arc<AtomicBool>) -> Vec<std::thread::JoinHandle<()>> {
    let mut handles = Vec::new();

    #[cfg(any(windows, target_os = "linux"))]
    {
        // Try to initialize OpenCL with GPU device
        match init_opencl_stress() {
            Ok(gpu_context) => {
                println!(
                    "  {} Starting GPU stress (OpenCL compute on {})",
                    "✓".green(),
                    gpu_context.device_name.yellow()
                );

                let running_clone = running.clone();
                let handle = std::thread::spawn(move || {
                    gpu_opencl_stress_worker(&running_clone, gpu_context);
                });
                handles.push(handle);
                return handles;
            }
            Err(e) => {
                println!(
                    "  {} OpenCL not available ({}), using CPU-based FP stress",
                    "⚠".yellow(),
                    e
                );
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        println!(
            "  {} GPU compute stress using CPU-based FP (OpenCL deprecated on macOS)",
            "⚠".yellow(),
        );
    }

    // Fallback: CPU-based FP stress
    let running_clone = running.clone();
    let handle = std::thread::spawn(move || {
        gpu_stress_worker_fallback(&running_clone);
    });
    handles.push(handle);

    handles
}

#[cfg(any(windows, target_os = "linux"))]
struct GpuComputeContext {
    device_name: String,
    kernel: opencl3::kernel::Kernel,
    queue: opencl3::command_queue::CommandQueue,
    buffer_a: opencl3::memory::Buffer<opencl3::types::cl_float>,
    buffer_b: opencl3::memory::Buffer<opencl3::types::cl_float>,
    buffer_c: opencl3::memory::Buffer<opencl3::types::cl_float>,
    work_size: usize,
}

/// Initialize OpenCL with a heavy FP32 compute kernel
#[cfg(any(windows, target_os = "linux"))]
fn init_opencl_stress() -> Result<GpuComputeContext, String> {
    use opencl3::platform::get_platforms;
    use opencl3::device::{Device, CL_DEVICE_TYPE_GPU};
    use opencl3::context::Context;
    use opencl3::command_queue::{CommandQueue, CL_QUEUE_PROFILING_ENABLE};
    use opencl3::program::Program;
    use opencl3::kernel::Kernel;
    use opencl3::memory::{Buffer, CL_MEM_READ_WRITE};
    use opencl3::types::CL_NON_BLOCKING;

    // OpenCL kernel: heavy FP math — trig + multiply chains per work item
    let kernel_src = r#"
        __kernel void stress(
            __global float* a,
            __global float* b,
            __global float* c,
            const unsigned int n,
            const float seed
        ) {
            int gid = get_global_id(0);
            if (gid >= n) return;

            float x = a[gid];
            float y = b[gid];
            float z = c[gid];

            // Heavy FP math loop — 256 iterations of trig + multiply chains
            for (int i = 0; i < 256; i++) {
                float fi = (float)i * 0.001f + seed;
                x = mad(sin(x + fi), cos(y + fi), z);
                y = mad(cos(y + fi), sin(z + fi), x);
                z = mad(sin(z + fi), cos(x + fi), y);
                x = mad(x, 0.9999f, y * 0.0001f);
                y = mad(y, 0.9999f, z * 0.0001f);
                z = mad(z, 0.9999f, x * 0.0001f);
                x = mad(tan(x * 0.01f), 0.01f, x);
                y = mad(atan(y), 0.01f, y);
                z = mad(sqrt(fabs(z) + 1.0f), 0.01f, z - 0.01f);
            }

            a[gid] = x;
            b[gid] = y;
            c[gid] = z;
        }
    "#;

    let platforms = get_platforms().map_err(|e| format!("No OpenCL platforms: {:?}", e))?;
    if platforms.is_empty() {
        return Err("No OpenCL platforms found".into());
    }

    // Find a GPU device across all platforms
    let mut gpu_device: Option<Device> = None;
    for platform in &platforms {
        if let Ok(devices) = platform.get_devices(CL_DEVICE_TYPE_GPU) {
            for dev_id in devices {
                let dev = Device::new(dev_id);
                gpu_device = Some(dev);
                break;
            }
            if gpu_device.is_some() { break; }
        }
    }

    let device = gpu_device.ok_or("No GPU OpenCL device found")?;
    let device_name = device.name().map_err(|e| format!("Can't read device name: {:?}", e))?;

    let context = Context::from_device(&device)
        .map_err(|e| format!("Context: {:?}", e))?;

    let queue = CommandQueue::create_default_with_properties(&context, CL_QUEUE_PROFILING_ENABLE, 0)
        .map_err(|e| format!("Queue: {:?}", e))?;

    let program = Program::create_and_build_from_source(&context, kernel_src, "")
        .map_err(|e| format!("Program build: {:?}", e))?;

    let kernel = Kernel::create(&program, "stress")
        .map_err(|e| format!("Kernel: {:?}", e))?;

    // 16M work items
    let work_size: usize = 16 * 1024 * 1024;
    let init_a = vec![1.0f32; work_size];
    let init_b = vec![0.5f32; work_size];
    let init_c = vec![0.25f32; work_size];

    let mut buffer_a = unsafe {
        Buffer::<opencl3::types::cl_float>::create(&context, CL_MEM_READ_WRITE, work_size, std::ptr::null_mut())
            .map_err(|e| format!("Buffer A: {:?}", e))?
    };
    let mut buffer_b = unsafe {
        Buffer::<opencl3::types::cl_float>::create(&context, CL_MEM_READ_WRITE, work_size, std::ptr::null_mut())
            .map_err(|e| format!("Buffer B: {:?}", e))?
    };
    let mut buffer_c = unsafe {
        Buffer::<opencl3::types::cl_float>::create(&context, CL_MEM_READ_WRITE, work_size, std::ptr::null_mut())
            .map_err(|e| format!("Buffer C: {:?}", e))?
    };

    // Upload initial data
    unsafe {
        queue.enqueue_write_buffer(&mut buffer_a, CL_NON_BLOCKING, 0, &init_a, &[])
            .map_err(|e| format!("Write A: {:?}", e))?;
        queue.enqueue_write_buffer(&mut buffer_b, CL_NON_BLOCKING, 0, &init_b, &[])
            .map_err(|e| format!("Write B: {:?}", e))?;
        queue.enqueue_write_buffer(&mut buffer_c, CL_NON_BLOCKING, 0, &init_c, &[])
            .map_err(|e| format!("Write C: {:?}", e))?;
    }
    queue.finish().map_err(|e| format!("Queue finish: {:?}", e))?;

    Ok(GpuComputeContext {
        device_name,
        kernel,
        queue,
        buffer_a,
        buffer_b,
        buffer_c,
        work_size,
    })
}

/// Run the OpenCL stress kernel in a loop until stopped
#[cfg(any(windows, target_os = "linux"))]
fn gpu_opencl_stress_worker(running: &AtomicBool, ctx: GpuComputeContext) {
    use opencl3::kernel::ExecuteKernel;

    let mut iteration = 0u32;
    let n = ctx.work_size as u32;

    while running.load(Ordering::Relaxed) {
        let seed = (iteration as f32) * 0.001;

        let result = unsafe {
            ExecuteKernel::new(&ctx.kernel)
                .set_arg(&ctx.buffer_a)
                .set_arg(&ctx.buffer_b)
                .set_arg(&ctx.buffer_c)
                .set_arg(&n)
                .set_arg(&seed)
                .set_global_work_size(ctx.work_size)
                .enqueue_nd_range(&ctx.queue)
        };

        if result.is_err() {
            break;
        }

        // Wait for GPU to finish this batch
        if ctx.queue.finish().is_err() {
            break;
        }

        iteration = iteration.wrapping_add(1);
    }
}

/// Fallback GPU stress for non-NVIDIA systems — pure CPU FP stress
fn gpu_stress_worker_fallback(running: &AtomicBool) {
    // Large buffer to simulate GPU memory bandwidth stress
    const SIZE: usize = 4 * 1024 * 1024; // 32 MB of f64
    let mut buf_a = vec![1.0001f64; SIZE];
    let mut buf_b = vec![0.9999f64; SIZE];

    let mut iteration = 0u64;

    while running.load(Ordering::Relaxed) {
        // SAXPY-style operation over large arrays (memory bandwidth heavy)
        let alpha = f64::sin(iteration as f64 * 0.0001) * 0.5 + 1.0;
        for i in 0..SIZE {
            buf_a[i] = buf_a[i].mul_add(alpha, buf_b[i]);
            buf_b[i] = f64::sin(buf_a[i] * 0.0001) * f64::cos(buf_b[i] * 0.0001) + 0.5;
            // Prevent values from growing unbounded
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
