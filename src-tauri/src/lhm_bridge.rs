//! LibreHardwareMonitor shared-memory bridge.
//!
//! Opens the memory-mapped file created by a running LHM instance and returns
//! a snapshot of all sensor readings.  LHM must be running (elevated is fine)
//! for this to succeed; returns None otherwise.
//!
//! Shared memory layout (CharSet=Unicode, Pack=1 — matches LHM C# source):
//!
//!   Header (20 bytes)
//!     u32  version
//!     u32  revision
//!     i64  timestamp  (Windows FILETIME ticks — 8 bytes)
//!     u32  numSensorData
//!
//!   SharedMemorySensor × numSensorData  (816 bytes each)
//!     u16[128]  name           (256 bytes)
//!     u16[128]  identifier     (256 bytes)
//!     u16[16]   sensorType     (32 bytes)
//!     u16[128]  hardwareName   (256 bytes)
//!     u32       index          (4 bytes, offset 800)
//!     f32       value          (4 bytes, offset 804)
//!     f32       min            (4 bytes, offset 808)
//!     f32       max            (4 bytes, offset 812)

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

/// Try to read all sensors from a running LibreHardwareMonitor instance.
/// Returns `None` if LHM is not running or the shared memory cannot be opened.
pub fn read_sensors() -> Option<Vec<SensorSnapshot>> {
    #[cfg(windows)]
    return read_impl();
    #[cfg(not(windows))]
    None
}

// ── Windows implementation ────────────────────────────────────────────

#[cfg(windows)]
fn read_impl() -> Option<Vec<SensorSnapshot>> {
    use std::ptr;

    const FILE_MAP_READ: u32 = 0x0004;
    // Try the Global namespace first (LHM elevated), fall back to session-local
    const MEM_NAMES: &[&str] = &[
        "Global\\LibreHardwareMonitorSharedMemory",
        "LibreHardwareMonitorSharedMemory",
    ];

    const HEADER_SIZE:  usize = 20;
    const SENSOR_SIZE:  usize = 816; // Unicode layout, Pack=1

    extern "system" {
        fn OpenFileMappingW(access: u32, inherit: i32, name: *const u16) -> isize;
        fn MapViewOfFile(
            handle: isize, access: u32, hi: u32, lo: u32, bytes: usize,
        ) -> *mut u8;
        fn UnmapViewOfFile(addr: *mut u8) -> i32;
        fn CloseHandle(handle: isize) -> i32;
    }

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

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

    // Try each memory name until one opens
    let mut handle: isize = 0;
    for name in MEM_NAMES {
        let wide = to_wide(name);
        handle = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, wide.as_ptr()) };
        if handle != 0 { break; }
    }
    if handle == 0 {
        return None; // LHM not running
    }

    let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ, 0, 0, 0) };
    if view.is_null() {
        unsafe { CloseHandle(handle) };
        return None;
    }

    let result = (|| -> Option<Vec<SensorSnapshot>> {
        // numSensorData sits at byte 16 of the header (after u32+u32+i64)
        let num: u32 = unsafe { ptr::read_unaligned(view.add(16) as *const u32) };

        if num == 0 || num > 8192 {
            return None;
        }

        let mut out = Vec::with_capacity(num as usize);

        for i in 0..num as usize {
            let base = HEADER_SIZE + i * SENSOR_SIZE;
            let b = unsafe { std::slice::from_raw_parts(view.add(base), SENSOR_SIZE) };

            // Byte offsets within one sensor entry (Unicode, Pack=1):
            //   0   name[128]          256 bytes
            // 256   identifier[128]    256 bytes
            // 512   sensorType[16]      32 bytes
            // 544   hardwareName[128]  256 bytes
            // 800   index   u32          4 bytes
            // 804   value   f32          4 bytes
            let name          = decode_utf16(&b[  0..256], 128);
            let identifier    = decode_utf16(&b[256..512], 128);
            let sensor_type   = decode_utf16(&b[512..544],  16);
            let hardware_name = decode_utf16(&b[544..800], 128);
            let value: f32    = unsafe { ptr::read_unaligned(b.as_ptr().add(804) as *const f32) };

            if name.is_empty() || !value.is_finite() {
                continue;
            }

            out.push(SensorSnapshot { name, identifier, sensor_type, hardware_name, value });
        }

        if out.is_empty() { None } else { Some(out) }
    })();

    unsafe {
        UnmapViewOfFile(view);
        CloseHandle(handle);
    }

    result
}
