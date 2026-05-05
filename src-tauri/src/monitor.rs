use nvml_wrapper::{enum_wrappers::device::TemperatureSensor, Nvml};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use sysinfo::{Components, CpuRefreshKind, RefreshKind, System};

static PM_LOG_DUMPED:      AtomicBool = AtomicBool::new(false);
static WMI_ENGINE_DUMPED:  AtomicBool = AtomicBool::new(false);

#[cfg(windows)]
use wmi::{COMLibrary, Variant, WMIConnection};

use crate::lhm_bridge::{self, SensorSnapshot};
use crate::settings::{self, AmdFanOverrideSetting, AppSettings, FanOverrideSetting};

// ── NVIDIA: Raw NVML fan control ──────────────────────────────────────

type NvmlSetFanFn   = unsafe extern "C" fn(*mut c_void, u32, u32) -> u32;
type NvmlResetFanFn = unsafe extern "C" fn(*mut c_void, u32) -> u32;

pub struct NvmlFanControl {
    _lib:     libloading::Library,
    set_fn:   NvmlSetFanFn,
    reset_fn: NvmlResetFanFn,
}

unsafe impl Send for NvmlFanControl {}

impl NvmlFanControl {
    fn try_init() -> Option<Self> {
        unsafe {
            let lib = libloading::Library::new("nvml.dll").ok()?;
            let set_fn: NvmlSetFanFn =
                *lib.get::<NvmlSetFanFn>(b"nvmlDeviceSetFanSpeed_v2\0").ok()?;
            let reset_fn: NvmlResetFanFn =
                *lib.get::<NvmlResetFanFn>(b"nvmlDeviceSetDefaultFanSpeed_v2\0").ok()?;
            eprintln!("[HybridGauge] NVML fan control ready");
            Some(NvmlFanControl { _lib: lib, set_fn, reset_fn })
        }
    }
}

// ── AMD: ADL2 fan control via atiadlxx.dll ────────────────────────────

type AdlMallocFn = unsafe extern "C" fn(i32) -> *mut c_void;

type Adl2MainControlCreateFn =
    unsafe extern "C" fn(AdlMallocFn, i32, *mut *mut c_void) -> i32;
type Adl2MainControlDestroyFn =
    unsafe extern "C" fn(*mut c_void) -> i32;
type Adl2AdapterNumberOfAdaptersGetFn =
    unsafe extern "C" fn(*mut c_void, *mut i32) -> i32;
type Adl2AdapterAdapterInfoGetFn =
    unsafe extern "C" fn(*mut c_void, *mut AdlAdapterInfo, i32) -> i32;
type Adl2OdnTemperatureGetFn =
    unsafe extern "C" fn(*mut c_void, i32, i32, *mut i32) -> i32;
type Adl2OdnFanControlGetFn =
    unsafe extern "C" fn(*mut c_void, i32, *mut AdlOdnFanControl) -> i32;
type Adl2OdnFanControlSetFn =
    unsafe extern "C" fn(*mut c_void, i32, *mut AdlOdnFanControl) -> i32;
// RDNA 4+ temperature (millidegrees Celsius)
type Adl2Od10TemperatureGetFn =
    unsafe extern "C" fn(*mut c_void, i32, i32, *mut i32) -> i32;
// Universal PM log data (all modern AMD GPUs incl. RDNA 4)
type Adl2NewQueryPmLogDataGetFn =
    unsafe extern "C" fn(*mut c_void, i32, *mut AdlPmLogDataOutput) -> i32;
// hDevice=0 means "no D3D device; use SMU polling" — safe for monitoring tools
type Adl2NewQueryPmLogDataStartFn =
    unsafe extern "C" fn(*mut c_void, i32, *const AdlPmLogStartInput, *mut AdlPmLogStartOutput, usize) -> i32;
type Adl2NewQueryPmLogDataStopFn =
    unsafe extern "C" fn(*mut c_void, i32) -> i32;

const PMLOG_MAX_SENSORS:      usize = 256;
const PMLOG_TEMPERATURE_EDGE: usize = 7;  // GPU edge/junction temperature
const PMLOG_TEMPERATURE_MEM:  usize = 8;  // GPU memory temperature
const PMLOG_TEMPERATURE_HOT:  usize = 25; // GPU hotspot temperature

#[repr(C)]
#[derive(Clone, Copy)]
struct AdlSingleSensorData {
    ul_active: i32,
    value:     f32,
}

impl Default for AdlSingleSensorData {
    fn default() -> Self { AdlSingleSensorData { ul_active: 0, value: 0.0 } }
}

#[repr(C)]
struct AdlPmLogDataOutput {
    ul_version:           i32,
    ul_num_logged_values: i32,
    a_logged_values:      [AdlSingleSensorData; PMLOG_MAX_SENSORS],
}

impl Default for AdlPmLogDataOutput {
    fn default() -> Self { unsafe { std::mem::zeroed() } }
}

// Sensor IDs to start monitoring (terminated by 0xFFFF)
const PM_LOG_SENSOR_IDS: &[u16] = &[
    7,   // TEMPERATURE_EDGE
    8,   // TEMPERATURE_MEM
    14,  // FAN_RPM
    15,  // FAN_PERCENTAGE
    19,  // INFO_ACTIVITY_GFX
    25,  // TEMPERATURE_HOTSPOT
    0xFFFF,
];

#[repr(C)]
struct AdlPmLogStartInput {
    us_sensors:     [u16; PMLOG_MAX_SENSORS],
    ul_sample_rate: u32, // milliseconds
}

impl Default for AdlPmLogStartInput {
    fn default() -> Self { unsafe { std::mem::zeroed() } }
}

#[repr(C)]
#[derive(Default)]
struct AdlPmLogStartOutput {
    p_logging_address: usize, // void* — mapped buffer, unused by us
}

// ADLAdapterInfo layout (Windows 64-bit, 1572 bytes, Pack=1)
#[repr(C)]
struct AdlAdapterInfo {
    i_size:              i32,
    i_adapter_index:     i32,
    str_udid:            [u8; 256],
    i_bus_number:        i32,
    i_device_number:     i32,
    i_function_number:   i32,
    i_vendor_id:         i32,
    str_adapter_name:    [u8; 256],
    str_display_name:    [u8; 256],
    i_present:           i32,
    i_exist:             i32,
    str_driver_path:     [u8; 256],
    str_driver_path_ext: [u8; 256],
    str_pnp_string:      [u8; 256],
    i_os_display_index:  i32,
}

impl Default for AdlAdapterInfo {
    fn default() -> Self { unsafe { std::mem::zeroed() } }
}

impl AdlAdapterInfo {
    fn adapter_name(&self) -> String {
        let end = self.str_adapter_name.iter().position(|&b| b == 0).unwrap_or(256);
        String::from_utf8_lossy(&self.str_adapter_name[..end]).trim().to_string()
    }
}

// ADLODNFanControl — 8 × i32
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct AdlOdnFanControl {
    i_mode:                   i32, // 0=auto, 1=manual
    i_fan_control_mode:       i32,
    i_current_fan_speed_mode: i32, // 0=rpm, 1=percent
    i_current_fan_speed:      i32,
    i_target_fan_speed:       i32,
    i_target_temperature:     i32,
    i_min_performance_clock:  i32,
    i_min_fan_limit:          i32,
}

// Malloc callback used by ADL — leaks are intentional (init-time only, < 4 KB)
unsafe extern "C" fn adl_malloc_fn(size: i32) -> *mut c_void {
    if size <= 0 { return std::ptr::null_mut(); }
    let v = vec![0u8; size as usize];
    let ptr = v.as_ptr() as *mut c_void;
    std::mem::forget(v);
    ptr
}

pub struct AdlFanControl {
    _lib:            libloading::Library,
    context:         *mut c_void,
    /// (ADL adapter index, display name) — AMD GPUs only, in enumeration order
    amd_adapters:    Vec<(i32, String)>,
    destroy_fn:      Adl2MainControlDestroyFn,
    temp_get_fn:     Adl2OdnTemperatureGetFn,
    od10_temp_fn:    Option<Adl2Od10TemperatureGetFn>,
    pm_log_fn:       Option<Adl2NewQueryPmLogDataGetFn>,
    pm_log_stop_fn:  Option<Adl2NewQueryPmLogDataStopFn>,
    pm_log_started:  Vec<i32>, // adapter indices with active PM log sessions
    fan_get_fn:      Adl2OdnFanControlGetFn,
    fan_set_fn:      Adl2OdnFanControlSetFn,
}

unsafe impl Send for AdlFanControl {}

impl Drop for AdlFanControl {
    fn drop(&mut self) {
        if !self.context.is_null() {
            if let Some(stop_fn) = self.pm_log_stop_fn {
                for &adl_idx in &self.pm_log_started {
                    unsafe { stop_fn(self.context, adl_idx) };
                }
            }
            unsafe { (self.destroy_fn)(self.context) };
        }
    }
}

impl AdlFanControl {
    fn try_init() -> Option<Self> {
        // AssertUnwindSafe: ADL FFI is not unwind-safe by default; we assert it here
        // because we validate all pointers and return values within try_init_inner.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            unsafe { Self::try_init_inner() }
        }))
        .unwrap_or_else(|_| {
            eprintln!("[HybridGauge] ADL init panicked — AMD fan control unavailable");
            None
        })
    }

    unsafe fn try_init_inner() -> Option<Self> {
        let lib = libloading::Library::new("atiadlxx.dll").ok()?;

        let create_fn: Adl2MainControlCreateFn =
            *lib.get(b"ADL2_Main_Control_Create\0").ok()?;
        let destroy_fn: Adl2MainControlDestroyFn =
            *lib.get(b"ADL2_Main_Control_Destroy\0").ok()?;
        let num_adapters_fn: Adl2AdapterNumberOfAdaptersGetFn =
            *lib.get(b"ADL2_Adapter_NumberOfAdapters_Get\0").ok()?;
        let adapter_info_fn: Adl2AdapterAdapterInfoGetFn =
            *lib.get(b"ADL2_Adapter_AdapterInfo_Get\0").ok()?;
        let temp_get_fn: Adl2OdnTemperatureGetFn =
            *lib.get(b"ADL2_OverdriveN_Temperature_Get\0").ok()?;
        // Optional: RDNA 4+ Overdrive10 temperature (returns millidegrees)
        let od10_temp_fn: Option<Adl2Od10TemperatureGetFn> =
            lib.get::<Adl2Od10TemperatureGetFn>(b"ADL2_Overdrive10_Temperature_Get\0")
               .ok().map(|f| *f);
        if od10_temp_fn.is_some() {
            eprintln!("[HybridGauge] ADL: Overdrive10 temperature available");
        }
        // Optional: PM log data (RDNA 2–4, returns values[] indexed by PMLOG_* sensor IDs)
        let pm_log_fn: Option<Adl2NewQueryPmLogDataGetFn> =
            lib.get::<Adl2NewQueryPmLogDataGetFn>(b"ADL2_New_QueryPMLogData_Get\0")
               .ok().map(|f| *f);
        let pm_log_start_fn: Option<Adl2NewQueryPmLogDataStartFn> =
            lib.get::<Adl2NewQueryPmLogDataStartFn>(b"ADL2_New_QueryPMLogData_Start\0")
               .ok().map(|f| *f);
        let pm_log_stop_fn: Option<Adl2NewQueryPmLogDataStopFn> =
            lib.get::<Adl2NewQueryPmLogDataStopFn>(b"ADL2_New_QueryPMLogData_Stop\0")
               .ok().map(|f| *f);
        if pm_log_fn.is_some() {
            eprintln!("[HybridGauge] ADL: PM log available (start={} stop={})",
                pm_log_start_fn.is_some(), pm_log_stop_fn.is_some());
        }
        let fan_get_fn: Adl2OdnFanControlGetFn =
            *lib.get(b"ADL2_OverdriveN_FanControl_Get\0").ok()?;
        let fan_set_fn: Adl2OdnFanControlSetFn =
            *lib.get(b"ADL2_OverdriveN_FanControl_Set\0").ok()?;

        let mut context: *mut c_void = std::ptr::null_mut();
        let rc = create_fn(adl_malloc_fn, 1, &mut context);
        if rc != 0 || context.is_null() {
            eprintln!("[HybridGauge] ADL2_Main_Control_Create failed: {rc} (0x{rc:08x})");
            return None;
        }

        let mut num = 0i32;
        let rc = num_adapters_fn(context, &mut num);
        if rc != 0 || num <= 0 || num > 64 {
            eprintln!("[HybridGauge] ADL2_Adapter_NumberOfAdapters_Get failed: rc={rc} num={num}");
            destroy_fn(context);
            return None;
        }

        let struct_size = std::mem::size_of::<AdlAdapterInfo>() as i32;
        let total_bytes = struct_size * num;
        let mut infos: Vec<AdlAdapterInfo> =
            (0..num).map(|_| AdlAdapterInfo::default()).collect();
        infos[0].i_size = struct_size;
        let ret = adapter_info_fn(context, infos.as_mut_ptr(), total_bytes);
        eprintln!("[HybridGauge] ADL: num_adapters={num} AdapterInfo_ret={ret} struct_size={struct_size}");

        let amd_adapters: Vec<(i32, String)> = if ret == 0 {
            // ADL vendor IDs are decimal: AMD = 1002 (0x03EA), not PCI VID 0x1002
            infos.iter()
                .filter(|i| i.i_vendor_id == 1002 && i.i_present != 0 && i.i_adapter_index >= 0)
                .map(|i| (i.i_adapter_index, i.adapter_name()))
                .collect()
        } else {
            eprintln!("[HybridGauge] ADL2_Adapter_AdapterInfo_Get err {ret} (0x{ret:08x}) — probing by temperature");
            // Fallback: probe each index by temperature
            (0..num).filter_map(|i| {
                let mut t = 0i32;
                let tret = temp_get_fn(context, i, 1, &mut t);
                eprintln!("[HybridGauge]   probe adapter {i}: temp_ret={tret} t={t}");
                if tret == 0 {
                    Some((i, format!("AMD GPU {i}")))
                } else {
                    None
                }
            }).collect()
        };

        if amd_adapters.is_empty() {
            eprintln!("[HybridGauge] ADL: no AMD adapters found");
            destroy_fn(context);
            return None;
        }

        let first_name = amd_adapters.first().map(|(_, n)| n.as_str()).unwrap_or("?");
        eprintln!("[HybridGauge] ADL ready ({} logical adapter(s)) — {first_name}", amd_adapters.len());

        // Start PM log session on the first physical adapter (others are virtual display outputs)
        let mut pm_log_started: Vec<i32> = Vec::new();
        if let (Some(start_fn), Some(&(first_idx, _))) =
            (pm_log_start_fn, amd_adapters.first())
        {
            let mut input = AdlPmLogStartInput { ul_sample_rate: 250, ..Default::default() };
            for (i, &s) in PM_LOG_SENSOR_IDS.iter().enumerate() {
                input.us_sensors[i] = s;
            }
            let mut output = AdlPmLogStartOutput::default();
            // hDevice=0: use SMU polling (no D3D device required)
            let ret = start_fn(context, first_idx, &input, &mut output, 0);
            eprintln!("[HybridGauge] PM log start adapter {first_idx}: ret={ret} (0x{ret:08x})");
            if ret == 0 {
                pm_log_started.push(first_idx);
                eprintln!("[HybridGauge] PM log active — sensor sampling at 250ms");
            }
        }

        Some(AdlFanControl {
            _lib: lib, context, amd_adapters,
            destroy_fn, temp_get_fn, od10_temp_fn,
            pm_log_fn, pm_log_stop_fn, pm_log_started,
            fan_get_fn, fan_set_fn,
        })
    }

    fn temperature(&self, adl_idx: i32) -> Option<f32> {
        if self.context.is_null() || adl_idx < 0 { return None; }

        // OverdriveN: some drivers return millidegrees (RDNA), others return Celsius (Polaris/Vega)
        let mut t = 0i32;
        let ret = unsafe { (self.temp_get_fn)(self.context, adl_idx, 1, &mut t) };
        if ret == 0 && t > 0 {
            let celsius = if t >= 1000 { t as f32 / 1000.0 } else { t as f32 };
            if celsius < 150.0 {
                return Some(celsius);
            }
        }

        // Overdrive10 fallback for RDNA 4+ (always millidegrees)
        if let Some(od10_fn) = self.od10_temp_fn {
            let mut t10 = 0i32;
            let ret10 = unsafe { od10_fn(self.context, adl_idx, 1, &mut t10) };
            if ret10 == 0 && t10 > 0 {
                let celsius = if t10 >= 1000 { t10 as f32 / 1000.0 } else { t10 as f32 };
                if celsius < 150.0 {
                    return Some(celsius);
                }
            }
        }

        // PM Log Data — works on RDNA 2/3/4 when session is started
        if let Some(pm_fn) = self.pm_log_fn {
            let mut data = AdlPmLogDataOutput::default();
            let ret = unsafe { pm_fn(self.context, adl_idx, &mut data) };
            if !PM_LOG_DUMPED.swap(true, Ordering::Relaxed) {
                let active: Vec<_> = data.a_logged_values.iter().enumerate()
                    .filter(|(_, s)| s.ul_active != 0 && s.value != 0.0)
                    .map(|(i, s)| format!("[{i}]={:.1}", s.value))
                    .collect();
                eprintln!("[HybridGauge] PM log adapter {adl_idx} ret={ret}: {}", active.join(" "));
            }
            if ret == 0 {
                for &idx in &[PMLOG_TEMPERATURE_EDGE, PMLOG_TEMPERATURE_HOT, PMLOG_TEMPERATURE_MEM] {
                    let s = &data.a_logged_values[idx];
                    if s.ul_active != 0 && s.value > 0.0 && s.value < 150.0 {
                        return Some(s.value);
                    }
                }
            }
        }

        None
    }

    fn fan_speed_pct(&self, adl_idx: i32) -> Option<u32> {
        if self.context.is_null() || adl_idx < 0 { return None; }
        let mut ctrl = AdlOdnFanControl::default();
        let ret = unsafe { (self.fan_get_fn)(self.context, adl_idx, &mut ctrl) };
        if ret != 0 { return None; }
        // Guard against garbage values from the driver
        if ctrl.i_current_fan_speed_mode == 1
            && (0..=100).contains(&ctrl.i_current_fan_speed)
        {
            Some(ctrl.i_current_fan_speed as u32)
        } else if ctrl.i_fan_control_mode == 1
            && (0..=100).contains(&ctrl.i_target_fan_speed)
        {
            Some(ctrl.i_target_fan_speed as u32)
        } else {
            None
        }
    }

    fn set_fan(&self, adl_idx: i32, pct: u32) -> bool {
        if self.context.is_null() || adl_idx < 0 { return false; }
        let mut ctrl = AdlOdnFanControl {
            i_mode:                   1,
            i_fan_control_mode:       1,
            i_current_fan_speed_mode: 1,
            i_current_fan_speed:      pct as i32,
            i_target_fan_speed:       pct as i32,
            ..Default::default()
        };
        let ret = unsafe { (self.fan_set_fn)(self.context, adl_idx, &mut ctrl) };
        if ret != 0 {
            eprintln!("[HybridGauge] ADL SetFan adapter{adl_idx} {pct}%: err {ret} (0x{ret:08x})");
        }
        ret == 0
    }

    fn reset_fan(&self, adl_idx: i32) -> bool {
        if self.context.is_null() || adl_idx < 0 { return false; }
        let mut ctrl = AdlOdnFanControl {
            i_mode: 0, i_fan_control_mode: 0, ..Default::default()
        };
        let ret = unsafe { (self.fan_set_fn)(self.context, adl_idx, &mut ctrl) };
        ret == 0
    }

    fn adl_index(&self, position: usize) -> Option<i32> {
        self.amd_adapters
            .get(position)
            .map(|(idx, _)| *idx)
            .filter(|&idx| idx >= 0)
    }
}

// ── Fan commands ──────────────────────────────────────────────────────

pub enum FanCommand {
    Set    { index: u32,   speed: Option<u32> }, // NVIDIA NVML device index
    SetAmd { index: usize, speed: Option<u32> }, // AMD position index
}

// ── Metrics structs ───────────────────────────────────────────────────

#[derive(Serialize, Clone, Debug)]
pub struct SystemMetrics {
    pub nvidia_gpus: Vec<NvidiaGpuMetrics>,
    pub amd_gpus:    Vec<AmdGpuMetrics>,
    pub cpu:         CpuMetrics,
}

#[derive(Serialize, Clone, Debug)]
pub struct NvidiaGpuMetrics {
    pub index:                  u32,
    pub name:                   String,
    pub temperature:            Option<u32>,
    pub utilization_gpu:        Option<u32>,
    pub utilization_mem:        Option<u32>,
    pub fan_speed:              Option<u32>,
    pub vram_used_mb:           Option<u64>,
    pub vram_total_mb:          Option<u64>,
    pub fan_control_available:  bool,
    pub safety_override_active: bool,
    pub fan_override:           Option<u32>,
    pub cooldown_secs:          Option<u32>,
}

#[derive(Serialize, Clone, Debug)]
pub struct AmdGpuMetrics {
    pub index:                  usize,
    pub name:                   String,
    pub vram_used_mb:           Option<u64>,
    pub vram_total_mb:          Option<u64>,
    pub utilization_3d:         Option<f64>,
    pub temperature:            Option<f32>,
    pub fan_speed:              Option<u32>, // control % (ADL or LHM /control/)
    pub fan_rpm:                Option<u32>, // actual RPM from LHM /fan/
    pub fan_control_available:  bool,
    pub safety_override_active: bool,
    pub fan_override:           Option<u32>,
    pub cooldown_secs:          Option<u32>,
}

#[derive(Serialize, Clone, Debug)]
pub struct SystemMemoryMetrics {
    pub used_gb:  f32,
    pub total_gb: f32,
}

#[derive(Serialize, Clone, Debug)]
pub struct CpuMetrics {
    pub name:          String,
    pub overall_usage: f32,
    pub core_usages:   Vec<f32>,
    pub package_temp:  Option<f32>,
    pub fan_rpm:       Option<u32>,
    pub ram:           Option<SystemMemoryMetrics>,
    pub npu_usage:     Option<f64>,
}

// ── Monitor ───────────────────────────────────────────────────────────

pub struct Monitor {
    nvml:       Option<Nvml>,
    nvml_fan:   Option<NvmlFanControl>,
    sys:        System,
    components: Components,
    // NVIDIA fan state
    fan_overrides:  HashMap<u32, u32>,
    safety_active:  HashSet<u32>,
    cooldown_ticks: HashMap<u32, u32>,
    // AMD fan state
    #[cfg(windows)]
    adl_fan:            Option<AdlFanControl>,
    amd_fan_overrides:  HashMap<usize, u32>,
    amd_safety_active:  HashSet<usize>,
    amd_cooldown_ticks: HashMap<usize, u32>,
    #[cfg(windows)]
    wmi_con:     Option<WMIConnection>,
    #[cfg(windows)]
    wmi_thermal: Option<WMIConnection>,
}

impl Monitor {
    pub fn new() -> Self {
        let nvml = match Nvml::init() {
            Ok(n)  => { eprintln!("[HybridGauge] NVML initialized"); Some(n) }
            Err(e) => { eprintln!("[HybridGauge] NVML unavailable: {e}"); None }
        };
        let nvml_fan = if nvml.is_some() { NvmlFanControl::try_init() } else { None };

        let mut sys = System::new_with_specifics(
            RefreshKind::new().with_cpu(CpuRefreshKind::everything()),
        );
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        sys.refresh_cpu_all();

        let components = Components::new_with_refreshed_list();

        #[cfg(windows)]
        let (wmi_con, wmi_thermal) = init_wmi();
        #[cfg(windows)]
        let adl_fan = AdlFanControl::try_init();

        // Restore persisted settings
        let saved = settings::load();
        let fan_overrides: HashMap<u32, u32> =
            saved.fan_overrides.iter().map(|s| (s.gpu_index, s.speed)).collect();
        let amd_fan_overrides: HashMap<usize, u32> =
            saved.amd_fan_overrides.iter().map(|s| (s.gpu_position, s.speed)).collect();

        let monitor = Monitor {
            nvml, nvml_fan, sys, components,
            fan_overrides,
            safety_active:  HashSet::new(),
            cooldown_ticks: HashMap::new(),
            #[cfg(windows)]
            adl_fan,
            amd_fan_overrides,
            amd_safety_active:  HashSet::new(),
            amd_cooldown_ticks: HashMap::new(),
            #[cfg(windows)]
            wmi_con,
            #[cfg(windows)]
            wmi_thermal,
        };

        for (&idx, &spd) in &monitor.fan_overrides {
            monitor.apply_fan_raw(idx, spd);
            eprintln!("[HybridGauge] Restored NVIDIA GPU{idx} fan: {spd}%");
        }
        for (&pos, &spd) in &monitor.amd_fan_overrides {
            monitor.apply_amd_fan_raw(pos, spd);
            eprintln!("[HybridGauge] Restored AMD GPU{pos} fan: {spd}%");
        }

        monitor
    }

    pub fn collect(&mut self) -> SystemMetrics {
        self.sys.refresh_cpu_all();
        self.components.refresh();

        // Read LHM sensors once per tick — shared by AMD GPU and CPU paths
        let lhm = lhm_bridge::read_sensors();
        let lhm_ref = lhm.as_deref();

        let nvidia_gpus = self.collect_nvidia();
        let amd_gpus    = self.collect_amd(lhm_ref);

        // ── NVIDIA safety override ──────────────────────────────────
        for gpu in &nvidia_gpus {
            let Some(temp) = gpu.temperature else { continue };
            let idx = gpu.index;
            if temp >= 85 {
                if self.safety_active.insert(idx) {
                    eprintln!("[HybridGauge] Safety ON NVIDIA GPU{idx} {temp}°C → 100%");
                    self.apply_fan_raw(idx, 100);
                }
            } else if temp < 80 && self.safety_active.remove(&idx) {
                eprintln!("[HybridGauge] Safety OFF NVIDIA GPU{idx} {temp}°C");
                match self.fan_overrides.get(&idx).copied() {
                    Some(s) => self.apply_fan_raw(idx, s),
                    None    => self.reset_fan_raw(idx),
                }
            }
        }

        // ── AMD safety override ─────────────────────────────────────
        for gpu in &amd_gpus {
            let Some(temp) = gpu.temperature else { continue };
            let pos = gpu.index;
            let t = temp as u32;
            if t >= 85 {
                if self.amd_safety_active.insert(pos) {
                    eprintln!("[HybridGauge] Safety ON AMD GPU{pos} {t}°C → 100%");
                    self.apply_amd_fan_raw(pos, 100);
                }
            } else if t < 80 && self.amd_safety_active.remove(&pos) {
                eprintln!("[HybridGauge] Safety OFF AMD GPU{pos} {t}°C");
                match self.amd_fan_overrides.get(&pos).copied() {
                    Some(s) => self.apply_amd_fan_raw(pos, s),
                    None    => self.reset_amd_fan_raw(pos),
                }
            }
        }

        // ── NVIDIA cooldown ─────────────────────────────────────────
        let mut nv_resets: Vec<u32> = Vec::new();
        for gpu in &nvidia_gpus {
            let Some(load) = gpu.utilization_gpu else { continue };
            let idx = gpu.index;
            if !self.fan_overrides.contains_key(&idx) {
                self.cooldown_ticks.remove(&idx);
                continue;
            }
            if load <= 20 {
                let t = self.cooldown_ticks.entry(idx).or_insert(0);
                *t += 1;
                if *t >= 30 { nv_resets.push(idx); }
            } else {
                self.cooldown_ticks.remove(&idx);
            }
        }
        for idx in nv_resets {
            eprintln!("[HybridGauge] Cooldown NVIDIA GPU{idx} → auto");
            self.fan_overrides.remove(&idx);
            self.cooldown_ticks.remove(&idx);
            self.reset_fan_raw(idx);
            self.persist_settings();
        }

        // ── AMD cooldown ────────────────────────────────────────────
        let mut amd_resets: Vec<usize> = Vec::new();
        for gpu in &amd_gpus {
            let Some(load) = gpu.utilization_3d else { continue };
            let pos = gpu.index;
            if !self.amd_fan_overrides.contains_key(&pos) {
                self.amd_cooldown_ticks.remove(&pos);
                continue;
            }
            if load <= 20.0 {
                let t = self.amd_cooldown_ticks.entry(pos).or_insert(0);
                *t += 1;
                if *t >= 30 { amd_resets.push(pos); }
            } else {
                self.amd_cooldown_ticks.remove(&pos);
            }
        }
        for pos in amd_resets {
            eprintln!("[HybridGauge] Cooldown AMD GPU{pos} → auto");
            self.amd_fan_overrides.remove(&pos);
            self.amd_cooldown_ticks.remove(&pos);
            self.reset_amd_fan_raw(pos);
            self.persist_settings();
        }

        // Rebuild with updated override/cooldown fields
        let nvidia_gpus = self.collect_nvidia();
        let amd_gpus    = self.collect_amd(lhm_ref);

        SystemMetrics {
            nvidia_gpus,
            amd_gpus,
            cpu: self.collect_cpu(lhm_ref),
        }
    }

    pub fn handle_fan_command(&mut self, cmd: FanCommand) {
        match cmd {
            FanCommand::Set    { index, speed } => { let _ = self.set_fan_override(index, speed); }
            FanCommand::SetAmd { index, speed } => { let _ = self.set_amd_fan_override(index, speed); }
        }
    }

    pub fn set_fan_override(&mut self, index: u32, speed: Option<u32>) -> Result<(), String> {
        if self.nvml_fan.is_none() {
            return Err("NVML fan control unavailable (requires NVIDIA driver ≥ 520)".into());
        }
        match speed {
            Some(s) => {
                let s = s.min(100);
                self.fan_overrides.insert(index, s);
                self.cooldown_ticks.remove(&index);
                if !self.safety_active.contains(&index) {
                    self.apply_fan_raw(index, s);
                }
            }
            None => {
                self.fan_overrides.remove(&index);
                self.safety_active.remove(&index);
                self.cooldown_ticks.remove(&index);
                self.reset_fan_raw(index);
            }
        }
        self.persist_settings();
        Ok(())
    }

    pub fn set_amd_fan_override(&mut self, position: usize, speed: Option<u32>) -> Result<(), String> {
        #[cfg(windows)]
        {
            if self.adl_fan.is_none() {
                return Err(
                    "AMD fan control unavailable (atiadlxx.dll not found or OverdriveN unsupported)".into()
                );
            }
            match speed {
                Some(s) => {
                    let s = s.min(100);
                    self.amd_fan_overrides.insert(position, s);
                    self.amd_cooldown_ticks.remove(&position);
                    if !self.amd_safety_active.contains(&position) {
                        self.apply_amd_fan_raw(position, s);
                    }
                }
                None => {
                    self.amd_fan_overrides.remove(&position);
                    self.amd_safety_active.remove(&position);
                    self.amd_cooldown_ticks.remove(&position);
                    self.reset_amd_fan_raw(position);
                }
            }
            self.persist_settings();
            Ok(())
        }
        #[cfg(not(windows))]
        Err("AMD fan control only available on Windows".into())
    }

    fn persist_settings(&self) {
        let s = AppSettings {
            fan_overrides: self.fan_overrides.iter()
                .map(|(&gpu_index, &speed)| FanOverrideSetting { gpu_index, speed })
                .collect(),
            amd_fan_overrides: self.amd_fan_overrides.iter()
                .map(|(&gpu_position, &speed)| AmdFanOverrideSetting { gpu_position, speed })
                .collect(),
        };
        settings::save(&s);
    }

    // ── NVIDIA collection ────────────────────────────────────────────

    fn collect_nvidia(&self) -> Vec<NvidiaGpuMetrics> {
        let nvml = match &self.nvml { Some(n) => n, None => return vec![] };
        let count = match nvml.device_count() { Ok(c) => c, Err(_) => return vec![] };
        let fan_control_available = self.nvml_fan.is_some();
        (0..count).filter_map(|i| {
            let dev = nvml.device_by_index(i).ok()?;
            Some(NvidiaGpuMetrics {
                index: i,
                name:            dev.name().unwrap_or_else(|_| format!("NVIDIA GPU {i}")),
                temperature:     dev.temperature(TemperatureSensor::Gpu).ok(),
                utilization_gpu: dev.utilization_rates().ok().as_ref().map(|u| u.gpu),
                utilization_mem: dev.utilization_rates().ok().as_ref().map(|u| u.memory),
                fan_speed:       dev.fan_speed(0).ok(),
                vram_used_mb:    dev.memory_info().ok().as_ref().map(|m| m.used >> 20),
                vram_total_mb:   dev.memory_info().ok().as_ref().map(|m| m.total >> 20),
                fan_control_available,
                safety_override_active: self.safety_active.contains(&i),
                fan_override:   self.fan_overrides.get(&i).copied(),
                cooldown_secs:  self.cooldown_ticks.get(&i).copied(),
            })
        }).collect()
    }

    // ── AMD collection ───────────────────────────────────────────────

    #[cfg(windows)]
    fn collect_amd(&self, lhm: Option<&[SensorSnapshot]>) -> Vec<AmdGpuMetrics> {
        let Some(wmi) = &self.wmi_con else { return vec![] };

        let adapters = query_video_controllers(wmi);
        let amd_adapters: Vec<_> = adapters
            .into_iter()
            .filter(|(name, compat, _)| {
                let c = compat.to_lowercase();
                let n = name.to_lowercase();
                if c.contains("intel") || n.contains("intel") { return false; }
                c.contains("amd") || c.contains("ati")
                    || n.contains("radeon") || n.contains("amd")
            })
            .collect();

        if amd_adapters.is_empty() { return vec![]; }

        let util_3d = query_gpu_3d_utilization(wmi);
        let fan_control_available = self.adl_fan.is_some();

        amd_adapters
            .into_iter()
            .enumerate()
            .map(|(pos, (name, _compat, vram_bytes))| {
                // Identifiers confirmed from LHM report (RX 9070 XT):
                //   temperature : /gpu-amd/0/temperature/0
                //   fan RPM     : /gpu-amd/0/fan/0
                //   fan control : /gpu-amd/0/control/0
                // Try exact match first; fall back to filter_by_id if not found.

                // ── Temperature: LHM → ADL → sysinfo/WMI ──────────────
                let lhm_temp: Option<f32> = lhm.and_then(|sensors| {
                    let exact_id = format!("/gpu-amd/{pos}/temperature/0");
                    lhm_bridge::find_by_id(sensors, &exact_id)
                        .map(|s| s.value)
                        .filter(|&v| v > 0.0 && v < 150.0)
                        .or_else(|| {
                            lhm_bridge::filter_by_id(
                                sensors, &["/gpu-amd/", "/temperature"], &["intel"],
                            )
                            .into_iter()
                            .nth(pos)
                            .map(|s| s.value)
                            .filter(|&v| v > 0.0 && v < 150.0)
                        })
                });

                // ── Fan RPM from LHM (/gpu-amd/N/fan/0) ───────────────
                let lhm_fan_rpm: Option<u32> = lhm.and_then(|sensors| {
                    let exact_id = format!("/gpu-amd/{pos}/fan/0");
                    lhm_bridge::find_by_id(sensors, &exact_id)
                        .map(|s| s.value as u32)
                        .filter(|&v| v > 0)
                        .or_else(|| {
                            lhm_bridge::filter_by_id(
                                sensors, &["/gpu-amd/", "/fan"], &["intel"],
                            )
                            .into_iter()
                            .nth(pos)
                            .map(|s| s.value as u32)
                            .filter(|&v| v > 0)
                        })
                });

                // ── Fan control % from LHM (/gpu-amd/N/control/0) ─────
                let lhm_fan_pct: Option<u32> = lhm.and_then(|sensors| {
                    let exact_id = format!("/gpu-amd/{pos}/control/0");
                    lhm_bridge::find_by_id(sensors, &exact_id)
                        .and_then(|s| {
                            let v = s.value;
                            if v >= 0.0 && v <= 100.0 { Some(v as u32) } else { None }
                        })
                        .or_else(|| {
                            lhm_bridge::filter_by_id(
                                sensors, &["/gpu-amd/", "/control"], &["intel"],
                            )
                            .into_iter()
                            .nth(pos)
                            .and_then(|s| {
                                let v = s.value;
                                if v >= 0.0 && v <= 100.0 { Some(v as u32) } else { None }
                            })
                        })
                });

                let adl_idx = self.adl_fan.as_ref().and_then(|adl| adl.adl_index(pos));

                let temperature = lhm_temp
                    .or_else(|| adl_idx.and_then(|idx| self.adl_fan.as_ref().unwrap().temperature(idx)))
                    .or_else(|| query_amd_temp_from_components(&self.components))
                    .or_else(|| self.wmi_thermal.as_ref().and_then(query_amd_temp_from_thermal_wmi));

                // fan_speed (%): ADL preferred; LHM /control/ as fallback
                let fan_speed = adl_idx
                    .and_then(|idx| self.adl_fan.as_ref().unwrap().fan_speed_pct(idx))
                    .or(lhm_fan_pct);

                // VRAM: LHM smalldata/1 = used MB, smalldata/2 = total MB
                let vram_used_mb = lhm.and_then(|sensors| {
                    lhm_bridge::find_by_id(sensors, &format!("/gpu-amd/{pos}/smalldata/1"))
                        .map(|s| s.value as u64).filter(|&v| v > 0)
                });
                let vram_total_mb = lhm.and_then(|sensors| {
                    lhm_bridge::find_by_id(sensors, &format!("/gpu-amd/{pos}/smalldata/2"))
                        .map(|s| s.value as u64).filter(|&v| v > 0)
                }).or_else(|| vram_bytes.map(|b: u64| b >> 20));

                AmdGpuMetrics {
                    index: pos,
                    name,
                    vram_used_mb,
                    vram_total_mb,
                    utilization_3d: util_3d,
                    temperature,
                    fan_speed,
                    fan_rpm:         lhm_fan_rpm,
                    fan_control_available,
                    safety_override_active: self.amd_safety_active.contains(&pos),
                    fan_override:    self.amd_fan_overrides.get(&pos).copied(),
                    cooldown_secs:   self.amd_cooldown_ticks.get(&pos).copied(),
                }
            })
            .collect()
    }

    #[cfg(not(windows))]
    fn collect_amd(&self, _lhm: Option<&[SensorSnapshot]>) -> Vec<AmdGpuMetrics> { vec![] }

    // ── CPU collection ───────────────────────────────────────────────

    fn collect_cpu(&self, lhm: Option<&[SensorSnapshot]>) -> CpuMetrics {
        let overall_usage = self.sys.global_cpu_usage();
        let core_usages: Vec<f32> = self.sys.cpus().iter().map(|c| c.cpu_usage()).collect();
        let name = self.sys.cpus().first()
            .map(|c| c.brand().to_string())
            .unwrap_or_else(|| "CPU".to_string());

        // Priority 1: LHM — reliable for Intel Core Ultra 285K (Arrow Lake) and Zen 4/5.
        let lhm_temp = lhm.and_then(|sensors| {
            // 1a. Exact identifier confirmed for Core Ultra 9 285K
            if let Some(v) = lhm_bridge::find_by_id(sensors, "/intelcpu/0/temperature/26")
                .map(|s| s.value)
                .filter(|&v| v > 0.0 && v < 150.0)
            {
                return Some(v);
            }

            // 1b. Any Intel/AMD CPU package or max-core temperature
            sensors.iter()
                .filter(|s| {
                    let id = s.identifier.to_lowercase();
                    let nm = s.name.to_lowercase();
                    (id.contains("/intelcpu") || id.contains("/amdcpu"))
                        && id.contains("/temperature")
                        && (nm.contains("package")
                            || nm == "core max"
                            || nm.contains("cpu composite")
                            || nm.contains("tdie")
                            || nm.contains("tctl"))
                })
                .map(|s| s.value)
                .find(|&v| v > 0.0 && v < 150.0)
        });

        // Priority 2: sysinfo Components (OpenHardwareMonitor kernel driver)
        let package_temp = lhm_temp.or_else(|| {
            self.components.iter()
                .find(|c| {
                    let l = c.label().to_lowercase();
                    l.contains("package") || l.contains("tctl") || l.contains("tccd")
                })
                .map(|c| c.temperature())
        });

        let fan_rpm = lhm.and_then(|sensors| {
            // Try exact confirmed ID first; fall back to any non-zero /lpc/*/fan/* sensor
            lhm_bridge::find_by_id(sensors, "/lpc/nct6687dr/0/fan/0")
                .map(|s| s.value as u32)
                .filter(|&v| v > 0)
                .or_else(|| {
                    sensors.iter()
                        .find(|s| {
                            let id = s.identifier.to_lowercase();
                            id.starts_with("/lpc/") && id.contains("/fan/") && s.value > 0.0
                        })
                        .map(|s| s.value as u32)
                })
        });

        let ram = lhm.and_then(|sensors| {
            let used = lhm_bridge::find_by_id(sensors, "/ram/data/0")?.value;
            let free = lhm_bridge::find_by_id(sensors, "/ram/data/1")?.value;
            if used > 0.0 {
                Some(SystemMemoryMetrics { used_gb: used, total_gb: used + free })
            } else {
                None
            }
        });

        #[cfg(windows)]
        let npu_usage = self.wmi_con.as_ref().and_then(query_npu_utilization);
        #[cfg(not(windows))]
        let npu_usage: Option<f64> = None;

        CpuMetrics { name, overall_usage, core_usages, package_temp, fan_rpm, ram, npu_usage }
    }

    // ── NVIDIA fan helpers ───────────────────────────────────────────

    fn apply_fan_raw(&self, gpu_index: u32, speed_pct: u32) {
        let Some(fc)   = &self.nvml_fan else { return };
        let Some(nvml) = &self.nvml     else { return };
        let Ok(dev)    = nvml.device_by_index(gpu_index) else { return };
        let handle     = unsafe { dev.handle() as *mut c_void };
        let num_fans   = dev.num_fans().unwrap_or(1);
        for fan in 0..num_fans {
            let ret = unsafe { (fc.set_fn)(handle, fan, speed_pct) };
            if ret != 0 {
                eprintln!("[HybridGauge] NVML SetFan GPU{gpu_index} fan{fan} {speed_pct}%: err {ret}");
            }
        }
    }

    fn reset_fan_raw(&self, gpu_index: u32) {
        let Some(fc)   = &self.nvml_fan else { return };
        let Some(nvml) = &self.nvml     else { return };
        let Ok(dev)    = nvml.device_by_index(gpu_index) else { return };
        let handle     = unsafe { dev.handle() as *mut c_void };
        let num_fans   = dev.num_fans().unwrap_or(1);
        for fan in 0..num_fans {
            let ret = unsafe { (fc.reset_fn)(handle, fan) };
            if ret != 0 {
                eprintln!("[HybridGauge] NVML ResetFan GPU{gpu_index} fan{fan}: err {ret}");
            }
        }
    }

    // ── AMD fan helpers ──────────────────────────────────────────────

    #[cfg(windows)]
    fn apply_amd_fan_raw(&self, position: usize, speed_pct: u32) {
        if let Some(adl) = &self.adl_fan {
            if let Some(idx) = adl.adl_index(position) {
                adl.set_fan(idx, speed_pct);
            }
        }
    }

    #[cfg(not(windows))]
    fn apply_amd_fan_raw(&self, _position: usize, _speed_pct: u32) {}

    #[cfg(windows)]
    fn reset_amd_fan_raw(&self, position: usize) {
        if let Some(adl) = &self.adl_fan {
            if let Some(idx) = adl.adl_index(position) {
                adl.reset_fan(idx);
            }
        }
    }

    #[cfg(not(windows))]
    fn reset_amd_fan_raw(&self, _position: usize) {}
}

// ── AMD temperature fallbacks ──────────────────────────────────────────

fn query_amd_temp_from_components(components: &Components) -> Option<f32> {
    for comp in components.iter() {
        let label = comp.label().to_lowercase();
        let is_gpu = ["gpu", "amd", "radeon", "vga", "tgpu", "display", "gfx"]
            .iter().any(|kw| label.contains(kw));
        if !is_gpu { continue; }
        let t = comp.temperature();
        if t > 0.0 && t < 120.0 { return Some(t); }
    }
    None
}

#[cfg(windows)]
fn query_amd_temp_from_thermal_wmi(wmi: &WMIConnection) -> Option<f32> {
    let rows: Vec<std::collections::HashMap<String, Variant>> = wmi
        .raw_query(
            "SELECT InstanceName, CurrentTemperature \
             FROM MSAcpi_ThermalZoneTemperature",
        )
        .ok()?;

    for mut row in rows {
        let instance = match row.remove("InstanceName") {
            Some(Variant::String(s)) => s.to_lowercase(),
            _ => continue,
        };
        let is_gpu = ["gpu", "vga", "tgpu", "disp", "amd", "gfx"]
            .iter().any(|kw| instance.contains(kw));
        if !is_gpu { continue; }
        if let Some(deci_k) = extract_u64(row.remove("CurrentTemperature")) {
            let celsius = deci_k as f32 / 10.0 - 273.15;
            if (0.0..=150.0).contains(&celsius) { return Some(celsius); }
        }
    }
    None
}

// ── WMI helpers ────────────────────────────────────────────────────────

#[cfg(windows)]
fn init_wmi() -> (Option<WMIConnection>, Option<WMIConnection>) {
    let com = match COMLibrary::new() {
        Ok(c) => c,
        Err(e) => { eprintln!("[HybridGauge] COM init failed: {e}"); return (None, None); }
    };
    let cimv2 = WMIConnection::new(com)
        .map_err(|e| eprintln!("[HybridGauge] WMI root\\cimv2 failed: {e}"))
        .ok();
    let thermal_com = unsafe { COMLibrary::assume_initialized() };
    let thermal = WMIConnection::with_namespace_path("root\\wmi", thermal_com)
        .map_err(|e| eprintln!("[HybridGauge] WMI root\\wmi failed: {e}"))
        .ok();
    (cimv2, thermal)
}

#[cfg(windows)]
fn query_video_controllers(wmi: &WMIConnection) -> Vec<(String, String, Option<u64>)> {
    let rows: Vec<std::collections::HashMap<String, Variant>> =
        match wmi.raw_query(
            "SELECT Name, AdapterCompatibility, AdapterRAM FROM Win32_VideoController",
        ) {
            Ok(r) => r,
            Err(e) => { eprintln!("[HybridGauge] VideoController query failed: {e}"); return vec![]; }
        };
    rows.into_iter()
        .filter_map(|mut row| {
            let name   = extract_string(row.remove("Name"))?;
            let compat = extract_string(row.remove("AdapterCompatibility")).unwrap_or_default();
            let vram   = extract_u64(row.remove("AdapterRAM"));
            Some((name, compat, vram))
        })
        .collect()
}

/// Query Intel NPU (AI Boost) utilization via WMI GPUEngine performance counters.
/// On first call, dumps all unique engine types found so we can identify the NPU adapter.
/// Returns None when NPU is not present or not enumerated by Windows.
#[cfg(windows)]
fn query_npu_utilization(wmi: &WMIConnection) -> Option<f64> {
    let rows: Vec<std::collections::HashMap<String, Variant>> = wmi
        .raw_query(
            "SELECT Name, UtilizationPercentage \
             FROM Win32_PerfFormattedData_GPUPerformanceCounters_GPUEngine",
        )
        .ok()?;

    // One-time diagnostic: log unique engine types to identify NPU adapter
    if !WMI_ENGINE_DUMPED.swap(true, Ordering::Relaxed) {
        eprintln!("[HybridGauge] WMI GPUEngine entries ({}):", rows.len());
        let mut seen = std::collections::HashSet::<String>::new();
        for row in &rows {
            if let Some(Variant::String(name)) = row.get("Name") {
                // engtype is the last '_'-separated segment
                let engtype = name.split('_').last().unwrap_or("?").to_lowercase();
                if seen.insert(engtype.clone()) {
                    let util = extract_u64(row.get("UtilizationPercentage").cloned()).unwrap_or(0);
                    let short = &name[..name.len().min(70)];
                    eprintln!("[HybridGauge]   [{engtype}] {util}% — {short}");
                }
            }
        }
    }

    // Match entries where the adapter description contains NPU / AI Boost keywords
    let (mut sum, mut count) = (0u64, 0usize);
    for row in &rows {
        let name = match row.get("Name") {
            Some(Variant::String(s)) => s.to_lowercase(),
            _ => continue,
        };
        if name.contains("npu") || name.contains("ai boost") || name.contains("neural") {
            if let Some(v) = extract_u64(row.get("UtilizationPercentage").cloned()) {
                sum += v; count += 1;
            }
        }
    }
    if count > 0 { Some(sum as f64 / count as f64) } else { None }
}

#[cfg(windows)]
fn query_gpu_3d_utilization(wmi: &WMIConnection) -> Option<f64> {
    let rows: Vec<std::collections::HashMap<String, Variant>> = wmi
        .raw_query(
            "SELECT UtilizationPercentage FROM \
             Win32_PerfFormattedData_GPUPerformanceCounters_GPUEngine \
             WHERE Name LIKE '%engtype_3D%'",
        )
        .ok()?;
    if rows.is_empty() { return None; }
    let sum: u64 = rows.iter()
        .filter_map(|row| extract_u64(row.get("UtilizationPercentage").cloned()))
        .sum();
    Some(sum as f64 / rows.len() as f64)
}

#[cfg(windows)]
fn extract_string(v: Option<Variant>) -> Option<String> {
    match v? { Variant::String(s) => Some(s), _ => None }
}

#[cfg(windows)]
fn extract_u64(v: Option<Variant>) -> Option<u64> {
    match v? {
        Variant::UI8(n)           => Some(n),
        Variant::UI4(n)           => Some(n as u64),
        Variant::UI2(n)           => Some(n as u64),
        Variant::UI1(n)           => Some(n as u64),
        Variant::I8(n) if n >= 0  => Some(n as u64),
        Variant::I4(n) if n >= 0  => Some(n as u64),
        _ => None,
    }
}
