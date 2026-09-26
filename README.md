# ThermalStats

A cross-platform tool that stress tests your CPU and GPU, reads real hardware temperatures, and submits verified results to [ThermalStats](https://thermalstats.com) for community comparison.

**Open source so you can see exactly what runs on your machine.**

> **ThermalStats 2** replaces the step-by-step prompts with a full-screen interface: a live temperature check before you start, a dashboard with a timer, charts and peak temperatures while the test runs, and a results screen that shows how your hardware compares. Need the previous version? [v1.2.6](https://github.com/atm420/thermalstats-cli/releases/tag/v1.2.6) is still available.

## Download

Pre-built binaries are available on the [Releases page](https://github.com/atm420/thermalstats-cli/releases) or directly from [thermalstats.com/test](https://thermalstats.com/test).

| Platform | Binary |
|----------|--------|
| Windows  | `thermalstats.exe` |
| Linux    | `thermalstats-linux` |
| macOS    | `thermalstats-macos` |

## What it does

1. **Checks your hardware and sensors.** Detects the CPU, every GPU and the OS, then shows live CPU and GPU temperatures (and where each reading comes from) so you can confirm they work *before* testing. If a sensor can't be read, it tells you how to fix it. If the CPU or GPU is already warm (above 56 °C) or hot (above 65 °C), it suggests checking for background apps or letting the PC cool down first, since the test should start from idle.
2. **Lets you choose the test.** CPU, GPU or both; which GPU to test on multi-GPU systems; duration; cooling details. Your answers are remembered for next time. After you've submitted a result, you can also say what you changed since (cleaned out dust, new thermal paste, a new cooler, fans, an undervolt, a laptop stand): the new result is then shown next to the old one, in the app and on the website.
3. **Warns you before loading the system.** Your PC may feel slow, fans get loud and the screen may stutter. That's expected, and you can stop at any time.
4. **Runs the stress test.** A progress bar driven by the clock (it never stalls), the finish time, live and peak temperatures with charts, and warnings if something looks wrong (a sensor that doesn't react to load, a GPU that isn't busy, thermal throttling).
5. **Shows results and submits them.** Idle, peak, rise and maximum load for each part. A completed test of 1 minute or longer is submitted automatically together with its temperature curve (use `--no-submit` to skip this), your results page opens in the browser, and you see where your result ranks among others for the same hardware. A test stopped early is never submitted, and neither is a 30-second quick test.

Press **F** at any time to send feedback (bug, idea, praise) straight from the app, or **S** to support ThermalStats.

## Keeping the system responsive

Stress tests are supposed to max out the machine; the app itself shouldn't feel frozen:

- CPU workers run **below normal priority**. They still take every spare cycle, so the load and heat are unchanged, but the interface, the sensor readers and the terminal always get CPU time first.
- GPU work is submitted in **short batches** (about 20 ms each) so the desktop can keep drawing between them.
- Sensors are read on **background threads**. A slow sensor never blocks the screen; if one lags, the dashboard says so instead of freezing.
- On Windows, the helper that reads CPU temperatures (LibreHardwareMonitor) stays open for the whole session instead of starting a new process for every reading, and console **QuickEdit** mode is turned off so clicking in the window can't pause the app.

## Quick start

```bash
# Run it: the interface guides you through everything
thermalstats

# Options pre-fill the interface
thermalstats --test cpu --duration 180 --cooling-type aio --cooling-model "NZXT Kraken X63"

# Plain text output (used automatically when output isn't a terminal)
thermalstats --plain --test both --duration 120

# Troubleshooting (only if temperatures aren't detected): check every
# temperature source and upload a diagnostic log
thermalstats --test debug

# Just detect your hardware (no stress test)
thermalstats --detect-only
```

## Options

| Flag | Description | Default |
|------|-------------|---------|
| `-t, --test` | Test type: `cpu`, `gpu`, `both`, or `debug` (diagnostics, only needed if temperatures aren't detected) | `both` |
| `-d, --duration` | Stress test duration in seconds (30–3600). Only tests of 60 seconds or longer are submitted; shorter ones are quick tests | `120` |
| `--cooling-type` | `stock`, `air`, `aio`, `custom_loop`, `passive`, `other` | — |
| `--cooling-model` | Cooling model name | — |
| `--ambient-temp` | Room temperature (°C) | — |
| `--lang` | `en`, `fr`, `es`, `de`, `pt`, `tr`, `ru`, `ko`, `ar` | OS language |
| `--no-submit` | Don't submit results | off |
| `--detect-only` | Show detected hardware and exit | off |
| `--plain` | Plain text output instead of the full-screen interface | off |
| `--api-url` | API endpoint (for local development) | `https://thermalstats.com/api/submissions` |

Keys: **Enter** continue · **Esc** back / stop · **↑↓←→ Tab** move and change options · **F** feedback · **S** support · **L** language · **?** help · **Q** quit. Buttons and options can also be clicked with the mouse.

## Platform notes

### Windows
- The binary requests administrator rights (UAC prompt) so it can read the CPU die temperature.
- If **HWiNFO** (with Shared Memory Support on), **MSI Afterburner**, **AIDA64** or **Core Temp** is running, temperatures are read from it and no driver is installed.
- Otherwise it uses the embedded [LibreHardwareMonitor](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor), which needs the free, open-source [PawnIO](https://github.com/namazso/PawnIO) driver. PawnIO is installed on first run and you're asked whether to keep it when you quit.
- NVIDIA GPU temperatures come from the NVIDIA driver (NVML); AMD and Intel GPUs from the monitoring app or LibreHardwareMonitor.
- On systems with several GPUs, pick the one to test with ←/→ on the first screen. It decides which GPU is stressed, whose temperature is read and which model is submitted.

### Linux
- CPU temperatures from `coretemp` / `k10temp` / `zenpower` (hwmon) or the CPU thermal zone. Install `lm-sensors` if nothing is detected.
- GPU temperatures from NVML (NVIDIA) or the `amdgpu` hwmon sensor.

### macOS
- CPU/GPU temperatures come from `powermetrics`, which needs root: `sudo ./thermalstats-macos`.

## Build from source

Requires [Rust](https://rustup.rs/) (stable).

```bash
git clone https://github.com/atm420/thermalstats-cli.git
cd thermalstats-cli
cargo build --release
cargo test
```

The binary is at `target/release/thermalstats` (`thermalstats.exe` on Windows).

`lhm/ThermalReader.cs` is the LibreHardwareMonitor helper. CI compiles it with the C# compiler that ships with Windows and repacks `lhm/lhm-bundle.zip` before building; a local build uses the helper already in the bundle, which also works (it's just polled once per second instead of streaming).

For UI work without real sensors, `--demo` (hidden) simulates temperatures. Demo results are never submitted, except to a local development server (`--api-url http://localhost:3000/api/submissions`).

## Releases

| Workflow | Trigger | Publishes to |
|----------|---------|--------------|
| `build.yml` | push to `main`, `v*` tags | public downloads + GitHub Release |
| `beta.yml` | push to `v2` | an unlisted folder on the site (`/downloads/beta/<BETA_DOWNLOAD_TOKEN>/`) |

Pre-release tags such as `v2.0.0-beta.1` never trigger the public release.

## Third-party software

- **[LibreHardwareMonitor](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor)** (MPL-2.0): hardware monitoring library used for accurate sensor readings on Windows
- **[PawnIO](https://pawnio.eu)**: WHQL-signed kernel driver required by LibreHardwareMonitor for CPU MSR access. The official installer is bundled and redistributed with permission from the developer ([namazso](https://github.com/namazso)). PawnIO is installed on first run and can be uninstalled via "Add and remove programs"

## License

[MIT](LICENSE)
