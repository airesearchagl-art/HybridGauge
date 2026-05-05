//! LibreHardwareMonitor sensor bridge.
//!
//! Two data paths (tried in order):
//!   1. Shared memory (OpenFileMappingW) — fast, but requires LHM with SharedMemory plugin.
//!   2. Remote Web Server HTTP (http://localhost:8085/data.json) — requires Remote Web Server
//!      to be enabled in LHM Options.  Works on all recent LHM versions without any plugin.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct SensorSnapshot {
    pub name:          String,
    pub identifier:    String,
    pub sensor_type:   String,
    pub hardware_name: String,
    pub value:         f32,
}

/// Find the first sensor whose identifier exactly matches `id` (case-insensitive).
pub fn find_by_id<'a>(sensors: &'a [SensorSnapshot], id: &str) -> Option<&'a SensorSnapshot> {
    let id_lower = id.to_lowercase();
    sensors.iter().find(|s| s.identifier.to_lowercase() == id_lower)
}

/// Collect sensors whose identifier contains every string in `must` and none in `exclude`.
pub fn filter_by_id<'a>(
    sensors: &'a [SensorSnapshot],
    must:    &[&str],
    exclude: &[&str],
) -> Vec<&'a SensorSnapshot> {
    sensors.iter()
        .filter(|s| {
            let id = s.identifier.to_lowercase();
            must.iter().all(|kw| id.contains(kw))
                && !exclude.iter().any(|kw| id.contains(kw))
        })
        .collect()
}

static DUMPED:     AtomicBool = AtomicBool::new(false);
/// Set to true after the first tick where all OpenFileMappingW attempts fail.
/// Suppresses duplicate error spam on subsequent ticks.
static LHM_WARNED: AtomicBool = AtomicBool::new(false);

/// Log a one-time summary of connected LHM sensors.
pub fn dump_sensors(sensors: &[SensorSnapshot]) {
    if DUMPED.swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!("[LHM] === Sensor dump ({} sensors) ===", sensors.len());
    for s in sensors {
        eprintln!(
            "[LHM]  type={:<14} val={:>8.2}  id={}",
            s.sensor_type, s.value, s.identifier
        );
    }
    eprintln!("[LHM] === End sensor dump ===");

    let cpu = find_by_id(sensors, "/intelcpu/0/temperature/26");
    let amd = find_by_id(sensors, "/gpu-amd/0/temperature/0");
    eprintln!(
        "[LHM] CPU package = {}  AMD GPU = {}",
        cpu.map_or("N/A".to_string(), |s| format!("{:.1}°C", s.value)),
        amd.map_or("N/A".to_string(), |s| format!("{:.1}°C", s.value)),
    );
}

/// Try to read all sensors from a running LibreHardwareMonitor instance.
///
/// Tries shared memory first; if that is unavailable, falls back to the
/// LHM Remote Web Server HTTP API (http://localhost:8085/data.json).
pub fn read_sensors() -> Option<Vec<SensorSnapshot>> {
    #[cfg(windows)]
    {
        if let Some(s) = read_impl() { return Some(s); }
        poll_lhm_http()
    }
    #[cfg(not(windows))]
    poll_lhm_http()
}

// ── LHM Remote Web Server HTTP polling ───────────────────────────────────

static HTTP_WARNED: AtomicBool = AtomicBool::new(false);

/// Poll http://localhost:8085/data.json and parse the sensor tree.
/// Returns None when LHM Remote Web Server is not enabled/reachable.
fn poll_lhm_http() -> Option<Vec<SensorSnapshot>> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(500))
        .build();

    match agent.get("http://localhost:8085/data.json").call() {
        Ok(resp) => {
            HTTP_WARNED.store(false, Ordering::Relaxed);
            let body = resp.into_string().ok()?;
            let json: serde_json::Value = serde_json::from_str(&body).ok()?;
            let mut sensors = Vec::new();
            traverse_json_node(&json, "", &mut sensors);
            if sensors.is_empty() { None } else {
                dump_sensors(&sensors);
                Some(sensors)
            }
        }
        Err(_) => {
            if !HTTP_WARNED.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "[LHM] HTTP: http://localhost:8085 unreachable. \
                     Enable Remote Web Server in LHM Options to get CPU/GPU temperatures."
                );
            }
            None
        }
    }
}

fn traverse_json_node(node: &serde_json::Value, hw_name: &str, out: &mut Vec<SensorSnapshot>) {
    // Determine hardware name for this subtree
    let text = node.get("Text").and_then(|v| v.as_str()).unwrap_or("");
    let effective_hw = if hw_name.is_empty() { text } else { hw_name };

    // If this node carries a SensorId it is a sensor leaf
    if let Some(id) = node.get("SensorId").and_then(|v| v.as_str()) {
        if let Some(raw_val) = node.get("Value").and_then(|v| v.as_str()) {
            if let Some(value) = parse_sensor_value(raw_val) {
                let sensor_type = node.get("Type").and_then(|v| v.as_str()).unwrap_or("").to_string();
                out.push(SensorSnapshot {
                    name:          text.to_string(),
                    identifier:    id.to_string(),
                    sensor_type,
                    hardware_name: effective_hw.to_string(),
                    value,
                });
            }
        }
    }

    if let Some(children) = node.get("Children").and_then(|v| v.as_array()) {
        for child in children {
            traverse_json_node(child, effective_hw, out);
        }
    }
}

/// Parse LHM value strings like "55.0 °C", "1200 RPM", "65.2 %", "120.5 W".
fn parse_sensor_value(s: &str) -> Option<f32> {
    let num: String = s.trim()
        .chars()
        .take_while(|&c| c.is_ascii_digit() || c == '.' || c == ',' || c == '-')
        .collect();
    num.replace(',', ".").parse::<f32>().ok().filter(|v| v.is_finite())
}

// ── Windows implementation ────────────────────────────────────────────

#[cfg(windows)]
fn decode_utf16(bytes: &[u8], max_chars: usize) -> String {
    let pairs = max_chars.min(bytes.len() / 2);
    let words: Vec<u16> = bytes
        .chunks_exact(2)
        .take(pairs)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&w| w != 0)
        .collect();
    String::from_utf16_lossy(&words).to_string()
}

#[cfg(windows)]
fn decode_ansi(bytes: &[u8], max_len: usize) -> String {
    let data = &bytes[..max_len.min(bytes.len())];
    let end  = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    String::from_utf8_lossy(&data[..end]).to_string()
}

/// Parse sensors using the Unicode layout (816 bytes each).
///
/// # Safety
/// `view` must point to a mapped region large enough to hold
/// `header_size + num * 816` bytes.
#[cfg(windows)]
unsafe fn parse_unicode(view: *const u8, num: usize, header_size: usize) -> Vec<SensorSnapshot> {
    use std::ptr;
    const SENSOR_SIZE: usize = 816;

    let mut out = Vec::with_capacity(num);
    for i in 0..num {
        let base = header_size + i * SENSOR_SIZE;
        let b    = std::slice::from_raw_parts(view.add(base), SENSOR_SIZE);

        let name          = decode_utf16(&b[  0..256], 128);
        let identifier    = decode_utf16(&b[256..512], 128);
        let sensor_type   = decode_utf16(&b[512..544],  16);
        let hardware_name = decode_utf16(&b[544..800], 128);
        let value: f32    = ptr::read_unaligned(b.as_ptr().add(804) as *const f32);

        if name.is_empty() || !value.is_finite() { continue; }
        out.push(SensorSnapshot { name, identifier, sensor_type, hardware_name, value });
    }
    out
}

/// Parse sensors using the ANSI layout (416 bytes each).
///
/// # Safety
/// `view` must point to a mapped region large enough to hold
/// `header_size + num * 416` bytes.
#[cfg(windows)]
unsafe fn parse_ansi(view: *const u8, num: usize, header_size: usize) -> Vec<SensorSnapshot> {
    use std::ptr;
    const SENSOR_SIZE: usize = 416;

    let mut out = Vec::with_capacity(num);
    for i in 0..num {
        let base = header_size + i * SENSOR_SIZE;
        let b    = std::slice::from_raw_parts(view.add(base), SENSOR_SIZE);

        let name          = decode_ansi(&b[  0..128], 128);
        let identifier    = decode_ansi(&b[128..256], 128);
        let sensor_type   = decode_ansi(&b[256..272],  16);
        let hardware_name = decode_ansi(&b[272..400], 128);
        let value: f32    = ptr::read_unaligned(b.as_ptr().add(404) as *const f32);

        if name.is_empty() || !value.is_finite() { continue; }
        out.push(SensorSnapshot { name, identifier, sensor_type, hardware_name, value });
    }
    out
}

/// Enumerate NT object-manager directories looking for Section objects whose
/// names contain "libre", "hardware", "lhm", or "ohm".  Runs at most once.
/// Uses ntdll.dll native APIs via libloading.
#[cfg(windows)]
fn nt_scan_for_lhm() {
    #[repr(C)]
    struct Us { len: u16, max: u16, buf: *const u16 }
    #[repr(C)]
    struct Oa { sz: u32, root: isize, name: *const Us, attr: u32, sd: *const (), sqos: *const () }
    #[repr(C)]
    struct Dbi { obj_name: Us, obj_type: Us }

    type FnOpenDir  = unsafe extern "system" fn(*mut isize, u32, *const Oa) -> i32;
    type FnQueryDir = unsafe extern "system" fn(isize, *mut u8, u32, u8, u8, *mut u32, *mut u32) -> i32;
    type FnClose    = unsafe extern "system" fn(isize) -> i32;

    extern "system" {
        fn GetCurrentProcessId() -> u32;
        fn ProcessIdToSessionId(pid: u32, session_id: *mut u32) -> i32;
    }

    let ntdll = match unsafe { libloading::Library::new("ntdll.dll") } {
        Ok(l) => l,
        Err(e) => { eprintln!("[LHM] NT scan: cannot load ntdll.dll: {e}"); return; }
    };
    let open_dir:  FnOpenDir  = unsafe { *ntdll.get(b"NtOpenDirectoryObject\0") .unwrap() };
    let query_dir: FnQueryDir = unsafe { *ntdll.get(b"NtQueryDirectoryObject\0").unwrap() };
    let nt_close:  FnClose    = unsafe { *ntdll.get(b"NtClose\0")               .unwrap() };

    // Determine the current Windows session ID
    let current_session = {
        let mut sid = 0u32;
        if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut sid) } != 0 {
            sid
        } else {
            1 // fallback
        }
    };
    eprintln!("[LHM] NT scan: current process session ID = {current_session}");

    // Helper: open an NT directory by path, return handle or None
    let open_nt_dir = |path: &str| -> Option<isize> {
        let w: Vec<u16> = path.encode_utf16().collect();
        let us = Us { len: (w.len() * 2) as u16, max: (w.len() * 2) as u16, buf: w.as_ptr() };
        let oa = Oa {
            sz: std::mem::size_of::<Oa>() as u32,
            root: 0, name: &us, attr: 0x40,
            sd: std::ptr::null(), sqos: std::ptr::null(),
        };
        let mut dh: isize = 0;
        let st = unsafe { open_dir(&mut dh, 1, &oa) };
        if st == 0 { Some(dh) } else {
            eprintln!("[LHM]   {path} → NtOpenDirectory 0x{:08x}", st as u32);
            None
        }
    };

    // Helper: scan a directory handle for LHM-related Section objects
    let scan_dir = |dh: isize, label: &str| -> bool {
        let mut data = vec![0u8; 65536];
        let mut ctx = 0u32; let mut rlen = 0u32;
        let st = unsafe { query_dir(dh, data.as_mut_ptr(), data.len() as u32, 0, 1, &mut ctx, &mut rlen) };
        if st != 0 && (st as u32) != 0x0000_0105 {
            eprintln!("[LHM]   {label} → NtQueryDirectory 0x{:08x}", st as u32);
            return false;
        }
        let buf_start = data.as_ptr() as usize;
        let buf_end   = buf_start + (rlen as usize).min(data.len());
        let step      = std::mem::size_of::<Dbi>();
        let mut off   = 0usize;
        let mut found = false;
        loop {
            if off + step > data.len() { break; }
            let e = unsafe { &*(data.as_ptr().add(off) as *const Dbi) };
            if e.obj_name.len == 0 { break; }
            let np = e.obj_name.buf as usize; let nl = e.obj_name.len as usize;
            let tp = e.obj_type.buf as usize; let tl = e.obj_type.len as usize;
            if np < buf_start || np.saturating_add(nl) > buf_end { off += step; continue; }
            if tp < buf_start || tp.saturating_add(tl) > buf_end { off += step; continue; }
            let name = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(e.obj_name.buf, nl / 2) });
            let typ  = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(e.obj_type.buf, tl / 2) });
            let nl_lc = name.to_lowercase();
            if typ == "Section" && (nl_lc.contains("libre") || nl_lc.contains("hardware") || nl_lc.contains("lhm") || nl_lc.contains("ohm")) {
                eprintln!("[LHM]   FOUND Section '{name}' in {label}");
                eprintln!("[LHM]   → Win32 name to add: if in Global=>\"{name}\", if in Sessions\\N=>\"Local\\{name}\"");
                found = true;
            }
            off += step;
        }
        found
    };

    eprintln!("[LHM] NT scan: scanning directories...");
    let mut found_any = false;

    // 1. Global namespace
    if let Some(dh) = open_nt_dir(r"\BaseNamedObjects") {
        found_any |= scan_dir(dh, r"\BaseNamedObjects");
        unsafe { nt_close(dh) };
    }

    // 2. Enumerate \Sessions\ to find all session directories
    let session_dirs: Vec<u32> = if let Some(sessions_dh) = open_nt_dir(r"\Sessions") {
        let mut data = vec![0u8; 65536];
        let mut ctx = 0u32; let mut rlen = 0u32;
        let st = unsafe { query_dir(sessions_dh, data.as_mut_ptr(), data.len() as u32, 0, 1, &mut ctx, &mut rlen) };
        unsafe { nt_close(sessions_dh) };
        let mut sids = Vec::new();
        if st == 0 || (st as u32) == 0x0000_0105 {
            let buf_start = data.as_ptr() as usize;
            let buf_end   = buf_start + (rlen as usize).min(data.len());
            let step      = std::mem::size_of::<Dbi>();
            let mut off   = 0usize;
            loop {
                if off + step > data.len() { break; }
                let e = unsafe { &*(data.as_ptr().add(off) as *const Dbi) };
                if e.obj_name.len == 0 { break; }
                let np = e.obj_name.buf as usize; let nl = e.obj_name.len as usize;
                if np >= buf_start && np.saturating_add(nl) <= buf_end {
                    let n = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(e.obj_name.buf, nl / 2) });
                    eprintln!("[LHM]   \\Sessions child: '{n}'");
                    if let Ok(id) = n.parse::<u32>() { sids.push(id); }
                }
                off += step;
            }
        }
        sids
    } else {
        // \Sessions not accessible — fall back to probing 0..=5 and current session
        (0u32..=5).chain(std::iter::once(current_session)).collect()
    };

    // 3. Scan BaseNamedObjects of each discovered session
    for sid in &session_dirs {
        let path = format!(r"\Sessions\{sid}\BaseNamedObjects");
        if let Some(dh) = open_nt_dir(&path) {
            found_any |= scan_dir(dh, &path);
            unsafe { nt_close(dh) };
        }
    }

    if !found_any {
        eprintln!("[LHM]   Shared memory not available — using Remote Web Server (HTTP) instead.");
    }
}

#[cfg(windows)]
fn read_impl() -> Option<Vec<SensorSnapshot>> {
    use std::ptr;

    const FILE_MAP_READ: u32 = 0x0004;
    const MEM_NAMES: &[&str] = &[
        // Global namespace (accessible cross-session)
        "Global\\LibreHardwareMonitorSharedMemory",
        "Global\\LibreHardwareMonitor_Window",
        "Global\\LibreHardwareMonitor",
        // Local session namespace
        "LibreHardwareMonitorSharedMemory",
        "LibreHardwareMonitor",
        "Local\\LibreHardwareMonitorSharedMemory",
        "Local\\LibreHardwareMonitor",
    ];
    const HEADER_SIZE: usize = 20;

    extern "system" {
        fn OpenFileMappingW(access: u32, inherit: i32, name: *const u16) -> isize;
        fn MapViewOfFile(handle: isize, access: u32, hi: u32, lo: u32, bytes: usize) -> *mut u8;
        fn UnmapViewOfFile(addr: *mut u8) -> i32;
        fn CloseHandle(handle: isize) -> i32;
        fn GetLastError() -> u32;
    }

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    // First-time-only logging flag (avoids spamming 7 error lines every second)
    let first_attempt = !LHM_WARNED.load(Ordering::Relaxed);

    // Try each memory name, log GetLastError() on the first failure round
    let mut handle: isize = 0;
    let mut opened_name   = "";
    for &name in MEM_NAMES {
        let wide = to_wide(name);
        handle = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, wide.as_ptr()) };
        if handle != 0 {
            opened_name = name;
            // Successful connection — reset warned flag so failures are reported again if LHM stops
            LHM_WARNED.store(false, Ordering::Relaxed);
            break;
        }
        if first_attempt {
            let err = unsafe { GetLastError() };
            let hint = match err {
                2   => "ERROR_FILE_NOT_FOUND — LHM not running or name mismatch",
                5   => "ERROR_ACCESS_DENIED — run HybridGauge as Administrator",
                6   => "ERROR_INVALID_HANDLE",
                231 => "ERROR_PIPE_BUSY",
                _   => "unknown",
            };
            eprintln!("[LHM] OpenFileMappingW('{name}') failed: GetLastError={err} ({hint})");
        }
    }

    if handle == 0 {
        if first_attempt {
            LHM_WARNED.store(true, Ordering::Relaxed);
            // Enumerate NT namespace to find what name LHM actually registered
            nt_scan_for_lhm();
        }
        return None;
    }

    let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ, 0, 0, 0) };
    if view.is_null() {
        let err = unsafe { GetLastError() };
        eprintln!("[LHM] MapViewOfFile failed: GetLastError={err}");
        unsafe { CloseHandle(handle) };
        return None;
    }

    let result = (|| -> Option<Vec<SensorSnapshot>> {
        // numSensorData sits at byte 16 (after u32 + u32 + i64)
        let num: u32 = unsafe { ptr::read_unaligned(view.add(16) as *const u32) };

        if num == 0 || num > 8192 {
            eprintln!("[LHM] '{opened_name}' opened but numSensorData={num} — invalid range");
            return None;
        }

        eprintln!("[LHM] Opened '{opened_name}', numSensorData={num}");

        // Auto-detect layout: try both, pick the one with more valid identifiers
        let unicode_sensors = unsafe { parse_unicode(view as *const u8, num as usize, HEADER_SIZE) };
        let ansi_sensors    = unsafe { parse_ansi   (view as *const u8, num as usize, HEADER_SIZE) };

        let unicode_score = unicode_sensors.iter().filter(|s| s.identifier.len() > 3).count();
        let ansi_score    = ansi_sensors.iter().filter(|s| s.identifier.len() > 3).count();

        eprintln!(
            "[LHM] Layout detection — Unicode: {unicode_score} valid, ANSI: {ansi_score} valid → using {}",
            if unicode_score >= ansi_score { "Unicode" } else { "ANSI" }
        );

        let sensors = if unicode_score >= ansi_score { unicode_sensors } else { ansi_sensors };

        if sensors.is_empty() { return None; }

        dump_sensors(&sensors);
        Some(sensors)
    })();

    unsafe {
        UnmapViewOfFile(view);
        CloseHandle(handle);
    }

    result
}
