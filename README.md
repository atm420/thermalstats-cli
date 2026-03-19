# ThermalStats CLI

A cross-platform command-line tool that stress tests your CPU and GPU, reads real hardware temperatures, and submits verified results to [ThermalStats](https://thermalstats.com) for community comparison.

**Open source so you can see exactly what runs on your machine.**

## Download

Pre-built binaries for every release are available on the [Releases page](https://github.com/atm420/thermalstats-cli/releases) or directly from [thermalstats.com/test](https://thermalstats.com/test).

| Platform | Binary |
|----------|--------|
| Windows  | `thermalstats.exe` |
| Linux    | `thermalstats-linux` |
| macOS    | `thermalstats-macos` |

## Features

- **Hardware Detection** — Auto-detects CPU model, core count, GPU model, VRAM, and OS
- **Real Temperatures** — Reads actual CPU/GPU die temperatures via system APIs
- **Native Stress Tests** — Multi-threaded CPU stress (1 thread/core, heavy math + cache thrashing); WebGPU compute stress for GPU (Vulkan/Metal/DX12)
- **Embedded LibreHardwareMonitor** — On Windows, bundles LHM for accurate CPU die temps (no manual install needed)
- **API Submission** — Automatically submits verified results to ThermalStats for community comparison
- **Interactive Prompts** — Just run it — guided prompts walk you through everything
- **Multi-Language** — Auto-detects OS locale with support for English, French, Spanish, German, and Portuguese
- **Cross-Platform** — Windows, Linux, and macOS

## Quick Start

```bash
# Just run it — interactive prompts guide you:
thermalstats

# Or pass flags directly:
thermalstats --test both --duration 120

# CPU-only test with cooling info:
thermalstats --test cpu --cooling-type aio --cooling-model "NZXT Kraken X63"

# Force a specific language:
thermalstats --lang fr

# Just detect your hardware (no stress test):
thermalstats --detect-only
```

## Options

| Flag | Description | Default |
|------|-------------|---------|
| `-t, --test` | Test type: `cpu`, `gpu`, or `both` | `both` |
| `-d, --duration` | Stress test duration in seconds | `60` |
| `--api-url` | API endpoint URL | `https://thermalstats.com/api/submissions` |
| `--no-submit` | Skip submitting results | `false` |
| `--detect-only` | Show detected hardware and exit | `false` |
| `--cooling-type` | Cooling: `air`, `aio`, `custom_loop`, `stock`, `passive`, `other` | — |
| `--cooling-model` | Cooling model name | — |
| `--ambient-temp` | Ambient room temperature (°C) | — |
| `--lang` | Language: `en`, `fr`, `es`, `de`, `pt` | Auto-detect |

## Platform Notes

### Windows
- **Right-click → Run as administrator** for the most accurate CPU die temperatures
- Embeds [LibreHardwareMonitor](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor) — no manual setup needed
- Falls back to WMI `MSAcpi_ThermalZoneTemperature` if LHM is unavailable
- GPU temps via `nvidia-smi` (NVIDIA) or WMI fallback

### Linux
- Run with `sudo` for sensor access, or `chmod +x thermalstats-linux` first
- CPU temps from `/sys/class/thermal/` and `/sys/class/hwmon/` (coretemp, k10temp)
- GPU temps from `nvidia-smi` (NVIDIA) or `/sys/class/drm/` hwmon (AMD)
- Install `lm-sensors` if temperatures aren't detected

### macOS
- CPU temps via IOKit / `powermetrics`
- GPU temp reading support varies by hardware

## Build from Source

Requires [Rust](https://rustup.rs/) 1.70+.

```bash
git clone https://github.com/atm420/thermalstats-cli.git
cd thermalstats-cli
cargo build --release
```

The binary will be at `target/release/thermalstats` (or `thermalstats.exe` on Windows).

## How It Works

1. **Detect** — Identifies CPU, GPU, OS via system APIs
2. **Idle Temps** — Reads baseline temperatures before stress
3. **Stress Test** — Spawns one thread per CPU core with heavy math + cache thrashing; runs WebGPU compute stress for GPU (Vulkan/Metal/DX12)
4. **Load Temps** — Reads temperatures at peak load
5. **Submit** — POSTs verified results to the ThermalStats API
6. **View** — Opens your results page in the browser

## Tips

- Run for at least **120 seconds** so your cooling system has time to fully engage under sustained load
- Close other heavy applications before testing for the most accurate results
- NVIDIA users: ensure drivers are installed so `nvidia-smi` is available

## License

[MIT](LICENSE)
