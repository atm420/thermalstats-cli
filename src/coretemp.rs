//! Core Temp shared memory reader (Windows only).
//!
//! Core Temp exposes a fixed-layout struct at `CoreTempMappingObjectEx`
//! (Global and session-local). CPU only — no GPU data.
//!
//! Layout reference: Core Temp "Shared Memory Reader" SDK
//! (`CoreTempSharedDataEx.h` / `GetCoreTempInfo.cpp`).
//!
//! Struct `CORE_TEMP_SHARED_DATA_EX`:
//!   uiLoad[256]        u32 ×256   (0)
//!   uiTjMax[128]       u32 ×128   (1024)
//!   uiCoreCnt          u32        (1536)
//!   uiCPUCnt           u32        (1540)
//!   fTemp[256]         f32 ×256   (1544)
//!   fVID               f32        (2568)
//!   fCPUSpeed          f32        (2572)
//!   fFSBSpeed          f32        (2576)
//!   fMultiplier        f32        (2580)
//!   sCPUName[100]      char×100   (2584)
//!   ucFahrenheit       u8         (2684)
//!   ucDeltaToTjMax     u8         (2685)
//!   ucTdpSupported     u8         (2686)
//!   ucPowerSupported   u8         (2687)
//!   uiStructVersion    u32        (2688)
//!   uiTdp[128]         u32 ×128   (2692)
//!   fPower[128]        f32 ×128   (3204)
//!   fMultipliers[256]  f32 ×256   (3716)
//! Total size: 4740 bytes.
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
pub struct CoreTempReading {
    pub cpu_temp: Option<f64>,
    /// Descriptive source: "Core Temp / <CPU name>"
    pub cpu_source: Option<String>,
}

/// Quick check: is the Core Temp shared memory mapping available?
pub fn is_available() -> bool {
    for name in candidate_names() {
        if open_mapping(name).is_some() {
            return true;
        }
    }
    false
}

/// Read current CPU temperature from Core Temp shared memory.
/// Returns None if Core Temp isn't running.
pub fn read_temps() -> Option<CoreTempReading> {
    for name in candidate_names() {
        if let Some(reading) = try_read(name) {
            return Some(reading);
        }
    }
    None
}

fn candidate_names() -> &'static [&'static str] {
    &[
        "Global\\CoreTempMappingObjectEx",
        "CoreTempMappingObjectEx",
        "Local\\CoreTempMappingObjectEx",
        // Older Core Temp versions used a different name (smaller struct).
        "Global\\CoreTempMappingObject",
        "CoreTempMappingObject",
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

fn try_read(name: &str) -> Option<CoreTempReading> {
    let mapping = open_mapping(name)?;
    // SAFETY: `mapping.view` points to a valid shared memory region owned
    // by Core Temp. We bounds-check every field read and validate counts
    // before iterating. Region is released when `mapping` is dropped.
    unsafe { parse(mapping.view as *const u8) }
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

/// SAFETY: caller guarantees `base` points to a valid Core Temp shared mapping.
unsafe fn parse(base: *const u8) -> Option<CoreTempReading> {
    // Read the whole struct (4740 bytes for the Ex layout).
    let buf = std::slice::from_raw_parts(base, 4740);

    let core_cnt = read_u32(buf, 1536)? as usize;
    let cpu_cnt = read_u32(buf, 1540)? as usize;

    // Sanity: Core Temp supports up to 128 physical CPUs × 256 logical slots.
    if core_cnt == 0 || core_cnt > 256 || cpu_cnt == 0 || cpu_cnt > 128 {
        return None;
    }
    let total = core_cnt.checked_mul(cpu_cnt)?;
    if total == 0 || total > 256 {
        return None;
    }

    let ucfahrenheit = *buf.get(2684)?;
    let uc_delta_to_tjmax = *buf.get(2685)?;

    // Read fTemp[] values and convert to Celsius (delta vs TjMax, and F→C).
    let mut max_c: Option<f64> = None;
    for i in 0..total {
        let raw = read_f32(buf, 1544 + i * 4)?;
        if !raw.is_finite() {
            continue;
        }
        // Core Temp stores either actual °C/°F OR distance-to-TjMax.
        let mut celsius = if uc_delta_to_tjmax != 0 {
            // Delta-to-TjMax: subtract from per-package TjMax.
            let cpu_idx = i / core_cnt;
            let tjmax = read_u32(buf, 1024 + cpu_idx * 4)? as f64;
            tjmax - raw as f64
        } else {
            raw as f64
        };
        if ucfahrenheit != 0 {
            celsius = (celsius - 32.0) * 5.0 / 9.0;
        }
        if !(celsius > 0.0 && celsius < 150.0) {
            continue;
        }
        max_c = Some(match max_c {
            Some(m) => m.max(celsius),
            None => celsius,
        });
    }

    let cpu_name = read_cstr(buf.get(2584..2684)?);
    let cpu_name = cpu_name.trim().to_string();

    max_c.map(|t| CoreTempReading {
        cpu_temp: Some(t),
        cpu_source: Some(if cpu_name.is_empty() {
            "Core Temp".to_string()
        } else {
            format!("Core Temp / {}", cpu_name)
        }),
    })
}
