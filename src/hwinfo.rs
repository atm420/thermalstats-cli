//! HWiNFO shared memory reader (Windows only).
//!
//! HWiNFO exposes live sensor data via a named shared memory mapping at
//! `Global\HWiNFO_SENS_SM2`. If the user has HWiNFO running with "Shared
//! Memory Support" enabled (default in recent versions), we can read
//! temperatures directly — no driver install, no admin rights required.
//!
//! Layout reference: HWiNFO SDK `HWiNFO_Shared_Memory_Viewer` sample.
//! The struct uses `#pragma pack(1)` in the SDK, so fields are unaligned.
//! We also accept a 4-byte-aligned layout (padding before `poll_time`) as
//! a fallback, validated by structural invariants.
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
pub struct HwinfoReading {
    pub cpu_temp: Option<f64>,
    pub gpu_temp: Option<f64>,
    /// Sensor group + label that was picked for the CPU temp (for diagnostics)
    pub cpu_source: Option<String>,
    /// Sensor group + label that was picked for the GPU temp (for diagnostics)
    pub gpu_source: Option<String>,
}

/// A raw temperature reading from HWiNFO (used for diagnostics/debug dump).
#[derive(Debug, Clone)]
pub struct TempSample {
    pub sensor_name: String,
    pub label: String,
    pub value: f64,
}

/// Return every temperature reading HWiNFO is currently exposing, tagged with
/// its parent sensor group. Used by debug mode to help diagnose wrong-sensor
/// picks. Returns an empty vec if HWiNFO isn't available.
pub fn dump_temps() -> Vec<TempSample> {
    for name in &["Global\\HWiNFO_SENS_SM2", "HWiNFO_SENS_SM2"] {
        if let Some(mapping) = open_mapping(name) {
            // SAFETY: valid mapping, signature/offsets validated inside parse_all
            if let Some(samples) = unsafe { parse_all(mapping.view as *const u8) } {
                return samples;
            }
        }
    }
    Vec::new()
}

/// Quick check: is the HWiNFO shared memory mapping available?
/// Used to decide whether we can skip driver installation.
pub fn is_available() -> bool {
    for name in &["Global\\HWiNFO_SENS_SM2", "HWiNFO_SENS_SM2"] {
        if open_mapping(name).is_some() {
            return true;
        }
    }
    false
}

/// Result of checking for HWiNFO on the system.
#[derive(Debug, PartialEq)]
pub enum HwinfoStatus {
    /// Shared memory is accessible — we can read sensors directly.
    SharedMemoryReadable,
    /// HWiNFO process is running but shared memory is not accessible
    /// (user needs to enable Shared Memory Support in Sensor Settings).
    ProcessRunningNoSharedMem,
    /// HWiNFO is not running.
    NotRunning,
}

/// Detect HWiNFO state: running with SM, running without SM, or not running.
pub fn check_status() -> HwinfoStatus {
    if is_available() {
        return HwinfoStatus::SharedMemoryReadable;
    }
    if is_process_running() {
        return HwinfoStatus::ProcessRunningNoSharedMem;
    }
    HwinfoStatus::NotRunning
}

/// Check if HWiNFO.exe or HWiNFO64.exe is running.
fn is_process_running() -> bool {
    use sysinfo::System;
    let mut sys = System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, false);
    sys.processes().values().any(|p| {
        let name = p.name().to_string_lossy().to_ascii_lowercase();
        name.starts_with("hwinfo") && name.ends_with(".exe")
    })
}

/// Read current CPU/GPU temperatures from HWiNFO shared memory.
/// Returns None if HWiNFO isn't running or shared memory is disabled.
pub fn read_temps() -> Option<HwinfoReading> {
    // Try the Global namespace first (HWiNFO typically runs as admin),
    // then fall back to the session-local name.
    for name in &["Global\\HWiNFO_SENS_SM2", "HWiNFO_SENS_SM2"] {
        if let Some(reading) = try_read(name) {
            return Some(reading);
        }
    }
    None
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

fn try_read(name: &str) -> Option<HwinfoReading> {
    let mapping = open_mapping(name)?;
    // SAFETY: `mapping.view` points to a valid shared memory region owned
    // by HWiNFO. We validate the signature and all offsets/sizes before
    // dereferencing any element. The region is released when `mapping`
    // is dropped.
    unsafe { parse(mapping.view as *const u8) }
}

/// Header layout (two possibilities — packed vs 4-byte aligned).
struct Header {
    offset_sensor: usize,
    size_sensor: usize,
    num_sensor: usize,
    offset_reading: usize,
    size_reading: usize,
    num_reading: usize,
}

/// Validate that the layout starting at `sensor_off_pos` makes sense.
/// `sensor_off_pos` is the byte offset of `dwOffsetOfSensorSection` in the header.
fn try_layout(header: &[u8], sensor_off_pos: usize) -> Option<Header> {
    if header.len() < sensor_off_pos + 24 {
        return None;
    }
    let off_s = read_u32(header, sensor_off_pos)? as usize;
    let sz_s = read_u32(header, sensor_off_pos + 4)? as usize;
    let n_s = read_u32(header, sensor_off_pos + 8)? as usize;
    let off_r = read_u32(header, sensor_off_pos + 12)? as usize;
    let sz_r = read_u32(header, sensor_off_pos + 16)? as usize;
    let n_r = read_u32(header, sensor_off_pos + 20)? as usize;

    // Invariants:
    //   - sensor section starts just past the header (40..64 is reasonable)
    //   - sensor element size >= 264 (sensor_id + inst + 2x 128-byte names)
    //   - reading element size >= 316 (packed; 320 if aligned)
    //   - reading section comes right after the sensor section
    //   - counts are sane
    if off_s < 40 || off_s > 64 {
        return None;
    }
    if n_s == 0 || n_s > 1024 || n_r == 0 || n_r > 65535 {
        return None;
    }
    if sz_s < 264 || sz_s > 2048 || sz_r < 316 || sz_r > 2048 {
        return None;
    }
    if off_r != off_s.checked_add(n_s.checked_mul(sz_s)?)? {
        return None;
    }

    Some(Header {
        offset_sensor: off_s,
        size_sensor: sz_s,
        num_sensor: n_s,
        offset_reading: off_r,
        size_reading: sz_r,
        num_reading: n_r,
    })
}

fn read_u32(bytes: &[u8], pos: usize) -> Option<u32> {
    let slice = bytes.get(pos..pos + 4)?;
    Some(u32::from_le_bytes(slice.try_into().ok()?))
}

fn read_f64(bytes: &[u8], pos: usize) -> Option<f64> {
    let slice = bytes.get(pos..pos + 8)?;
    Some(f64::from_le_bytes(slice.try_into().ok()?))
}

fn read_cstr(bytes: &[u8]) -> String {
    let len = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..len]).into_owned()
}

/// Parse shared memory starting at `base`.
/// SAFETY: caller guarantees `base` points to a valid HWiNFO shared mapping.
unsafe fn parse(base: *const u8) -> Option<HwinfoReading> {
    // Read enough of the header to cover both layout possibilities.
    // Max header size is ~48 bytes; read 64 for safety.
    let header_slice = std::slice::from_raw_parts(base, 64);

    // Validate signature — HWiNFO writes ASCII "HWiS" as the first 4 bytes
    // (dwSignature = 0x5369_5748 little-endian).
    if &header_slice[0..4] != b"HWiS" {
        return None;
    }

    // Try packed layout first (offset 20), fall back to 4-byte-aligned (offset 24).
    let header = try_layout(header_slice, 20).or_else(|| try_layout(header_slice, 24))?;

    // ── Read sensor names (indexed by sensor_index in readings) ──
    let mut sensor_names: Vec<String> = Vec::with_capacity(header.num_sensor);
    for i in 0..header.num_sensor {
        let elem_offset = header.offset_sensor + i * header.size_sensor;
        let elem_ptr = base.add(elem_offset);
        // Sensor element: [id:4][inst:4][name_orig:128][name_user:128] ...
        let elem = std::slice::from_raw_parts(elem_ptr, header.size_sensor);
        if elem.len() < 136 {
            sensor_names.push(String::new());
            continue;
        }
        // Prefer user name, fall back to original
        let user_name = if elem.len() >= 264 {
            read_cstr(&elem[136..264])
        } else {
            String::new()
        };
        let orig_name = read_cstr(&elem[8..136]);
        sensor_names.push(if !user_name.is_empty() { user_name } else { orig_name });
    }

    // ── Scan reading elements for temperature readings ──
    const SENSOR_TYPE_TEMP: u32 = 1;

    let mut cpu_best: Option<(i32, f64, String)> = None;
    let mut gpu_best: Option<(i32, f64, String)> = None;

    for i in 0..header.num_reading {
        let elem_offset = header.offset_reading + i * header.size_reading;
        let elem_ptr = base.add(elem_offset);
        let elem = std::slice::from_raw_parts(elem_ptr, header.size_reading);
        // Reading element layout (stable across HWiNFO versions):
        //   [type:4][sensor_index:4][reading_id:4]
        //   [label_orig:128][label_user:128][unit:16]
        //   [value:8][min:8][max:8][avg:8]
        //   — newer versions append UTF-8 label copies *after* the 316-byte core.
        // Packed: value at 284. Aligned (3 bytes padding before value): 288.
        if elem.len() < 316 {
            continue;
        }

        let reading_type = read_u32(elem, 0)?;
        if reading_type != SENSOR_TYPE_TEMP {
            continue;
        }

        let sensor_idx = read_u32(elem, 4)? as usize;
        let label_user = read_cstr(&elem[140..268]);
        let label_orig = read_cstr(&elem[12..140]);
        let label = if !label_user.is_empty() { label_user } else { label_orig };

        // Try packed offset 284 first; if value is nonsense, try aligned 288.
        let v_packed = read_f64(elem, 284)?;
        let value = if v_packed.is_finite() && v_packed > -100.0 && v_packed < 500.0 {
            v_packed
        } else {
            read_f64(elem, 288)?
        };

        // Sanity-check the temperature
        if !(value > 0.0 && value < 150.0) {
            continue;
        }

        let sensor_name = sensor_names.get(sensor_idx).map(|s| s.as_str()).unwrap_or("");

        // Score as CPU candidate
        if let Some(score) = score_cpu(sensor_name, &label) {
            if cpu_best.as_ref().map_or(true, |(s, _, _)| score > *s) {
                cpu_best = Some((score, value, format!("{} / {}", sensor_name, label)));
            }
        }

        // Score as GPU candidate
        if let Some(score) = score_gpu(sensor_name, &label) {
            if gpu_best.as_ref().map_or(true, |(s, _, _)| score > *s) {
                gpu_best = Some((score, value, format!("{} / {}", sensor_name, label)));
            }
        }
    }

    if cpu_best.is_none() && gpu_best.is_none() {
        return None;
    }

    Some(HwinfoReading {
        cpu_temp: cpu_best.as_ref().map(|(_, t, _)| *t),
        gpu_temp: gpu_best.as_ref().map(|(_, t, _)| *t),
        cpu_source: cpu_best.map(|(_, _, src)| src),
        gpu_source: gpu_best.map(|(_, _, src)| src),
    })
}

/// Diagnostic: dump the first 64 bytes of each candidate HWiNFO mapping
/// as hex + ASCII, plus a parsed interpretation. Used in debug mode when
/// the signature check fails, so we can see what's actually in memory.
pub fn debug_raw_header() -> String {
    let mut out = String::new();
    for name in &["Global\\HWiNFO_SENS_SM2", "HWiNFO_SENS_SM2", "Local\\HWiNFO_SENS_SM2"] {
        out.push_str(&format!("Mapping '{}': ", name));
        let Some(mapping) = open_mapping(name) else {
            out.push_str("not found\n");
            continue;
        };
        // SAFETY: valid mapping, reading first 64 bytes
        let bytes: [u8; 64] = unsafe {
            let ptr = mapping.view as *const u8;
            let slice = std::slice::from_raw_parts(ptr, 64);
            let mut arr = [0u8; 64];
            arr.copy_from_slice(slice);
            arr
        };
        out.push_str("FOUND\n");
        // Hex dump
        out.push_str("  Hex:   ");
        for b in &bytes[..32] {
            out.push_str(&format!("{:02x} ", b));
        }
        out.push('\n');
        // ASCII dump (first 32 bytes)
        out.push_str("  ASCII: ");
        for b in &bytes[..32] {
            let c = if *b >= 0x20 && *b < 0x7f { *b as char } else { '.' };
            out.push_str(&format!(" {} ", c));
        }
        out.push('\n');
        // Parsed interpretation (treat as packed HWiNFO header)
        out.push_str(&format!(
            "  Signature: '{}{}{}{}'\n",
            if bytes[0] >= 0x20 && bytes[0] < 0x7f { bytes[0] as char } else { '.' },
            if bytes[1] >= 0x20 && bytes[1] < 0x7f { bytes[1] as char } else { '.' },
            if bytes[2] >= 0x20 && bytes[2] < 0x7f { bytes[2] as char } else { '.' },
            if bytes[3] >= 0x20 && bytes[3] < 0x7f { bytes[3] as char } else { '.' },
        ));
        out.push_str(&format!(
            "  Version: {}, Revision: {}\n",
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            u32::from_le_bytes(bytes[8..12].try_into().unwrap()),
        ));
        // Try both layouts
        out.push_str(&format!(
            "  [packed]  sensor_off={}, sensor_sz={}, n_sensor={}, reading_off={}, reading_sz={}, n_reading={}\n",
            u32::from_le_bytes(bytes[20..24].try_into().unwrap()),
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            u32::from_le_bytes(bytes[28..32].try_into().unwrap()),
            u32::from_le_bytes(bytes[32..36].try_into().unwrap()),
            u32::from_le_bytes(bytes[36..40].try_into().unwrap()),
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
        ));
        out.push_str(&format!(
            "  [aligned] sensor_off={}, sensor_sz={}, n_sensor={}, reading_off={}, reading_sz={}, n_reading={}\n",
            u32::from_le_bytes(bytes[24..28].try_into().unwrap()),
            u32::from_le_bytes(bytes[28..32].try_into().unwrap()),
            u32::from_le_bytes(bytes[32..36].try_into().unwrap()),
            u32::from_le_bytes(bytes[36..40].try_into().unwrap()),
            u32::from_le_bytes(bytes[40..44].try_into().unwrap()),
            u32::from_le_bytes(bytes[44..48].try_into().unwrap()),
        ));

        // Dump one reading element (scan forward for a temp-type reading where
        // we expect a non-zero value — e.g. GPU temp, which works w/o kernel).
        // Use packed layout values since those validated.
        let reading_off = u32::from_le_bytes(bytes[32..36].try_into().unwrap()) as usize;
        let reading_sz = u32::from_le_bytes(bytes[36..40].try_into().unwrap()) as usize;
        let n_reading = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
        if reading_sz > 0 && reading_sz < 4096 && n_reading > 0 && n_reading < 10000 {
            // SAFETY: mapping validated above; bounds checked via header values
            let ptr = unsafe { (mapping.view as *const u8).add(reading_off) };
            // Find a reading whose label contains "GPU" or "Hot Spot" as a proxy for
            // a sensor that should have a non-zero value in user mode.
            let mut picked: Option<usize> = None;
            for i in 0..n_reading.min(200) {
                let elem = unsafe { std::slice::from_raw_parts(ptr.add(i * reading_sz), reading_sz) };
                if elem.len() < 268 { continue; }
                let label = read_cstr(&elem[12..140]).to_lowercase();
                let reading_type = u32::from_le_bytes(elem[0..4].try_into().unwrap());
                // type 1 = temp
                if reading_type == 1 && (label.contains("gpu") || label.contains("hot spot") || label.contains("drive")) {
                    picked = Some(i);
                    break;
                }
            }
            if let Some(i) = picked {
                let elem = unsafe { std::slice::from_raw_parts(ptr.add(i * reading_sz), reading_sz) };
                let label = read_cstr(&elem[12..140]);
                out.push_str(&format!("  Reading[{}] label='{}' size={}:\n", i, label, reading_sz));
                // Dump bytes 260..reading_sz in 16-byte rows (covers unit+values+tail)
                let start = 260;
                let end = reading_sz.min(elem.len());
                for row_start in (start..end).step_by(16) {
                    let row_end = (row_start + 16).min(end);
                    out.push_str(&format!("    [{:3}] ", row_start));
                    for b in &elem[row_start..row_end] {
                        out.push_str(&format!("{:02x} ", b));
                    }
                    // pad short rows
                    for _ in 0..(16 - (row_end - row_start)) {
                        out.push_str("   ");
                    }
                    out.push_str(" | ");
                    for b in &elem[row_start..row_end] {
                        let c = if *b >= 0x20 && *b < 0x7f { *b as char } else { '.' };
                        out.push(c);
                    }
                    out.push('\n');
                }
                // Try f64 at every 4-byte offset from 268..end-8 and print plausible temps
                out.push_str("    f64 candidates (offset: value):\n");
                let scan_end = reading_sz.saturating_sub(8);
                let mut off = 264;
                while off <= scan_end {
                    let val = f64::from_le_bytes(elem[off..off+8].try_into().unwrap());
                    if val.is_finite() && val >= -50.0 && val <= 200.0 && val != 0.0 {
                        out.push_str(&format!("      [{:3}] {:.2}\n", off, val));
                    }
                    off += 4;
                }
            } else {
                out.push_str("  (no GPU/Drive temp reading found to dump)\n");
            }
        }
    }
    out
}

/// Walk the shared memory and return ALL temperature readings with their
/// parent sensor names. Used for debug dumps. Returns None on invalid data.
unsafe fn parse_all(base: *const u8) -> Option<Vec<TempSample>> {
    let header_slice = std::slice::from_raw_parts(base, 64);
    if &header_slice[0..4] != b"HWiS" {
        return None;
    }
    let header = try_layout(header_slice, 20).or_else(|| try_layout(header_slice, 24))?;

    let mut sensor_names: Vec<String> = Vec::with_capacity(header.num_sensor);
    for i in 0..header.num_sensor {
        let elem_offset = header.offset_sensor + i * header.size_sensor;
        let elem = std::slice::from_raw_parts(base.add(elem_offset), header.size_sensor);
        if elem.len() < 136 {
            sensor_names.push(String::new());
            continue;
        }
        let user_name = if elem.len() >= 264 { read_cstr(&elem[136..264]) } else { String::new() };
        let orig_name = read_cstr(&elem[8..136]);
        sensor_names.push(if !user_name.is_empty() { user_name } else { orig_name });
    }

    const SENSOR_TYPE_TEMP: u32 = 1;
    let mut samples = Vec::new();
    for i in 0..header.num_reading {
        let elem_offset = header.offset_reading + i * header.size_reading;
        let elem = std::slice::from_raw_parts(base.add(elem_offset), header.size_reading);
        if elem.len() < 316 {
            continue;
        }
        let reading_type = read_u32(elem, 0)?;
        if reading_type != SENSOR_TYPE_TEMP {
            continue;
        }
        let sensor_idx = read_u32(elem, 4)? as usize;
        let label_user = read_cstr(&elem[140..268]);
        let label_orig = read_cstr(&elem[12..140]);
        let label = if !label_user.is_empty() { label_user } else { label_orig };
        let v_packed = read_f64(elem, 284)?;
        let value = if v_packed.is_finite() && v_packed > -100.0 && v_packed < 500.0 {
            v_packed
        } else {
            read_f64(elem, 288)?
        };
        let sensor_name = sensor_names.get(sensor_idx).cloned().unwrap_or_default();
        samples.push(TempSample { sensor_name, label, value });
    }
    Some(samples)
}

/// Higher score = better CPU temperature candidate. None = not a CPU temp.
fn score_cpu(sensor: &str, label: &str) -> Option<i32> {
    let s = sensor.to_lowercase();
    let l = label.to_lowercase();

    // Must come from a CPU-related sensor
    let is_cpu = s.contains("cpu")
        || s.contains("intel core")
        || s.contains("ryzen")
        || s.contains("threadripper")
        || s.contains("xeon")
        || s.contains("epyc");
    if !is_cpu {
        return None;
    }

    // Skip motherboard/socket sensors — those are board, not die
    if l.contains("motherboard") || l.contains("socket") || l.contains("vrm") {
        return None;
    }

    // Prefer die/package readings over per-core
    if l.contains("cpu package") || l == "package" {
        return Some(100);
    }
    if l.contains("tctl/tdie") || l.contains("tctl") || l.contains("tdie") {
        return Some(95);
    }
    if l.contains("cpu die") || l.contains("die (average)") {
        return Some(90);
    }
    if l.contains("core max") || l.contains("ccd max") {
        return Some(85);
    }
    if l == "cpu" || l == "cpu temperature" {
        return Some(80);
    }
    if l.starts_with("ccd") {
        return Some(70);
    }
    if l.contains("core") && !l.contains("distance") {
        return Some(50); // per-core fallback
    }
    None
}

/// Higher score = better GPU temperature candidate. None = not a GPU temp.
fn score_gpu(sensor: &str, label: &str) -> Option<i32> {
    let s = sensor.to_lowercase();
    let l = label.to_lowercase();

    // Must come from a GPU-related sensor
    let is_gpu = s.contains("gpu")
        || s.contains("nvidia")
        || s.contains("geforce")
        || s.contains("radeon")
        || s.contains("rtx ")
        || s.contains("gtx ")
        || s.contains("arc ") // Intel Arc
        || (s.contains("amd ") && !s.contains("ryzen") && !s.contains("epyc"));
    if !is_gpu {
        return None;
    }

    // VRAM/memory temp isn't the die temp we want
    if l.contains("memory") || l.contains("vram") {
        return Some(20);
    }

    if l.contains("hot spot") || l.contains("hotspot") {
        return Some(100);
    }
    if l.contains("junction") {
        return Some(95);
    }
    if l == "gpu temperature" || l == "gpu" {
        return Some(90);
    }
    if l.contains("edge") {
        return Some(85);
    }
    if l.contains("gpu core") || l == "core" {
        return Some(80);
    }
    if l.contains("gpu") && l.contains("temperature") {
        return Some(75);
    }
    if l.contains("temperature") {
        return Some(60);
    }
    None
}
