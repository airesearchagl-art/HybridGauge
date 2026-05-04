mod lhm_bridge;
mod monitor;
mod settings;
use monitor::{FanCommand, Monitor};

use std::sync::mpsc::{self, Sender};
use std::time::Duration;
use tauri::{Emitter, State};

pub struct FanControl(pub Sender<FanCommand>);

#[tauri::command]
fn set_fan_speed(
    ctrl: State<FanControl>,
    index: u32,
    speed: Option<u32>,
) -> Result<(), String> {
    ctrl.0
        .send(FanCommand::Set { index, speed })
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn set_amd_fan_speed(
    ctrl: State<FanControl>,
    index: usize,
    speed: Option<u32>,
) -> Result<(), String> {
    ctrl.0
        .send(FanCommand::SetAmd { index, speed })
        .map_err(|e| e.to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let (fan_tx, fan_rx) = mpsc::channel::<FanCommand>();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(FanControl(fan_tx))
        .setup(|app| {
            let handle = app.handle().clone();

            std::thread::spawn(move || {
                let mut monitor = Monitor::new();
                loop {
                    // Apply pending fan commands before sleeping
                    while let Ok(cmd) = fan_rx.try_recv() {
                        monitor.handle_fan_command(cmd);
                    }

                    std::thread::sleep(Duration::from_secs(1));

                    let metrics = monitor.collect();
                    if let Err(e) = handle.emit("metrics-update", &metrics) {
                        eprintln!("[HybridGauge] emit error: {e}");
                    }
                }
            });

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![set_fan_speed, set_amd_fan_speed])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
