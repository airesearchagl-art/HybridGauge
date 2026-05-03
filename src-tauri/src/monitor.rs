use nvml_wrapper::{enum_wrappers::device::TemperatureSensor, Nvml};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use sysinfo::{Components, CpuRefreshKind, RefreshKind, System};

#[cfg(windows)]
use wmi::{COMLibrary, Variant, WMIConnection};

use crate::settings::{self, AppSettings, FanOverrideSetting};

// ──────────────────────────────────────────────
// Raw NVML fan control (nvml-wrapper exposes read-only fan API;
// fan speed setting requires nvmlDeviceSetFanSpeed_v2 via libloading)
// ──────────────────────────────────────────────

type NvmlSetFanFn   = unsafe extern "C" fn(*mut c_void, u32, u32) -> u32;
type NvmlResetFanFn = unsafe extern "C" fn(*mut c_void, u32) -> u32;

pub struct NvmlFanControl {
    _lib: libloading::Library,
    set_fn: NvmlSetFanFn,
    reset_fn: NvmlResetFanFn,
}

// Library + raw fn ptrs are all Send on Windows
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

// ──────────────────────────────────────────────
// Fan commands (sent from Tauri command → background thread)
// ──────────────────────────────────────────────

pub enum FanCommand {
    Set { index: u32, speed: Option<u32> },
}

// ──────────────────────────────────────────────
// Data structures sent to the frontend
// ──────────────────────────────────────────────

#[derive(Serialize, Clone, Debug)]
pub struct SystemMetrics {
    pub nvidia_gpus: Vec<NvidiaGpuMetrics>,
    pub amd_gpus:    Vec<AmdGpuMetrics>,
    pub cpu:         CpuMetrics,
}

#[derive(Serialize, Clone, Debug)]
pub struct NvidiaGpuMetrics {
    pub index:          u32,
    pub name:           String,
    pub temperature:    Option<u32>,
    pub utilization_gpu: Option<u32>,
    pub utilization_mem: Option<u32>,
    pub fan_speed:      Option<u32>,
    pub vram_used_mb:   Option<u64>,
    pub vram_total_mb:  Option<u64>,
    // Fan control metadata
    pub fan_control_available: bool,
    pub safety_override_active: bool,
    /// Current manual override speed (None = auto mode).
    pub fan_override: Option<u32>,
    /// Seconds elapsed at ≤20% load while a manual override is active.
    /// Reaches 30 → fan resets to auto (cool-down).
    pub cooldown_secs: Option<u32>,
}

#[derive(Serialize, Clone, Debug)]
pub struct AmdGpuMetrics {
    pub name:            String,
    pub vram_mb:         Option<u64>,
    pub utilization_3d:  Option<f64>,
    pub temperature:     Option<f32>,
}

#[derive(Serialize, Clone, Debug)]
pub struct CpuMetrics {
    pub name:         String,
    pub overall_usage: f32,
    pub core_usages:  Vec<f32>,
    pub package_temp: Option<f32>,
}

// ──────────────────────────────────────────────
// Monitor — long-lived handles
// ──────────────────────────────────────────────

pub struct Monitor {
    nvml:       Option<Nvml>,
    nvml_fan:   Option<NvmlFanControl>,
    sys:        System,
    components: Components,
    // Manual fan overrides: gpu_index → 0..=100
    fan_overrides: HashMap<u32, u32>,
    // GPUs currently under safety-100% override
    safety_active: HashSet<u32>,
    // Cool-down: consecutive seconds each GPU has been at ≤20% load
    cooldown_ticks: HashMap<u32, u32>,
    #[cfg(windows)]
    wmi_con:     Option<WMIConnection>,
    #[cfg(windows)]
    wmi_thermal: Option<WMIConnection>, // root\wmi for thermal zones
}

impl Monitor {
    pub fn new() -> Self {
        let nvml = match Nvml::init() {
            Ok(n) => {
                eprintln!("[HybridGauge] NVIDIA NVML initialized");
                Some(n)
            }
            Err(e) => {
                eprintln!("[HybridGauge] NVML unavailable: {e}");
                None
            }
        };

        let nvml_fan = if nvml.is_some() {
            NvmlFanControl::try_init()
        } else {
            None
        };

        let mut sys = System::new_with_specifics(
            RefreshKind::new().with_cpu(CpuRefreshKind::everything()),
        );
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        sys.refresh_cpu_all();

        let components = Components::new_with_refreshed_list();

        #[cfg(windows)]
        let (wmi_con, wmi_thermal) = init_wmi();
        #[cfg(not(windows))]
        let _ = ();

        // Restore persisted fan overrides from AppData JSON
        let saved = settings::load();
        let fan_overrides: HashMap<u32, u32> = saved
            .fan_overrides
            .iter()
            .map(|s| (s.gpu_index, s.speed))
            .collect();

        let monitor = Monitor {
            nvml,
            nvml_fan,
            sys,
            components,
            fan_overrides,
            safety_active: HashSet::new(),
            cooldown_ticks: HashMap::new(),
            #[cfg(windows)]
            wmi_con,
            #[cfg(windows)]
            wmi_thermal,
        };

        // Apply restored overrides immediately
        let indices: Vec<u32> = monitor.fan_overrides.keys().copied().collect();
        for idx in indices {
            let speed = monitor.fan_overrides[&idx];
            monitor.apply_fan_raw(idx, speed);
            eprintln!("[HybridGauge] Restored fan override GPU{}: {}%", idx, speed);
        }

        monitor
    }

    pub fn collect(&mut self) -> SystemMetrics {
        self.sys.refresh_cpu_all();
        self.components.refresh();

        let nvidia_gpus = self.collect_nvidia();

        // Safety override: force fan to 100% when temp ≥ 85°C, restore at < 80°C
        for gpu in &nvidia_gpus {
            let Some(temp) = gpu.temperature else { continue };
            let idx = gpu.index;

            if temp >= 85 {
                if !self.safety_active.contains(&idx) {
                    self.safety_active.insert(idx);
                    eprintln!(
                        "[HybridGauge] Safety override ON: GPU{} {}°C → fan 100%",
                        idx, temp
                    );
                    self.apply_fan_raw(idx, 100);
                }
            } else if temp < 80 && self.safety_active.contains(&idx) {
                self.safety_active.remove(&idx);
                eprintln!(
                    "[HybridGauge] Safety override OFF: GPU{} {}°C",
                    idx, temp
                );
                let manual = self.fan_overrides.get(&idx).copied();
                match manual {
                    Some(s) => self.apply_fan_raw(idx, s),
                    None    => self.reset_fan_raw(idx),
                }
            }
        }

        // ── Cool-down: GPU load ≤ 20% for 30s → reset manual fan to auto ──
        let mut cooldown_resets: Vec<u32> = Vec::new();
        for gpu in &nvidia_gpus {
            let Some(load) = gpu.utilization_gpu else { continue };
            let idx = gpu.index;

            if !self.fan_overrides.contains_key(&idx) {
                self.cooldown_ticks.remove(&idx);
                continue;
            }

            if load <= 20 {
                let ticks = self.cooldown_ticks.entry(idx).or_insert(0);
                *ticks += 1;
                if *ticks >= 30 {
                    cooldown_resets.push(idx);
                }
            } else {
                self.cooldown_ticks.remove(&idx);
            }
        }
        for idx in cooldown_resets {
            eprintln!(
                "[HybridGauge] Cool-down: GPU{} idle 30s → fan auto",
                idx
            );
            self.fan_overrides.remove(&idx);
            self.cooldown_ticks.remove(&idx);
            self.reset_fan_raw(idx);
            self.persist_settings();
        }

        // Rebuild nvidia_gpus with updated cooldown_secs / fan_override fields
        let nvidia_gpus = self.collect_nvidia();

        SystemMetrics {
            nvidia_gpus,
            amd_gpus: self.collect_amd(),
            cpu:      self.collect_cpu(),
        }
    }

    /// Called by the background thread when it receives a FanCommand.
    pub fn handle_fan_command(&mut self, cmd: FanCommand) {
        match cmd {
            FanCommand::Set { index, speed } => {
                let _ = self.set_fan_override(index, speed);
            }
        }
    }

    pub fn set_fan_override(&mut self, index: u32, speed: Option<u32>) -> Result<(), String> {
        if self.nvml_fan.is_none() {
            return Err("NVML fan control not available (requires NVIDIA driver ≥ 520)".into());
        }
        match speed {
            Some(s) => {
                let s = s.min(100);
                self.fan_overrides.insert(index, s);
                self.cooldown_ticks.remove(&index); // reset cooldown on manual change
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

    fn persist_settings(&self) {
        let s = AppSettings {
            fan_overrides: self
                .fan_overrides
                .iter()
                .map(|(&gpu_index, &speed)| FanOverrideSetting { gpu_index, speed })
                .collect(),
        };
        settings::save(&s);
    }

    // ── NVIDIA ──────────────────────────────────

    fn collect_nvidia(&self) -> Vec<NvidiaGpuMetrics> {
        let nvml = match &self.nvml {
            Some(n) => n,
            None    => return vec![],
        };
        let count = match nvml.device_count() {
            Ok(c)  => c,
            Err(_) => return vec![],
        };

        let fan_control_available = self.nvml_fan.is_some();

        let mut out = Vec::new();
        for i in 0..count {
            let Ok(dev) = nvml.device_by_index(i) else { continue };
            let name        = dev.name().unwrap_or_else(|_| format!("NVIDIA GPU {i}"));
            let temperature = dev.temperature(TemperatureSensor::Gpu).ok();
            let util        = dev.utilization_rates().ok();
            let fan_speed   = dev.fan_speed(0).ok();
            let mem         = dev.memory_info().ok();

            out.push(NvidiaGpuMetrics {
                index: i,
                name,
                temperature,
                utilization_gpu:        util.as_ref().map(|u| u.gpu),
                utilization_mem:        util.as_ref().map(|u| u.memory),
                fan_speed,
                vram_used_mb:           mem.as_ref().map(|m| m.used >> 20),
                vram_total_mb:          mem.as_ref().map(|m| m.total >> 20),
                fan_control_available,
                safety_override_active: self.safety_active.contains(&i),
                fan_override:           self.fan_overrides.get(&i).copied(),
                cooldown_secs:          self.cooldown_ticks.get(&i).copied(),
            });
        }
        out
    }

    // ── AMD (Windows) ────────────────────────────

    #[cfg(windows)]
    fn collect_amd(&self) -> Vec<AmdGpuMetrics> {
        let Some(wmi) = &self.wmi_con else { return vec![] };

        let adapters     = query_video_controllers(wmi);
        let amd_adapters: Vec<_> = adapters
            .into_iter()
            .filter(|(name, compat, _)| {
                let c = compat.to_lowercase();
                let n = name.to_lowercase();
                // Exclude Intel iGPU (AdapterCompatibility or Name contains "intel")
                if c.contains("intel") || n.contains("intel") {
                    return false;
                }
                c.contains("amd") || c.contains("ati")
                    || n.contains("radeon") || n.contains("amd")
            })
            .collect();

        if amd_adapters.is_empty() {
            return vec![];
        }

        let util_3d = query_gpu_3d_utilization(wmi);

        // Temperature: try sysinfo components first, then WMI thermal zones
        let temperature = query_amd_temp_from_components(&self.components)
            .or_else(|| {
                self.wmi_thermal
                    .as_ref()
                    .and_then(query_amd_temp_from_thermal_wmi)
            });

        amd_adapters
            .into_iter()
            .map(|(name, _compat, vram_bytes)| AmdGpuMetrics {
                name,
                vram_mb:        vram_bytes.map(|b: u64| b >> 20),
                utilization_3d: util_3d,
                temperature,
            })
            .collect()
    }

    #[cfg(not(windows))]
    fn collect_amd(&self) -> Vec<AmdGpuMetrics> {
        vec![]
    }

    // ── CPU ─────────────────────────────────────

    fn collect_cpu(&self) -> CpuMetrics {
        let overall_usage = self.sys.global_cpu_usage();
        let core_usages: Vec<f32> =
            self.sys.cpus().iter().map(|c| c.cpu_usage()).collect();
        let name = self
            .sys
            .cpus()
            .first()
            .map(|c| c.brand().to_string())
            .unwrap_or_else(|| "CPU".to_string());

        let package_temp = self
            .components
            .iter()
            .find(|c| {
                let label = c.label().to_lowercase();
                label.contains("package")
                    || label.contains("tctl")
                    || label.contains("tccd")
            })
            .map(|c| c.temperature());

        CpuMetrics { name, overall_usage, core_usages, package_temp }
    }

    // ── Fan helpers ──────────────────────────────

    fn apply_fan_raw(&self, gpu_index: u32, speed_pct: u32) {
        let Some(fc)   = &self.nvml_fan else { return };
        let Some(nvml) = &self.nvml     else { return };
        let Ok(dev)    = nvml.device_by_index(gpu_index) else { return };
        let handle     = unsafe { dev.handle() as *mut c_void };
        let num_fans   = dev.num_fans().unwrap_or(1);
        for fan_idx in 0..num_fans {
            let ret = unsafe { (fc.set_fn)(handle, fan_idx, speed_pct) };
            if ret != 0 {
                eprintln!(
                    "[HybridGauge] SetFanSpeed GPU{} fan{} {}%: NVML err {}",
                    gpu_index, fan_idx, speed_pct, ret
                );
            }
        }
    }

    fn reset_fan_raw(&self, gpu_index: u32) {
        let Some(fc)   = &self.nvml_fan else { return };
        let Some(nvml) = &self.nvml     else { return };
        let Ok(dev)    = nvml.device_by_index(gpu_index) else { return };
        let handle     = unsafe { dev.handle() as *mut c_void };
        let num_fans   = dev.num_fans().unwrap_or(1);
        for fan_idx in 0..num_fans {
            let ret = unsafe { (fc.reset_fn)(handle, fan_idx) };
            if ret != 0 {
                eprintln!(
                    "[HybridGauge] ResetFanSpeed GPU{} fan{}: NVML err {}",
                    gpu_index, fan_idx, ret
                );
            }
        }
    }
}

// ──────────────────────────────────────────────
// AMD temperature helpers
// ──────────────────────────────────────────────

fn query_amd_temp_from_components(components: &Components) -> Option<f32> {
    for comp in components.iter() {
        let label = comp.label().to_lowercase();
        let is_gpu = ["gpu", "amd", "radeon", "vga", "tgpu", "display", "gfx"]
            .iter()
            .any(|kw| label.contains(kw));
        if !is_gpu {
            continue;
        }
        let t = comp.temperature();
        if t > 0.0 && t < 120.0 {
            return Some(t);
        }
    }
    None
}

#[cfg(windows)]
fn query_amd_temp_from_thermal_wmi(wmi: &WMIConnection) -> Option<f32> {
    // MSAcpi_ThermalZoneTemperature.CurrentTemperature is in tenths of Kelvin
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
            .iter()
            .any(|kw| instance.contains(kw));
        if !is_gpu {
            continue;
        }
        if let Some(deci_k) = extract_u64(row.remove("CurrentTemperature")) {
            let celsius = deci_k as f32 / 10.0 - 273.15;
            if (0.0..=150.0).contains(&celsius) {
                return Some(celsius);
            }
        }
    }
    None
}

// ──────────────────────────────────────────────
// WMI helpers (Windows-only)
// ──────────────────────────────────────────────

#[cfg(windows)]
fn init_wmi() -> (Option<WMIConnection>, Option<WMIConnection>) {
    let com = match COMLibrary::new() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[HybridGauge] COM init failed: {e}");
            return (None, None);
        }
    };
    let cimv2 = WMIConnection::new(com)
        .map_err(|e| eprintln!("[HybridGauge] WMI root\\cimv2 failed: {e}"))
        .ok();

    // COM is already initialised for this thread; create a second connection
    // to root\wmi for ACPI thermal zone queries.
    let thermal_com = unsafe { COMLibrary::assume_initialized() };
    let thermal = WMIConnection::with_namespace_path("root\\wmi", thermal_com)
        .map_err(|e| eprintln!("[HybridGauge] WMI root\\wmi failed: {e}"))
        .ok();

    (cimv2, thermal)
}

#[cfg(windows)]
fn query_video_controllers(
    wmi: &WMIConnection,
) -> Vec<(String, String, Option<u64>)> {
    let query =
        "SELECT Name, AdapterCompatibility, AdapterRAM FROM Win32_VideoController";
    let rows: Vec<std::collections::HashMap<String, Variant>> =
        match wmi.raw_query(query) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("[HybridGauge] VideoController query failed: {e}");
                return vec![];
            }
        };

    rows.into_iter()
        .filter_map(|mut row| {
            let name   = extract_string(row.remove("Name"))?;
            let compat = extract_string(row.remove("AdapterCompatibility"))
                .unwrap_or_default();
            let vram   = extract_u64(row.remove("AdapterRAM"));
            Some((name, compat, vram))
        })
        .collect()
}

#[cfg(windows)]
fn query_gpu_3d_utilization(wmi: &WMIConnection) -> Option<f64> {
    let query = "SELECT UtilizationPercentage FROM \
        Win32_PerfFormattedData_GPUPerformanceCounters_GPUEngine \
        WHERE Name LIKE '%engtype_3D%'";
    let rows: Vec<std::collections::HashMap<String, Variant>> =
        wmi.raw_query(query).ok()?;
    if rows.is_empty() {
        return None;
    }
    let sum: u64 = rows
        .iter()
        .filter_map(|row| {
            extract_u64(row.get("UtilizationPercentage").cloned())
        })
        .sum();
    Some(sum as f64 / rows.len() as f64)
}

#[cfg(windows)]
fn extract_string(v: Option<Variant>) -> Option<String> {
    match v? {
        Variant::String(s) => Some(s),
        _ => None,
    }
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
