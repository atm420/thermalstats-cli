//! AIDA64 shared memory reader (Windows only).
//!
//! AIDA64 exposes a text-based stream of sensor values at
//! `AIDA64_SensorValues` when "Write to WMI / Shared Memory" is enabled
//! (Preferences → Hardware Monitoring → External Applications).
//!
//! The buffer contains a concatenation of tagged records, e.g.
//!   `<sys><id>SCPUCLK</id><label>CPU Clock</label><value>4500</value></sys>`
//!   `<temp><id>TCPU</id><label>CPU</label><value>45</value></temp>`
//!   `<temp><id>TGPU1</id><label>GPU Diode</label><value>38</value></temp>`
//! terminated by a null byte. Values are integers or decimals (°C).
#![cfg(windows)]

use std::ffi::c_void;

// ─── Windows API FFI ────────────────────────────────────────────────

type HANDLE = *mut c_void;
const FILE_MAP_READ: u32 = 0x0004;
/// Maximum bytes we'll read from the mapping. AIDA64's buffer is 65 KB by
/// default but may be larger on systems with many sensors.
const MAX_READ: usize = 131_072;

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
pub struct Aida64Reading {
    pub cpu_temp: Option<f64>,
    pub gpu_temp: Option<f64>,
    pub cpu_source: Option<String>,
    pub gpu_source: Option<String>,
}

/// Quick check: is the AIDA64 shared memory mapping available?
pub fn is_available() -> bool {
    for name in candidate_names() {
        if open_mapping(name).is_some() {
            return true;
        }
    }
    false
}

/// Read current CPU/GPU temperatures from AIDA64 shared memory.
/// Returns None if AIDA64 isn't running with shared memory enabled.
pub fn read_temps() -> Option<Aida64Reading> {
    for name in candidate_names() {
        if let Some(reading) = try_read(name) {
            return Some(reading);
        }
    }
    None
}

fn candidate_names() -> &'static [&'static str] {
    &[
        "Global\\AIDA64_SensorValues",
        "AIDA64_SensorValues",
        "Local\\AIDA64_SensorValues",
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

fn try_read(name: &str) -> Option<Aida64Reading> {
    let mapping = open_mapping(name)?;
    // SAFETY: we only read bytes up to the first null terminator or MAX_READ,
    // whichever comes first. AIDA64 always null-terminates its buffer.
    let text = unsafe { read_to_string(mapping.view as *const u8) };
    parse(&text)
}

/// SAFETY: caller guarantees `base` points to a valid AIDA64 shared mapping.
unsafe fn read_to_string(base: *const u8) -> String {
    let raw = std::slice::from_raw_parts(base, MAX_READ);
    let len = raw.iter().position(|&b| b == 0).unwrap_or(MAX_READ);
    String::from_utf8_lossy(&raw[..len]).into_owned()
}

fn parse(text: &str) -> Option<Aida64Reading> {
    let mut cpu_best: Option<(i32, f64, String)> = None;
    let mut gpu_best: Option<(i32, f64, String)> = None;

    for (id, label, value) in iter_temp_records(text) {
        if let Some(score) = score_cpu(id, label) {
            if cpu_best.as_ref().map_or(true, |(s, _, _)| score > *s) {
                cpu_best = Some((score, value, format!("{} ({})", label, id)));
            }
        }
        if let Some(score) = score_gpu(id, label) {
            if gpu_best.as_ref().map_or(true, |(s, _, _)| score > *s) {
                gpu_best = Some((score, value, format!("{} ({})", label, id)));
            }
        }
    }

    if cpu_best.is_none() && gpu_best.is_none() {
        return None;
    }
    Some(Aida64Reading {
        cpu_temp: cpu_best.as_ref().map(|(_, t, _)| *t),
        gpu_temp: gpu_best.as_ref().map(|(_, t, _)| *t),
        cpu_source: cpu_best.map(|(_, _, src)| format!("AIDA64 / {}", src)),
        gpu_source: gpu_best.map(|(_, _, src)| format!("AIDA64 / {}", src)),
    })
}

/// Yield `(id, label, value_celsius)` for every `<temp>…</temp>` record
/// whose value parses as a sane temperature.
fn iter_temp_records(text: &str) -> impl Iterator<Item = (&str, &str, f64)> {
    let mut rest = text;
    std::iter::from_fn(move || {
        loop {
            let start = rest.find("<temp>")?;
            rest = &rest[start + "<temp>".len()..];
            let end = rest.find("</temp>")?;
            let record = &rest[..end];
            rest = &rest[end + "</temp>".len()..];

            let id = extract_tag(record, "id").unwrap_or("");
            let label = extract_tag(record, "label").unwrap_or("");
            let value_str = extract_tag(record, "value").unwrap_or("");
            // AIDA64 may localise the decimal separator (e.g. "45,5").
            let normalised = value_str.replace(',', ".");
            if let Ok(v) = normalised.parse::<f64>() {
                if v.is_finite() && v > 0.0 && v < 150.0 {
                    return Some((id, label, v));
                }
            }
        }
    })
}

fn extract_tag<'a>(record: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let start = record.find(&open)? + open.len();
    let end_rel = record[start..].find(&close)?;
    Some(&record[start..start + end_rel])
}

/// Higher score = better CPU temperature candidate. None = not a CPU temp.
fn score_cpu(id: &str, label: &str) -> Option<i32> {
    let id_l = id.to_ascii_lowercase();
    let lbl = label.to_ascii_lowercase();

    // IDs that start with TCPU or TCC (per-core) or TCCD (Ryzen CCD die)
    // are the CPU family in AIDA64. Exclude TCPUV/TCPUT etc. that are fan/voltage.
    let is_cpu = id_l.starts_with("tcpu") || id_l.starts_with("tcc") || id_l.starts_with("tccd");
    if !is_cpu {
        return None;
    }
    // Skip motherboard/socket-adjacent labels
    if lbl.contains("motherboard") || lbl.contains("socket") || lbl.contains("vrm") {
        return None;
    }

    // Best: explicit package/die
    if lbl.contains("cpu package") || id_l == "tcpupck" || id_l == "tcpupkg" {
        return Some(100);
    }
    if lbl.contains("tctl/tdie") || lbl.contains("tctl") || lbl.contains("tdie") {
        return Some(95);
    }
    if lbl.contains("cpu diode") || id_l == "tcpudio" {
        return Some(90);
    }
    if lbl.starts_with("ccd") || id_l.starts_with("tccd") {
        return Some(85);
    }
    if lbl == "cpu" || id_l == "tcpu" {
        return Some(80);
    }
    if lbl.contains("core") && !lbl.contains("distance") {
        return Some(50); // per-core fallback
    }
    None
}

/// Higher score = better GPU temperature candidate. None = not a GPU temp.
fn score_gpu(id: &str, label: &str) -> Option<i32> {
    let id_l = id.to_ascii_lowercase();
    let lbl = label.to_ascii_lowercase();

    // AIDA64 GPU temp IDs always start with TGPU.
    if !id_l.starts_with("tgpu") {
        return None;
    }
    // Memory temp isn't the die temp
    if lbl.contains("memory") || lbl.contains("vram") || id_l.contains("mem") {
        return Some(20);
    }
    if lbl.contains("hot spot") || lbl.contains("hotspot") || id_l.ends_with("hs") {
        return Some(100);
    }
    if lbl.contains("junction") {
        return Some(95);
    }
    if lbl.contains("diode") {
        return Some(90);
    }
    if lbl == "gpu" || lbl.contains("gpu core") || lbl.contains("gpu die") {
        return Some(80);
    }
    if lbl.contains("gpu") {
        return Some(60);
    }
    None
}
