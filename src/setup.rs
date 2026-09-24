//! Start-up work, run on a worker thread while the interface shows progress:
//! hardware detection, then picking (and if needed installing) the way CPU
//! temperatures will be read.

use crate::hardware::{self, HardwareInfo};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BootStep {
    Hardware,
    SensorTools,
    Driver,
    Done,
}

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(not(windows), allow(dead_code))]
pub enum DriverState {
    /// A monitoring app already provides temperatures (or not Windows).
    NotNeeded,
    AlreadyInstalled,
    /// Installed by this session — the user is asked whether to keep it.
    InstalledNow,
    Failed(String),
    /// Needs administrator rights, which we don't have.
    NoAdmin,
}

#[derive(Debug, Clone)]
pub struct SensorSetup {
    /// Embedded LibreHardwareMonitor helper, when used.
    pub lhm_dir: Option<PathBuf>,
    /// Monitoring app we read from (HWiNFO, MSI Afterburner, AIDA64, Core Temp).
    pub monitoring_app: Option<&'static str>,
    pub driver: DriverState,
    pub elevated: bool,
    /// HWiNFO is running but its "Shared Memory Support" is off.
    pub hwinfo_shared_memory_off: bool,
}

pub struct Boot {
    pub hw: HardwareInfo,
    pub setup: SensorSetup,
}

pub fn run(step: impl Fn(BootStep)) -> Boot {
    step(BootStep::Hardware);
    let hw = hardware::detect_hardware();

    step(BootStep::SensorTools);
    let setup = sensor_setup(&step);

    step(BootStep::Done);
    Boot { hw, setup }
}

#[cfg(windows)]
fn sensor_setup(step: &impl Fn(BootStep)) -> SensorSetup {
    use crate::{afterburner, aida64, coretemp, hwinfo, lhm};

    let elevated = crate::platform::is_elevated();
    let hwinfo_status = hwinfo::check_status();

    // A running monitoring app with a CPU temperature means no driver install.
    let monitoring_app: Option<(&'static str, bool)> = if hwinfo_status == hwinfo::HwinfoStatus::SharedMemoryReadable {
        Some(("HWiNFO", hwinfo::read_temps().is_some_and(|r| r.cpu_temp.is_some())))
    } else if afterburner::is_available() {
        Some(("MSI Afterburner", afterburner::read_temps().is_some_and(|r| r.cpu_temp.is_some())))
    } else if aida64::is_available() {
        Some(("AIDA64", aida64::read_temps().is_some_and(|r| r.cpu_temp.is_some())))
    } else if coretemp::is_available() {
        Some(("Core Temp", coretemp::read_temps().is_some_and(|r| r.cpu_temp.is_some())))
    } else {
        None
    };

    let mut setup = SensorSetup {
        lhm_dir: None,
        monitoring_app: monitoring_app.map(|(name, _)| name),
        driver: DriverState::NotNeeded,
        elevated,
        hwinfo_shared_memory_off: hwinfo_status == hwinfo::HwinfoStatus::ProcessRunningNoSharedMem,
    };

    if monitoring_app.is_some_and(|(_, has_cpu)| has_cpu) {
        return setup;
    }
    if !elevated {
        setup.driver = DriverState::NoAdmin;
        return setup;
    }

    step(BootStep::Driver);
    let (dir, status) = lhm::ensure_extracted();
    setup.lhm_dir = dir;
    setup.driver = match status {
        lhm::PawnIOStatus::AlreadyInstalled => DriverState::AlreadyInstalled,
        lhm::PawnIOStatus::Installed => DriverState::InstalledNow,
        lhm::PawnIOStatus::Failed(e) => DriverState::Failed(e),
        lhm::PawnIOStatus::InstallerMissing => DriverState::Failed("PawnIO installer missing".into()),
    };
    setup
}

#[cfg(not(windows))]
fn sensor_setup(_step: &impl Fn(BootStep)) -> SensorSetup {
    SensorSetup {
        lhm_dir: None,
        monitoring_app: None,
        driver: DriverState::NotNeeded,
        // macOS reads temperatures through powermetrics, which needs root.
        elevated: unsafe { libc::geteuid() } == 0,
        hwinfo_shared_memory_off: false,
    }
}
