//! MSI Afterburner shared memory reader (Windows only).
//!
//! MSI Afterburner exposes its sensor values at `MAHMSharedMemory` when
//! it is running. This is the same data feed that RTSS uses to render
//! the in-game OSD, so users running Afterburner + RTSS already have it.
//!
//! Layout reference: MSI Afterburner Monitoring SDK
//! (`MAHMSharedMemory.h`).
//!
//! Header (32 bytes):
//!   dwSignature       u32    ('MAHM' = 0x4D48414D)
//!   dwVersion         u32    (0x00020000 for v2.0)
//!   dwHeaderSize      u32    (typically 32)
//!   dwNumEntries      u32
//!   dwEntrySize       u32    (typically 1324)
//!   time              i32
//!   dwNumGpuEntries   u32
//!   dwGpuEntrySize    u32
//!
//! GPU entries follow the sensor entries (one per GPU, dwGpuEntrySize each):
//!   szGpuId[260], szFamily[260], szDevice[260], szDriver[260], szBIOS[260],
//!   dwMemAmount — `dwGpu` in a sensor entry indexes this table.
//!
//! Entry (1324 bytes):
//!   szSrcName[260]          (ASCII, e.g. "CPU1 temperature", "GPU1 temperature")
//!   szSrcUnits[260]
//!   szLocalizedSrcName[260]
//!   szLocalizedSrcUnits[260]
//!   szRecommendedFormat[260]
//!   data              f32   (current value; NaN if unavailable)
//!   minLimit          f32
//!   maxLimit          f32
//!   dwFlags           u32
//!   dwGpu             u32
//!   dwSrcId           u32
#![cfg(windows)]

use std::ffi::c_void;

// ─── Windows API FFI ────────────────────────────────────────────────

type HANDLE = *mut c_void;
const FILE_MAP_READ: u32 = 0x0004;

extern "system" {
    fn OpenFileMappingW(dw_desired_access: u32, b_inherit_handle: i32, lp_name: *const u16) -> HANDLE;
    fn MapViewOfFile(
        h_file_mapping_object: HANDLE,
        dw_desired_access: u32,
        dw_file_offset_high: u32,
        dw_file_offset_low: u32,
        dw_number_of_bytes_to_map: usize,
    ) -> *mut c_void;
    fn UnmapViewOfFile(lp_base_address: *const c_void) -> i32;
    fn CloseHandle(h_object: HANDLE) -> i32;
}

// ─── Public API ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AfterburnerReading {
    pub cpu_temp: Option<f64>,
    pub gpu_temp: Option<f64>,
    pub cpu_source: Option<String>,
    pub gpu_source: Option<String>,
    pub gpu_usage: Option<f64>,
}

/// Quick check: is MSI Afterburner's shared memory available?
pub fn is_available() -> bool {
    for name in candidate_names() {
        if open_mapping(name).is_some() {
            return true;
        }
    }
    false
}

/// Read CPU/GPU temperatures from MSI Afterburner shared memory.
pub fn read_temps() -> Option<AfterburnerReading> {
    read_filtered(&|_| true)
}

/// Like `read_temps`, but GPU readings only come from GPUs whose device name
/// passes `gpu_filter` (used to pick one GPU of several).
pub fn read_filtered(gpu_filter: &dyn Fn(&str) -> bool) -> Option<AfterburnerReading> {
    for name in candidate_names() {
        if let Some(reading) = try_read(name, gpu_filter) {
            return Some(reading);
        }
    }
    None
}

fn candidate_names() -> &'static [&'static str] {
    &[
        "Global\\MAHMSharedMemory",
        "MAHMSharedMemory",
        "Local\\MAHMSharedMemory",
    ]
}

// ─── Implementation ─────────────────────────────────────────────────

struct MappedView {
    handle: HANDLE,
    view: *mut c_void,
}

impl Drop for MappedView {
    fn drop(&mut self) {
        unsafe {
            if !self.view.is_null() {
                UnmapViewOfFile(self.view);
            }
            if !self.handle.is_null() {
                CloseHandle(self.handle);
            }
        }
    }
}

fn open_mapping(name: &str) -> Option<MappedView> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let handle = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, wide.as_ptr()) };
    if handle.is_null() {
        return None;
    }
    let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ, 0, 0, 0) };
    if view.is_null() {
        unsafe { CloseHandle(handle) };
        return None;
    }
    Some(MappedView { handle, view })
}

fn try_read(name: &str, gpu_filter: &dyn Fn(&str) -> bool) -> Option<AfterburnerReading> {
    let mapping = open_mapping(name)?;
    // SAFETY: `mapping.view` points to a valid MAHM shared mapping. We
    // validate the signature and bound every entry read against the header
    // size/count values before dereferencing. Region released on drop.
    unsafe { parse(mapping.view as *const u8, gpu_filter) }
}

fn read_u32(bytes: &[u8], pos: usize) -> Option<u32> {
    let slice = bytes.get(pos..pos + 4)?;
    Some(u32::from_le_bytes(slice.try_into().ok()?))
}

fn read_f32(bytes: &[u8], pos: usize) -> Option<f32> {
    let slice = bytes.get(pos..pos + 4)?;
    Some(f32::from_le_bytes(slice.try_into().ok()?))
}

fn read_cstr(bytes: &[u8]) -> String {
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..len]).into_owned()
}

/// SAFETY: caller guarantees `base` points to a valid MAHM shared mapping.
unsafe fn parse(base: *const u8, gpu_filter: &dyn Fn(&str) -> bool) -> Option<AfterburnerReading> {
    // Read the 32-byte header.
    let header = std::slice::from_raw_parts(base, 32);

    // 'MAHM' = 0x4D 0x41 0x48 0x4D (LE dword 0x4D48414D)
    if &header[0..4] != b"MAHM" {
        return None;
    }
    let _version = read_u32(header, 4)?;
    let header_size = read_u32(header, 8)? as usize;
    let num_entries = read_u32(header, 12)? as usize;
    let entry_size = read_u32(header, 16)? as usize;

    // Sanity: header is 32 bytes, entries have five 260-byte strings (1300)
    // plus 24 bytes of numeric fields → 1324. Accept a small range to cover
    // future minor changes.
    if header_size < 24 || header_size > 256 {
        return None;
    }
    if num_entries == 0 || num_entries > 1024 {
        return None;
    }
    if entry_size < 1300 || entry_size > 4096 {
        return None;
    }

    // GPU table: device names indexed by each entry's dwGpu.
    let num_gpus = read_u32(header, 24)? as usize;
    let gpu_entry_size = read_u32(header, 28)? as usize;
    let mut gpu_names: Vec<String> = Vec::new();
    if num_gpus > 0 && num_gpus <= 16 && (1300..=4096).contains(&gpu_entry_size) {
        let table = header_size + num_entries * entry_size;
        for g in 0..num_gpus {
            let elem = std::slice::from_raw_parts(base.add(table + g * gpu_entry_size), gpu_entry_size);
            // szDevice at 520, falling back to szFamily at 260
            let device = read_cstr(&elem[520..780]);
            gpu_names.push(if device.is_empty() { read_cstr(&elem[260..520]) } else { device });
        }
    }

    // Entry field offsets (within one entry):
    const OFF_SRC_NAME: usize = 0;
    const NAME_LEN: usize = 260;
    const OFF_SRC_UNITS: usize = 260;
    const UNITS_LEN: usize = 260;
    const OFF_DATA: usize = 1300; // after five 260-byte strings
    const OFF_FLAGS: usize = 1312; // data + min + max = 12 bytes after
    const OFF_GPU: usize = 1316;
    // MSI Afterburner marks unavailable sensors via dwFlags:
    //   0x00000001 = MONITORING_SOURCE_FLAG_ACTIVE
    // Sensors whose hardware doesn't support them have the flag cleared and
    // usually report NaN. We additionally treat negative or absurd values as
    // unavailable.
    const FLAG_ACTIVE: u32 = 0x00000001;

    let mut cpu_best: Option<(i32, f64, String)> = None;
    let mut gpu_best: Option<(i32, f64, String)> = None;
    let mut usage_best: Option<(i32, f64)> = None;

    for i in 0..num_entries {
        let elem_offset = header_size + i * entry_size;
        let elem = std::slice::from_raw_parts(base.add(elem_offset), entry_size);

        let name = read_cstr(&elem[OFF_SRC_NAME..OFF_SRC_NAME + NAME_LEN]);
        let units = read_cstr(&elem[OFF_SRC_UNITS..OFF_SRC_UNITS + UNITS_LEN]);

        // Only consider °C sensors. The unit string is UTF-8, so compare against
        // the raw bytes for "°C" (0xC2 0xB0 'C') or the ASCII fallback "C".
        let units_bytes = units.as_bytes();
        let is_celsius = units_bytes == b"\xc2\xb0C"
            || units_bytes == b"C"
            || units.contains("°C");
        let is_percent = units.trim() == "%";
        if !is_celsius && !is_percent {
            continue;
        }

        let flags = read_u32(elem, OFF_FLAGS)?;
        if flags & FLAG_ACTIVE == 0 {
            continue;
        }

        let raw = read_f32(elem, OFF_DATA)?;
        if !raw.is_finite() {
            continue;
        }
        let value = raw as f64;

        let gpu_index = read_u32(elem, OFF_GPU).unwrap_or(u32::MAX) as usize;
        let gpu_name = gpu_names.get(gpu_index).map(|s| s.as_str()).unwrap_or("");
        let gpu_ok = gpu_filter(gpu_name);

        if is_percent {
            let n = name.to_ascii_lowercase();
            if gpu_ok && n.starts_with("gpu") && n.contains("usage") && (0.0..=100.0).contains(&value) {
                let score = if n == "gpu usage" || n.starts_with("gpu1 usage") { 100 } else { 50 };
                if usage_best.map_or(true, |(s, _)| score > s) {
                    usage_best = Some((score, value));
                }
            }
            continue;
        }
        if !(value > 0.0 && value < 150.0) {
            continue;
        }

        if let Some(score) = score_cpu(&name) {
            if cpu_best.as_ref().map_or(true, |(s, _, _)| score > *s) {
                cpu_best = Some((score, value, name.clone()));
            }
        }
        if !gpu_ok {
            continue;
        }
        if let Some(score) = score_gpu(&name) {
            if gpu_best.as_ref().map_or(true, |(s, _, _)| score > *s) {
                gpu_best = Some((score, value, name.clone()));
            }
        }
    }

    if cpu_best.is_none() && gpu_best.is_none() {
        return None;
    }
    Some(AfterburnerReading {
        cpu_temp: cpu_best.as_ref().map(|(_, t, _)| *t),
        gpu_temp: gpu_best.as_ref().map(|(_, t, _)| *t),
        cpu_source: cpu_best.map(|(_, _, n)| format!("MSI Afterburner / {}", n)),
        gpu_source: gpu_best.map(|(_, _, n)| format!("MSI Afterburner / {}", n)),
        gpu_usage: usage_best.map(|(_, v)| v),
    })
}

/// Higher score = better CPU temperature candidate. None = not a CPU temp.
fn score_cpu(name: &str) -> Option<i32> {
    let n = name.to_ascii_lowercase();
    // Afterburner uses names like "CPU temperature", "CPU1 temperature",
    // "CPU package", "CCD1 temperature". Exclude usage/clock/power sources.
    let is_cpu = n.starts_with("cpu") || n.starts_with("ccd");
    if !is_cpu {
        return None;
    }
    if !n.contains("temperature") && !n.contains("temp") {
        return None;
    }
    if n.contains("motherboard") || n.contains("vrm") {
        return None;
    }
    if n.contains("package") {
        return Some(100);
    }
    if n.contains("tctl") || n.contains("tdie") {
        return Some(95);
    }
    if n.contains("die") {
        return Some(90);
    }
    if n.starts_with("ccd") {
        return Some(85);
    }
    // "CPU temperature" or "CPU1 temperature" (primary package-ish reading)
    if n == "cpu temperature" || n.starts_with("cpu1 temp") || n.starts_with("cpu temp") {
        return Some(80);
    }
    if n.contains("core") {
        return Some(50);
    }
    None
}

/// Higher score = better GPU temperature candidate. None = not a GPU temp.
fn score_gpu(name: &str) -> Option<i32> {
    let n = name.to_ascii_lowercase();
    if !n.starts_with("gpu") {
        return None;
    }
    if !n.contains("temperature") && !n.contains("temp") && !n.contains("hotspot") && !n.contains("hot spot") {
        return None;
    }
    if n.contains("memory") || n.contains("vram") {
        return Some(20);
    }
    if n.contains("hot spot") || n.contains("hotspot") {
        return Some(100);
    }
    if n.contains("junction") {
        return Some(95);
    }
    if n.contains("edge") {
        return Some(85);
    }
    // "GPU temperature" or "GPU1 temperature"
    if n == "gpu temperature" || n.starts_with("gpu1 temp") || n.starts_with("gpu temp") {
        return Some(80);
    }
    if n.contains("core") {
        return Some(75);
    }
    Some(60)
}
