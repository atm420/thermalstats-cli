//! OS-specific helpers: thread priorities, console setup, opening URLs,
//! clipboard, power status and the app data directory.
//!
//! The priority helpers are what keep the interface responsive during a
//! stress test: the CPU workers run *below* normal priority, so the UI,
//! sensor threads and the terminal itself always get CPU time first. On an
//! otherwise idle machine the workers still take every spare cycle, so the
//! load (and the heat) is the same as running them at normal priority.

use std::path::PathBuf;
#[cfg(windows)]
use std::process::Command;

/// Lower the calling thread's priority. Used by the CPU stress workers.
pub fn lower_thread_priority() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_LOWEST,
        };
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST);
    }

    #[cfg(target_os = "linux")]
    unsafe {
        // Per-thread niceness: on Linux PRIO_PROCESS with a TID targets one thread.
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        libc::setpriority(libc::PRIO_PROCESS as _, tid, 10);
    }

    #[cfg(target_os = "macos")]
    unsafe {
        // USER_INITIATED keeps the workers eligible for performance cores;
        // UTILITY/BACKGROUND would pin them to efficiency cores and weaken the test.
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INITIATED, 0);
    }
}

/// Raise the calling thread's priority. Used by the UI, the test controller
/// and the sensor threads so they are never starved by the stress workers.
pub fn raise_thread_priority() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL,
        };
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL);
    }

    #[cfg(target_os = "macos")]
    unsafe {
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
    }

    // Linux: raising priority needs CAP_SYS_NICE; the workers are niced instead.
}

/// One-time process setup, called before the interface starts.
pub fn prepare_process() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcess, ProcessPowerThrottling, SetProcessInformation,
            PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            PROCESS_POWER_THROTTLING_STATE,
        };
        // Opt out of EcoQoS / power throttling. Without this, Windows 11 may move
        // the stress workers to efficiency cores or lower their clocks when the
        // window is in the background, which would understate temperatures.
        let state = PROCESS_POWER_THROTTLING_STATE {
            Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION,
            ControlMask: PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
            StateMask: 0,
        };
        SetProcessInformation(
            GetCurrentProcess(),
            ProcessPowerThrottling,
            &state as *const _ as *const std::ffi::c_void,
            std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
        );

        enable_ansi_colors();
        disable_quick_edit();
        set_console_title("ThermalStats");
    }
}

/// Undo `prepare_process` changes that would outlive us in a shared console.
pub fn restore_process() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::{GetStdHandle, SetConsoleMode, STD_INPUT_HANDLE};
        let mode = ORIGINAL_INPUT_MODE.load(std::sync::atomic::Ordering::SeqCst);
        if mode != u32::MAX {
            SetConsoleMode(GetStdHandle(STD_INPUT_HANDLE), mode);
        }
    }
}

#[cfg(windows)]
static ORIGINAL_INPUT_MODE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(u32::MAX);

/// An elevated console starts without ANSI escape processing, which would
/// print raw codes like ←[33m in plain mode. (The full-screen interface
/// enables this itself.)
#[cfg(windows)]
unsafe fn enable_ansi_colors() {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
        STD_OUTPUT_HANDLE,
    };
    let handle = GetStdHandle(STD_OUTPUT_HANDLE);
    let mut mode = 0u32;
    if GetConsoleMode(handle, &mut mode) != 0 {
        SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
    }
}

/// QuickEdit mode pauses a console program's output as soon as the user
/// clicks inside the window, which makes the app look frozen. Turn it off
/// (restored on exit by `restore_process`).
#[cfg(windows)]
unsafe fn disable_quick_edit() {
    use windows_sys::Win32::System::Console::{
        GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_EXTENDED_FLAGS,
        ENABLE_QUICK_EDIT_MODE, STD_INPUT_HANDLE,
    };
    let handle = GetStdHandle(STD_INPUT_HANDLE);
    let mut mode = 0u32;
    if GetConsoleMode(handle, &mut mode) != 0 {
        ORIGINAL_INPUT_MODE.store(mode, std::sync::atomic::Ordering::SeqCst);
        SetConsoleMode(handle, (mode & !ENABLE_QUICK_EDIT_MODE) | ENABLE_EXTENDED_FLAGS);
    }
}

#[cfg(windows)]
unsafe fn set_console_title(title: &str) {
    use windows_sys::Win32::System::Console::SetConsoleTitleW;
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    SetConsoleTitleW(wide.as_ptr());
}

/// True when this process has the console window to itself — i.e. it was
/// started by double-clicking, so the window closes as soon as we exit.
pub fn owns_console() -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Console::GetConsoleProcessList;
        let mut pids = [0u32; 4];
        GetConsoleProcessList(pids.as_mut_ptr(), pids.len() as u32) <= 1
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Keep a double-clicked console window open until the user presses Enter.
pub fn pause_if_own_console(message: &str) {
    if owns_console() {
        println!("\n  {}", message);
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    }
}

/// Whether the process is running with administrator rights (Windows only).
#[cfg(windows)]
pub fn is_elevated() -> bool {
    let output = hidden_command("net").args(["session"]).output();
    matches!(output, Ok(o) if o.status.success())
}

/// A `Command` that never flashes a console window on Windows.
#[cfg(windows)]
pub fn hidden_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = Command::new(program);
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

/// Open a URL in the default browser. Returns false if the launcher failed
/// (or THERMALSTATS_NO_BROWSER is set, for headless machines and tests).
pub fn open_url(url: &str) -> bool {
    if std::env::var_os("THERMALSTATS_NO_BROWSER").is_some() {
        return false;
    }

    // cmd treats `&` as a command separator; escape it so query strings survive.
    #[cfg(windows)]
    let result = hidden_command("cmd")
        .args(["/C", "start", "", &url.replace('&', "^&")])
        .spawn();

    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();

    #[cfg(target_os = "linux")]
    let result = std::process::Command::new("xdg-open")
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    result.is_ok()
}

/// Copy text to the system clipboard. Returns false if unavailable.
pub fn copy_to_clipboard(text: &str) -> bool {
    #[cfg(windows)]
    {
        windows_clipboard(text)
    }

    #[cfg(target_os = "macos")]
    {
        pipe_to("pbcopy", &[], text)
    }

    #[cfg(target_os = "linux")]
    {
        pipe_to("wl-copy", &[], text)
            || pipe_to("xclip", &["-selection", "clipboard"], text)
            || pipe_to("xsel", &["--clipboard", "--input"], text)
    }
}

#[cfg(unix)]
fn pipe_to(program: &str, args: &[&str], text: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(text.as_bytes()).is_err() {
            return false;
        }
    }
    matches!(child.wait(), Ok(s) if s.success())
}

#[cfg(windows)]
fn windows_clipboard(text: &str) -> bool {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    const CF_UNICODETEXT: u32 = 13;

    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * std::mem::size_of::<u16>();
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return false;
        }
        let mut ok = false;
        if EmptyClipboard() != 0 {
            let mem = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if !mem.is_null() {
                let dst = GlobalLock(mem) as *mut u16;
                if !dst.is_null() {
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
                    GlobalUnlock(mem);
                    // On success the clipboard owns the memory.
                    ok = !SetClipboardData(CF_UNICODETEXT, mem).is_null();
                }
            }
        }
        CloseClipboard();
        ok
    }
}

/// `Some(true)` when a laptop is running on battery, `Some(false)` on AC,
/// `None` when unknown (e.g. desktops without a battery).
pub fn on_battery() -> Option<bool> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
        let mut status: SYSTEM_POWER_STATUS = std::mem::zeroed();
        if GetSystemPowerStatus(&mut status) == 0 {
            return None;
        }
        // BatteryFlag 128 = no system battery; ACLineStatus 0 = offline, 1 = online.
        if status.BatteryFlag == 128 {
            return None;
        }
        match status.ACLineStatus {
            0 => Some(true),
            1 => Some(false),
            _ => None,
        }
    }

    #[cfg(target_os = "linux")]
    {
        let entries = std::fs::read_dir("/sys/class/power_supply").ok()?;
        let mut has_battery = false;
        let mut mains_online = None;
        for entry in entries.flatten() {
            let path = entry.path();
            let kind = std::fs::read_to_string(path.join("type")).unwrap_or_default();
            match kind.trim() {
                "Battery" => has_battery = true,
                "Mains" => {
                    let online = std::fs::read_to_string(path.join("online")).unwrap_or_default();
                    mains_online = Some(online.trim() == "1");
                }
                _ => {}
            }
        }
        if !has_battery {
            return None;
        }
        mains_online.map(|online| !online)
    }

    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("pmset").args(["-g", "batt"]).output().ok()?;
        let text = String::from_utf8_lossy(&output.stdout);
        if !text.contains("InternalBattery") {
            return None;
        }
        if text.contains("'Battery Power'") {
            Some(true)
        } else if text.contains("'AC Power'") {
            Some(false)
        } else {
            None
        }
    }
}

/// Whether the machine has a battery (used for laptop detection).
pub fn has_battery() -> bool {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
        let mut status: SYSTEM_POWER_STATUS = std::mem::zeroed();
        // BatteryFlag 128 = no system battery, 255 = unknown
        GetSystemPowerStatus(&mut status) != 0
            && status.BatteryFlag != 128
            && status.BatteryFlag != 255
    }

    #[cfg(target_os = "linux")]
    {
        std::path::Path::new("/sys/class/power_supply/BAT0").exists()
            || std::path::Path::new("/sys/class/power_supply/BAT1").exists()
    }

    #[cfg(target_os = "macos")]
    {
        on_battery().is_some()
    }
}

/// Per-user data directory for settings and saved results.
pub fn data_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(|p| PathBuf::from(p).join("ThermalStats"))
    }

    #[cfg(target_os = "macos")]
    {
        std::env::var_os("HOME")
            .map(|p| PathBuf::from(p).join("Library/Application Support/ThermalStats"))
    }

    #[cfg(target_os = "linux")]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|p| p.join("thermalstats"))
    }
}
