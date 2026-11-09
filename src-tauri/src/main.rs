// Prevents the console window from popping up on Windows
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral, ScanFilter, WriteType,
};
use btleplug::platform::{Manager, Peripheral as PlatformPeripheral};
use futures::stream::StreamExt;
use std::time::Duration;
use tauri::{
    CustomMenuItem, Manager as TauriManager, State, SystemTray, SystemTrayEvent, SystemTrayMenu,
    SystemTrayMenuItem,
};
use tokio::sync::Mutex;
use tokio::time;
use uuid::Uuid;

// ── OPO Protocol UUIDs ──────────────────────────────────────────────
const WRITE_UUID: Uuid = Uuid::from_u128(0x0100079ad10211e19b2300025b00a5a5);
const NOTIFY_UUID: Uuid = Uuid::from_u128(0x0200079ad10211e19b2300025b00a5a5);

// ── Authentication packets (from open-oneplus-buds protocol) ────────
const CMD_HELLO: [u8; 10] = [0xAA, 0x07, 0x00, 0x00, 0x00, 0x01, 0x23, 0x00, 0x00, 0x12];
const CMD_REGISTER: [u8; 14] = [
    0xAA, 0x0C, 0x00, 0x00, 0x00, 0x85, 0x41, 0x05, 0x00, 0x00, 0xB5, 0x50, 0xA0, 0x69,
];

// ── ANC commands ────────────────────────────────────────────────────
const CMD_ANC_ON: [u8; 12] = [0xAA, 0x0A, 0x00, 0x00, 0x04, 0x04, 0x42, 0x03, 0x00, 0x01, 0x01, 0x01];
const CMD_ANC_TRANS: [u8; 12] = [0xAA, 0x0A, 0x00, 0x00, 0x04, 0x04, 0x42, 0x03, 0x00, 0x01, 0x01, 0x02];
const CMD_ANC_OFF: [u8; 12] = [0xAA, 0x0A, 0x00, 0x00, 0x04, 0x04, 0x42, 0x03, 0x00, 0x01, 0x01, 0x04];
const CMD_ANC_QUERY: [u8; 11] = [0xAA, 0x09, 0x00, 0x00, 0x04, 0x82, 0x44, 0x02, 0x00, 0x00, 0xF2];

// ── EQ commands ─────────────────────────────────────────────────────
const CMD_EQ_BALANCED: [u8; 12] = [0xAA, 0x0A, 0x00, 0x00, 0x05, 0x04, 0x2B, 0x03, 0x00, 0x01, 0x01, 0x01];
const CMD_EQ_BASS: [u8; 12] = [0xAA, 0x0A, 0x00, 0x00, 0x05, 0x04, 0x2B, 0x03, 0x00, 0x01, 0x01, 0x02];
const CMD_EQ_SERENADE: [u8; 12] = [0xAA, 0x0A, 0x00, 0x00, 0x05, 0x04, 0x2B, 0x03, 0x00, 0x01, 0x01, 0x03];
const CMD_EQ_QUERY: [u8; 9] = [0xAA, 0x07, 0x00, 0x00, 0x05, 0x01, 0x2B, 0x00, 0x00];

// ── Battery ─────────────────────────────────────────────────────────
const CMD_BATT_QUERY: [u8; 9] = [0xAA, 0x07, 0x00, 0x00, 0x06, 0x01, 0x25, 0x00, 0x00];

struct BluetoothState {
    device: Mutex<Option<PlatformPeripheral>>,
    write_char: Mutex<Option<Characteristic>>,
    authenticated: Mutex<bool>,
    is_connecting: Mutex<bool>,
}

#[derive(Clone, serde::Serialize)]
struct BatteryPayload {
    left: u8,
    right: u8,
    case: u8,
}

#[derive(Clone, serde::Serialize)]
struct ANCPayload {
    mode: String,
}

async fn opo_authenticate(
    device: &PlatformPeripheral,
    write_char: &Characteristic,
) -> Result<(), String> {
    println!("[OPO] Handshake step 1: HELLO...");
    device
        .write(write_char, &CMD_HELLO, WriteType::WithoutResponse)
        .await
        .map_err(|e| format!("HELLO failed: {e}"))?;

    time::sleep(Duration::from_secs(2)).await;

    println!("[OPO] Handshake step 2: REGISTER token...");
    device
        .write(write_char, &CMD_REGISTER, WriteType::WithoutResponse)
        .await
        .map_err(|e| format!("REGISTER failed: {e}"))?;

    time::sleep(Duration::from_millis(1500)).await;
    println!("[OPO] Handshake complete!");

    Ok(())
}

fn parse_battery(val: &[u8]) -> Option<BatteryPayload> {
    if val.len() < 12
        || val[0] != 0xAA
        || val[4] != 0x06
        || val[5] != 0x81
        || val[6] != 0x25
    {
        return None;
    }

    if val.len() > 10 {
        let count = val[10] as usize;
        let start = 11;
        let need = start + count * 2;
        if (1..=3).contains(&count) && val.len() >= need {
            let (mut l, mut r, mut c) = (0u8, 0u8, 0u8);
            for k in 0..count {
                let idx = start + k * 2;
                match val[idx] {
                    0x01 => l = val[idx + 1],
                    0x02 => r = val[idx + 1],
                    0x03 => c = val[idx + 1],
                    _ => {}
                }
            }
            if (l > 0 && l <= 100) || (r > 0 && r <= 100) {
                return Some(BatteryPayload { left: l, right: r, case: c });
            }
        }
    }

    if val.len() >= 17 {
        let (l, r, c) = (val[12], val[14], val[16]);
        if (l > 0 && l <= 100) || (r > 0 && r <= 100) {
            return Some(BatteryPayload { left: l, right: r, case: c });
        }
    }

    None
}

fn parse_anc_notification(val: &[u8]) -> Option<String> {
    if val.len() < 8 || val[0] != 0xAA || val[4] != 0x04 {
        return None;
    }
    match val[5] {
        0x81 => {
            for i in 6..val.len().saturating_sub(4) {
                if val[i] == 0x03
                    && val[i + 1] == 0x00
                    && val[i + 2] == 0x01
                    && val[i + 3] == 0x01
                {
                    if let Some(&b) = val.get(i + 4) {
                        return match b {
                            0x01 => Some("anc_on".into()),
                            0x02 => Some("anc_trans".into()),
                            0x04 => Some("anc_off".into()),
                            _ => None,
                        };
                    }
                }
            }
            None
        }
        0x02 if val.len() >= 12 => {
            let n = val.len();
            if val[n - 4] == 0x03 && val[n - 3] == 0x01 && val[n - 2] == 0x01 {
                return match val[n - 1] {
                    0x10 => Some("anc_on".into()),
                    0x04 => Some("anc_trans".into()),
                    0x01 => Some("anc_off".into()),
                    _ => None,
                };
            }
            None
        }
        _ => None,
    }
}

async fn connect_device_internal(
    app_handle: &tauri::AppHandle,
    state: &State<'_, BluetoothState>,
) -> Result<String, String> {
    *state.authenticated.lock().await = false;

    // Cleanly release any prior peripheral session
    {
        let mut old_dev = state.device.lock().await;
        if let Some(d) = old_dev.take() {
            println!("[BUDS] Releasing prior device session...");
            let _ = d.disconnect().await;
            time::sleep(Duration::from_millis(200)).await;
        }
    }

    println!("[BUDS] Initializing Bluetooth Manager...");
    let manager = Manager::new().await.map_err(|e| e.to_string())?;
    let adapters = manager.adapters().await.map_err(|e| e.to_string())?;
    if adapters.is_empty() {
        return Err("No Bluetooth adapter found on Windows".into());
    }
    let central = &adapters[0];

    println!("[BUDS] Scanning for 4.5 seconds...");
    let _ = central.start_scan(ScanFilter::default()).await;
    time::sleep(Duration::from_millis(4500)).await;
    let _ = central.stop_scan().await;

    let peripherals = central.peripherals().await.map_err(|e| e.to_string())?;
    println!("[BUDS] Discovered {} peripherals nearby", peripherals.len());
    let mut found_names = Vec::new();

    for peripheral in peripherals {
        let addr = peripheral.address();
        let props = match peripheral.properties().await {
            Ok(Some(p)) => p,
            _ => continue,
        };
        let name = props.local_name.unwrap_or_default();
        println!("[BUDS] Nearby peripheral: '{}' [{:?}]", name, addr);
        if !name.is_empty() {
            found_names.push(format!("{} [{:?}]", name, addr));
        }

        let is_target = name.contains("Nord Buds")
            || name.contains("Buds")
            || name.contains("OnePlus")
            || format!("{:?}", addr).to_uppercase().contains("58:03:70");

        if !is_target {
            continue;
        }

        let display_name = if name.is_empty() { "OnePlus Nord Buds 3 Pro".to_string() } else { name.clone() };
        println!("[BUDS] Found target earbud: {} [{:?}]", display_name, addr);

        if !peripheral.is_connected().await.unwrap_or(false) {
            println!("[BUDS] Connecting...");
            if let Err(e) = peripheral.connect().await {
                println!("[BUDS] Connect failed ({:?}), trying next peripheral...", e);
                continue;
            }
            println!("[BUDS] Connected!");
        }

        println!("[BUDS] Discovering services...");
        let _ = peripheral.discover_services().await;

        let chars = peripheral.characteristics();
        println!("[BUDS] Discovered {} characteristics", chars.len());

        let write_char = chars.iter().find(|c| c.uuid == WRITE_UUID).cloned();
        let notify_char = chars.iter().find(|c| c.uuid == NOTIFY_UUID).cloned();

        let w_char = match write_char {
            Some(w) => w,
            None => {
                println!("[BUDS] Write characteristic 0100079A not found on candidate, disconnecting...");
                let _ = peripheral.disconnect().await;
                continue;
            }
        };

        // Listen for notifications
        if let Ok(mut stream) = peripheral.notifications().await {
            let app = app_handle.clone();
            tokio::spawn(async move {
                while let Some(notif) = stream.next().await {
                    let val = notif.value;
                    if let Some(batt) = parse_battery(&val) {
                        println!("[BUDS] Battery update: L:{}% R:{}% Case:{}%", batt.left, batt.right, batt.case);
                        let _ = app.emit_all("battery-update", batt);
                    }
                    if let Some(mode) = parse_anc_notification(&val) {
                        println!("[BUDS] ANC mode update: {}", mode);
                        let _ = app.emit_all("anc-update", ANCPayload { mode });
                    }
                }
            });
        }

        // Subscribe strictly to NOTIFY_UUID 0200079a
        if let Some(ref n) = notify_char {
            println!("[BUDS] Subscribing to NOTIFY_UUID (0200079a)...");
            let sub_res = time::timeout(Duration::from_secs(3), peripheral.subscribe(n)).await;
            println!("[BUDS] Subscribed: {:?}", sub_res);
        }

        // Authenticate with the earbuds
        opo_authenticate(&peripheral, &w_char).await?;

        // Query initial battery status immediately
        println!("[BUDS] Querying initial battery...");
        let _ = peripheral.write(&w_char, &CMD_BATT_QUERY, WriteType::WithoutResponse).await;
        time::sleep(Duration::from_millis(250)).await;

        // Query initial ANC status
        println!("[BUDS] Querying initial ANC status...");
        let _ = peripheral.write(&w_char, &CMD_ANC_QUERY, WriteType::WithoutResponse).await;
        time::sleep(Duration::from_millis(250)).await;

        // Query initial EQ status
        println!("[BUDS] Querying initial EQ status...");
        let _ = peripheral.write(&w_char, &CMD_EQ_QUERY, WriteType::WithoutResponse).await;

        // Start background keepalive loop (pings battery every 25s so the BLE connection never drops or goes idle)
        let p_keep = peripheral.clone();
        let w_keep = w_char.clone();
        tokio::spawn(async move {
            loop {
                time::sleep(Duration::from_secs(25)).await;
                if !p_keep.is_connected().await.unwrap_or(false) {
                    break;
                }
                let _ = p_keep.write(&w_keep, &CMD_BATT_QUERY, WriteType::WithoutResponse).await;
            }
        });

        // Save connection state
        *state.device.lock().await = Some(peripheral);
        *state.write_char.lock().await = Some(w_char);
        *state.authenticated.lock().await = true;

        println!("[BUDS] Ready to control!");
        return Ok("Connected".into());
    }

    if found_names.is_empty() {
        println!("[BUDS] No BLE devices found in scan.");
        Err("No Bluetooth devices found. Ensure earbuds are out of the case.".into())
    } else {
        println!("[BUDS] Nearby devices: {:?}", found_names);
        Err(format!(
            "Found nearby devices ({}), but OnePlus Buds did not respond.",
            found_names.join(", ")
        ))
    }
}

#[tauri::command]
async fn connect_device(
    app_handle: tauri::AppHandle,
    state: State<'_, BluetoothState>,
) -> Result<String, String> {
    {
        let mut connecting = state.is_connecting.lock().await;
        if *connecting {
            println!("[BUDS] Connection attempt already in progress, skipping duplicate call.");
            return Ok("Connecting in progress...".into());
        }
        *connecting = true;
    }

    let result = connect_device_internal(&app_handle, &state).await;
    *state.is_connecting.lock().await = false;
    result
}

#[tauri::command]
async fn send_anc_command(
    mode: String,
    state: State<'_, BluetoothState>,
) -> Result<String, String> {
    let device_guard = state.device.lock().await;
    let char_guard = state.write_char.lock().await;

    if let (Some(device), Some(write_char)) = (device_guard.as_ref(), char_guard.as_ref()) {
        {
            let mut auth = state.authenticated.lock().await;
            if !*auth {
                opo_authenticate(device, write_char).await?;
                *auth = true;
            }
        }

        let payload: &[u8] = match mode.as_str() {
            "anc_on" => &CMD_ANC_ON,
            "anc_trans" => &CMD_ANC_TRANS,
            "anc_off" => &CMD_ANC_OFF,
            "eq_balanced" => &CMD_EQ_BALANCED,
            "eq_bass" => &CMD_EQ_BASS,
            "eq_serenade" => &CMD_EQ_SERENADE,
            "battery" => &CMD_BATT_QUERY,
            _ => return Err(format!("Unknown mode: {mode}")),
        };

        device
            .write(write_char, payload, WriteType::WithoutResponse)
            .await
            .map_err(|e| format!("Write failed: {e}"))?;

        println!("[BUDS] Sent command: {}", mode);
        Ok(format!("Sent {}", mode))
    } else {
        Err("Not connected. Click Reconnect first.".into())
    }
}

#[tauri::command]
async fn quit_app(app_handle: tauri::AppHandle, state: State<'_, BluetoothState>) -> Result<(), ()> {
    println!("[BUDS] Cleanly disconnecting Bluetooth before quitting...");
    let mut dev = state.device.lock().await;
    if let Some(device) = dev.take() {
        let _ = device.disconnect().await;
    }
    *state.write_char.lock().await = None;
    *state.authenticated.lock().await = false;
    time::sleep(Duration::from_millis(200)).await;
    app_handle.exit(0);
    Ok(())
}

fn main() {
    let open = CustomMenuItem::new("open".to_string(), "Open Buds");
    let quit = CustomMenuItem::new("quit".to_string(), "Quit");
    let tray_menu = SystemTrayMenu::new()
        .add_item(open)
        .add_native_item(SystemTrayMenuItem::Separator)
        .add_item(quit);

    let tray = SystemTray::new().with_menu(tray_menu);

    tauri::Builder::default()
        .manage(BluetoothState {
            device: Mutex::new(None),
            write_char: Mutex::new(None),
            authenticated: Mutex::new(false),
            is_connecting: Mutex::new(false),
        })
        .system_tray(tray)
        .on_system_tray_event(|app, event| {
            match event {
                SystemTrayEvent::LeftClick { position, .. } => {
                    if let Some(window) = app.get_window("main") {
                        if window.is_visible().unwrap_or(false) {
                            let _ = window.hide();
                        } else {
                            let win_w = 380.0;
                            let win_h = 470.0;
                            let (mon_w, _mon_h) = if let Ok(Some(mon)) = window.current_monitor() {
                                let size = mon.size();
                                (size.width as f64, size.height as f64)
                            } else {
                                (1920.0, 1080.0)
                            };

                            let mut target_x = position.x - (win_w / 2.0);
                            if target_x + win_w > mon_w - 10.0 {
                                target_x = mon_w - win_w - 10.0;
                            }
                            if target_x < 10.0 {
                                target_x = 10.0;
                            }

                            let mut target_y = position.y - win_h - 10.0;
                            if target_y < 10.0 {
                                target_y = position.y + 25.0;
                            }

                            let _ = window.set_position(tauri::Position::Physical(tauri::PhysicalPosition {
                                x: target_x as i32,
                                y: target_y as i32,
                            }));
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                }
                SystemTrayEvent::MenuItemClick { id, .. } => {
                    match id.as_str() {
                        "open" => {
                            if let Some(window) = app.get_window("main") {
                                let _ = window.show();
                                let _ = window.set_focus();
                            }
                        }
                        "quit" => {
                            let app_handle = app.app_handle();
                            tauri::async_runtime::spawn(async move {
                                let h = app_handle.clone();
                                let state = app_handle.state::<BluetoothState>();
                                let _ = quit_app(h, state).await;
                            });
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        })
        .on_window_event(|event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event.event() {
                // Clicking the close action hides the window to tray instead of quitting
                api.prevent_close();
                let _ = event.window().hide();
            }
        })
        .invoke_handler(tauri::generate_handler![connect_device, send_anc_command, quit_app])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
