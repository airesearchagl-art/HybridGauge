use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct AppSettings {
    #[serde(default)]
    pub fan_overrides: Vec<FanOverrideSetting>,
    #[serde(default)]
    pub amd_fan_overrides: Vec<AmdFanOverrideSetting>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct FanOverrideSetting {
    pub gpu_index: u32,
    pub speed: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AmdFanOverrideSetting {
    pub gpu_position: usize,
    pub speed: u32,
}

/// Returns the path to the JSON settings file inside the OS AppData directory.
/// Uses confy for platform-correct path discovery, then switches extension to .json.
fn config_path() -> PathBuf {
    confy::get_configuration_file_path("hybrid-gauge", None::<&str>)
        .map(|p| p.with_extension("json"))
        .unwrap_or_else(|_| PathBuf::from("hybrid-gauge-settings.json"))
}

pub fn load() -> AppSettings {
    let path = config_path();
    eprintln!("[HybridGauge] Settings path: {}", path.display());
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(settings: &AppSettings) {
    let path = config_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match serde_json::to_string_pretty(settings) {
        Ok(json) => {
            if let Err(e) = std::fs::write(&path, json) {
                eprintln!("[HybridGauge] Settings save failed: {e}");
            }
        }
        Err(e) => eprintln!("[HybridGauge] Settings serialize failed: {e}"),
    }
}
