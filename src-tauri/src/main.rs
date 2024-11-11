// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::env;
use std::ffi::c_void;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::mem::size_of;
use std::os::windows::prelude::{AsRawHandle, RawHandle};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
extern crate winreg;
use lazy_static::lazy_static;
use once_cell::sync::OnceCell;
use serde::Serialize;
use std::sync::Mutex;
use systray_menu::CONNECT_MENU_ITEMS;
use tauri::{Manager, SystemTray, SystemTrayEvent, Window};
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_window_state::{AppHandleExt, StateFlags};
use windows::{
    core::PCSTR,
    Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WIN32_ERROR},
    Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectA, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    },
    Win32::System::Threading::GetCurrentProcessId,
};
use winreg::enums::*;
use winreg::RegKey;

mod systray_menu;

#[derive(Clone, serde::Serialize)]
struct Payload {
    args: Vec<String>,
    cwd: String,
}

#[derive(Debug)]
struct ChildProcessTracker {
    job_handle: HANDLE,
}

impl ChildProcessTracker {
    fn new() -> Result<Self, WIN32_ERROR> {
        let job_name = format!("ChildProcessTracker{}\0", unsafe { GetCurrentProcessId() });
        let job_handle = match unsafe {
            CreateJobObjectA(None, PCSTR::from_raw(job_name.as_bytes().as_ptr()))
        } {
            Ok(handle) => handle,
            Err(_) => return Err(unsafe { GetLastError() }),
        };

        let job_object_info = JOBOBJECT_BASIC_LIMIT_INFORMATION {
            LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            ..Default::default()
        };

        let job_object_ext_info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: job_object_info,
            ..Default::default()
        };

        let result = unsafe {
            SetInformationJobObject(
                job_handle,
                JobObjectExtendedLimitInformation,
                &job_object_ext_info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION
                    as *const c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };

        if result.as_bool() {
            Ok(Self { job_handle })
        } else {
            unsafe { CloseHandle(job_handle) };
            Err(unsafe { GetLastError() })
        }
    }

    pub fn add_process(&self, process_handle: RawHandle) -> Result<(), WIN32_ERROR> {
        if process_handle.is_null() {
            return Err(unsafe { GetLastError() });
        }

        let result = unsafe {
            AssignProcessToJobObject(
                self.job_handle,
                HANDLE(process_handle as *const c_void as isize),
            )
        };

        if result.as_bool() {
            Ok(())
        } else {
            Err(unsafe { GetLastError() })
        }
    }

    pub fn global() -> Option<&'static ChildProcessTracker> {
        CHILD_PROCESS_TRACKER.get()
    }
}

impl Drop for ChildProcessTracker {
    fn drop(&mut self) {
        if self.job_handle != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.job_handle) };
        }
    }
}

static CHILD_PROCESS_TRACKER: OnceCell<ChildProcessTracker> = OnceCell::new();

struct WiresockEnablingGuard;

impl Drop for WiresockEnablingGuard {
    fn drop(&mut self) {
        let mut state = WIRESOCK_STATE.lock().unwrap();
        state.wiresock_status = "STOPPED".to_string();
        state.tunnel_status = "DISCONNECTED".to_string();
    }
}

#[derive(Clone, Serialize, Debug)]
struct WiresockState {
    tunnel_id: String,
    wiresock_status: String,
    tunnel_status: String,
    logs: Vec<String>,
}

impl WiresockState {
    fn new() -> Self {
        WiresockState {
            tunnel_id: String::new(),
            wiresock_status: "STOPPED".to_string(),
            tunnel_status: "DISCONNECTED".to_string(),
            logs: Vec::new(),
        }
    }
}

lazy_static! {
    static ref WIRESOCK_STATE: Mutex<WiresockState> = Mutex::new(WiresockState::new());
}

mod tunnel;
use tunnel::Tunnel;
#[tauri::command]
async fn enable_wiresock(
    tunnel: Tunnel,
    log_level: String,
    app_handle: tauri::AppHandle,
) -> Result<(), String> {
    // Check if enable_wiresock is already running
    {
        let state = WIRESOCK_STATE.lock().unwrap();
        if state.wiresock_status != "STOPPED" {
            println!(
                "wiresock_state at start of enable_wiresock is {:?}",
                &*state
            );
            return Err("enable_wiresock is already running".into());
        }
    }

    // Update the WIRESOCK_STATE and emit the change
    update_state(&app_handle, |state| {
        state.tunnel_id = tunnel.id.clone();
        state.wiresock_status = "STARTING".to_string();
        state.tunnel_status = "DISCONNECTED".to_string();
        state.logs = Vec::new();
    });

    // Create a guard that will reset the wiresock_status when dropped
    let _guard = WiresockEnablingGuard;

    // Get the users home directory
    let mut tunnel_config_path = PathBuf::new();
    match home::home_dir() {
        Some(path) => tunnel_config_path.push(path),
        None => return Err("Unable to retrieve the user home directory.".into()),
    }

    // Create a path to the wiresock config directory
    tunnel_config_path.push("AppData");
    tunnel_config_path.push("Local");
    tunnel_config_path.push("TunnlTo");

    // Create a TunnlTo directory in appdata/local if it doesn't exist already
    fs::create_dir_all(&tunnel_config_path).unwrap_or_else(|e| panic!("Error creating dir: {}", e));

    // Create a path to the wiresock config file
    tunnel_config_path.push("tunnel.conf");

    // Write the config file to disk
    let mut w = fs::File::create(&tunnel_config_path).unwrap();

    // Interface section
    writeln!(&mut w, "[Interface]").unwrap();

    // Interface Private Key
    writeln!(&mut w, "PrivateKey = {}", tunnel.interface.privateKey).unwrap();

    // Interface addresses
    let mut interface_addresses = String::new();

    if !tunnel.interface.ipv4Address.is_empty() {
        interface_addresses = format!("Address = {}", tunnel.interface.ipv4Address);
    }

    if !tunnel.interface.ipv6Address.is_empty() {
        if !interface_addresses.is_empty() {
            // Ensure there is comma between the ipv4 and ipv6 addresses
            interface_addresses =
                format!("{}, {}", interface_addresses, tunnel.interface.ipv6Address);
        } else {
            // There is no ipv4 address set, but a ipv6 address is set
            interface_addresses = format!("Address = {}", tunnel.interface.ipv6Address);
        }
    }

    // Write the interface addresses to the config file
    if !interface_addresses.is_empty() {
        writeln!(&mut w, "{}", interface_addresses).unwrap();
    }

    // Interface ListenPort
    if !tunnel.interface.port.is_empty() {
        writeln!(&mut w, "ListenPort = {}", tunnel.interface.port).unwrap();
    }

    // Interface DNS
    if !tunnel.interface.dns.is_empty() {
        writeln!(&mut w, "DNS = {}", tunnel.interface.dns).unwrap();
    }

    // Interface MTU
    if !tunnel.interface.mtu.is_empty() {
        writeln!(&mut w, "MTU = {}", tunnel.interface.mtu).unwrap();
    }

    // Put a space between the interface and peer sections for readability
    writeln!(&mut w, "").unwrap();

    // Peer section
    writeln!(&mut w, "[Peer]").unwrap();

    // Peer Public Key
    writeln!(&mut w, "PublicKey = {}", tunnel.peer.publicKey).unwrap();

    // Peer Preshared Key
    if !tunnel.peer.presharedKey.is_empty() {
        writeln!(&mut w, "PresharedKey = {}", tunnel.peer.presharedKey).unwrap();
    }

    // Peer Endpoint
    writeln!(
        &mut w,
        "Endpoint = {}:{}",
        tunnel.peer.endpoint, tunnel.peer.port
    )
    .unwrap();

    // Peer Persistent Keep-alive
    if !tunnel.peer.persistentKeepalive.is_empty() {
        writeln!(
            &mut w,
            "PersistentKeepalive = {}",
            tunnel.peer.persistentKeepalive
        )
        .unwrap();
    }

    // Rules

    // Allowed

    // Allowed Apps
    let mut allowed_apps = String::new();
    if !tunnel.rules.allowed.apps.is_empty() {
        allowed_apps = format!("AllowedApps = {}", tunnel.rules.allowed.apps);
    }

    // Allowed Folders
    if !tunnel.rules.allowed.folders.is_empty() {
        if !allowed_apps.is_empty() {
            // Ensure there is comma between the allowed apps and allowed folders
            allowed_apps = format!("{}, {}", allowed_apps, tunnel.rules.allowed.folders);
        } else {
            allowed_apps = format!("AllowedApps = {}", tunnel.rules.allowed.folders);
        }
    }

    // Write Allowed Apps/Folders to the config file
    if !allowed_apps.is_empty() {
        writeln!(&mut w, "{}", allowed_apps).unwrap();
    }

    // Allowed IP Addresses
    if !tunnel.rules.allowed.ipAddresses.is_empty() {
        writeln!(&mut w, "AllowedIPs = {}", tunnel.rules.allowed.ipAddresses).unwrap();
    }

    // Disallowed

    // Disallowed Apps
    let mut disallowed_apps = String::new();
    if !tunnel.rules.disallowed.apps.is_empty() {
        disallowed_apps = format!("DisallowedApps = {}", tunnel.rules.disallowed.apps);
    }

    // Disallowed Folders
    if !tunnel.rules.disallowed.folders.is_empty() {
        if !disallowed_apps.is_empty() {
            // Ensure there is comma between the disallowed apps and disallowed folders
            disallowed_apps = format!("{}, {}", disallowed_apps, tunnel.rules.disallowed.folders);
        } else {
            disallowed_apps = format!("DisallowedApps = {}", tunnel.rules.disallowed.folders);
        }
    }

    // Write Disallowed Apps/Folders to the config file
    if !disallowed_apps.is_empty() {
        writeln!(&mut w, "{}", disallowed_apps).unwrap();
    }

    // Disallowed IP Addresses
    if !tunnel.rules.disallowed.ipAddresses.is_empty() {
        writeln!(
            &mut w,
            "DisallowedIPs = {}",
            tunnel.rules.disallowed.ipAddresses
        )
        .unwrap();
    }

    // Build the full path to the wiresock executable
    let mut wiresock_location: String = get_wiresock_install_path().unwrap(); // unwrapping as we expect Wiresock is installed at this point
    let exe: &str = "/bin/wiresock-client.exe";
    wiresock_location.push_str(exe);

    // Create a string of the WireSock config file path
    let wiresock_config_path = &tunnel_config_path.into_os_string().into_string().unwrap();

    // Enable Wiresock and output the stdout
    let mut child = Command::new(wiresock_location)
        .arg("run")
        .arg("-config")
        .arg(wiresock_config_path)
        .arg("-log-level")
        .arg(log_level)
        .creation_flags(0x08000000) // CREATE_NO_WINDOW - stop a command window showing
        .stdout(Stdio::piped())
        .spawn()
        .expect("Unable to start WireSock process");

    // Update the WIRESOCK_STATE and emit the change
    update_state(&app_handle, |state| {
        state.wiresock_status = "RUNNING".to_string();
    });

    // Add process to global Job object
    // This ensures if TunnlTo process finishes, wiresock will also exit
    if let Some(tracker) = ChildProcessTracker::global() {
        tracker.add_process(child.as_raw_handle()).ok();
    }

    // Process the stdout data
    if let Some(stdout) = &mut child.stdout.take() {
        let reader = BufReader::new(stdout);

        for line in reader.lines() {
            let line_string = line.unwrap();

            if line_string.is_empty() {
                continue;
            }

            if cfg!(debug_assertions) {
                println!("wiresock_log: {}", line_string);
            }

            // Update the WIRESOCK_STATE and emit the change
            update_state(&app_handle, |state| {
                if line_string.contains("Tunnel has started") {
                    state.tunnel_status = "CONNECTED".to_string();
                }

                // Lock the mutex to safely access LOG_LIMIT
                let log_limit = LOG_LIMIT.lock().unwrap();

                // Check if the logs array has reached the maximum limit
                if state.logs.len() >= (*log_limit).try_into().unwrap() {
                    // Remove the oldest log
                    state.logs.remove(0);
                }

                // Append the log data to the state
                state.logs.push(line_string.clone());
            });
        }
    }

    // Handle the wiresock process stopping
    match child.wait() {
        Ok(status) => {
            println!("wiresock process exited with: {}", status);

            // Update the WIRESOCK_STATE and emit the change
            update_state(&app_handle, |state| {
                state.wiresock_status = "STOPPED".to_string();
                state.tunnel_status = "DISCONNECTED".to_string();
                state
                    .logs
                    .push("Tunnel Disabled. Wiresock process stopped".into())
            });
        }
        Err(e) => println!("error attempting to wait: {}", e),
    }

    println!("End of enable_wiresock function");
    Ok(())
}

#[tauri::command]
fn change_icon(app_handle: tauri::AppHandle, enabled: bool) {
    if enabled {
        app_handle
            .tray_handle()
            .set_icon(tauri::Icon::Raw(
                include_bytes!("assets/icons/icon-enabled.ico").to_vec(),
            ))
            .unwrap();
    } else {
        app_handle
            .tray_handle()
            .set_icon(tauri::Icon::Raw(
                include_bytes!("assets/icons/icon-default.ico").to_vec(),
            ))
            .unwrap();
    }
}

#[tauri::command]
fn change_systray_tooltip(app_handle: tauri::AppHandle, tooltip: String) {
    let _ = app_handle.tray_handle().set_tooltip(&tooltip);
}

#[tauri::command]
fn add_or_update_systray_menu_item(
    app_handle: tauri::AppHandle,
    item_id: String,
    item_label: String,
) {
    systray_menu::add_or_update_systray_menu_item(&app_handle, item_id, item_label);
}

#[tauri::command]
fn update_systray_connect_menu_items(app_handle: tauri::AppHandle, items: Vec<(String, String)>) {
    systray_menu::update_systray_connect_menu_items(&app_handle, items);
}

#[tauri::command]
fn remove_systray_menu_item(app_handle: tauri::AppHandle, item_id: String) {
    systray_menu::remove_systray_menu_item(&app_handle, item_id);
}

fn update_state<F>(app_handle: &tauri::AppHandle, update: F)
where
    F: FnOnce(&mut WiresockState),
{
    let mut state = WIRESOCK_STATE.lock().unwrap();
    update(&mut state);
    app_handle.emit_all("wiresock_state", &*state).unwrap();
}

#[tauri::command]
fn get_wiresock_state(app_handle: tauri::AppHandle) -> Result<(), String> {
    let state = WIRESOCK_STATE.lock().unwrap();
    app_handle.emit_all("wiresock_state", &*state).unwrap();
    Ok(())
}

fn get_wiresock_install_path() -> Result<String, String> {
    // Get the Wiresock install location from the Windows registry
    let hklm = RegKey::predef(HKEY_LOCAL_MACHINE);

    let subkey = match hklm.open_subkey_with_flags(
        r#"SOFTWARE\NTKernelResources\WinpkFilterForVPNClient"#,
        KEY_READ,
    ) {
        Ok(regkey) => regkey,
        Err(_err) => return Err("WIRESOCK_NOT_INSTALLED".to_string()),
    };

    let wiresock_location: String = subkey
        .get_value("InstallLocation")
        .expect("Failed to read registry key");

    Ok(wiresock_location)
}

#[tauri::command]
fn get_wiresock_version() -> Result<String, String> {
    let uninstall_keys = RegKey::predef(HKEY_LOCAL_MACHINE)
        .open_subkey("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Uninstall")
        .map_err(|e| e.to_string())?;

    for name in uninstall_keys.enum_keys().map(|x| x.unwrap()) {
        let subkey = uninstall_keys
            .open_subkey(&name)
            .map_err(|e| e.to_string())?;
        match subkey.get_value::<String, _>("DisplayName") {
            Ok(display_name) => {
                if display_name.starts_with("WireSock VPN Client") {
                    match subkey.get_value::<String, _>("DisplayVersion") {
                        Ok(version) => {
                            println!("Installed WireSock Version: {}", version);
                            return Ok(version);
                        }
                        Err(e) => eprintln!("Error getting display version: {:?}", e),
                    }
                }
            }
            Err(_) => (),
        }
    }

    Ok("wiresock_not_installed".into())
}

#[tauri::command]
async fn disable_wiresock() -> Result<(), String> {
    println!("Attempting to kill the WireSock process");
    let status = Command::new("taskkill")
        .arg("/F")
        .arg("/IM")
        .arg("wiresock-client.exe")
        .arg("/T")
        .creation_flags(0x08000000) // CREATE_NO_WINDOW - stop a command window showing
        .status();

    match status {
        Ok(exit_status) => match exit_status.code() {
            Some(0) | Some(128) => {
                println!("WireSock successfully stopped or was not running.");
                Ok(())
            }
            Some(error_code) => {
                println!("Failed to stop WireSock, exit code: {}", error_code);
                Err(format!(
                    "Failed to stop WireSock, exit code: {}",
                    error_code
                ))
            }
            None => {
                println!("Failed to stop WireSock, no exit code available.");
                Err("Failed to stop WireSock, no exit code available.".to_string())
            }
        },
        Err(e) => {
            println!("Failed to execute taskkill command: {}", e);
            Err(format!("Failed to execute taskkill command: {}", e))
        }
    }
}

#[tauri::command]
async fn install_wiresock() -> Result<String, String> {
    // Get the current directory
    let current_dir = env::current_dir().unwrap();

    // Build the path to the WireSock installer
    let wiresock_installer_path = &mut current_dir.into_os_string().into_string().unwrap();
    wiresock_installer_path.push_str(r#"\wiresock\wiresock-vpn-client-x64-1.4.7.1.msi"#);

    // Use powershell to launch msiexec so we can get the exit code to see if WireSock was installed succesfully
    let arg = format!("(Start-Process -FilePath \"msiexec.exe\" -ArgumentList \"/i\", '\"{}\"', \"/qr\" -Wait -Passthru).ExitCode", wiresock_installer_path);

    // Start the WireSock installer in quiet mode (no user prompts).
    let child = Command::new("powershell")
        .arg("-command")
        .arg(arg)
        .creation_flags(0x08000000) // CREATE_NO_WINDOW - stop a command window showing
        .stdout(Stdio::piped())
        .spawn();

    let mut child = match child {
        Ok(child) => child,
        Err(e) => return Err(format!("Failed to start powershell: {}", e)),
    };

    // Check the stdout data
    let mut output_lines = Vec::new();
    if let Some(stdout) = &mut child.stdout {
        let lines = BufReader::new(stdout).lines();
        for line in lines {
            if let Ok(line) = line {
                output_lines.push(line);
            }
        }
    }

    // Convert the vector of lines to a JSON string
    let output_json = serde_json::to_string(&output_lines).unwrap_or_else(|_| "[]".to_string());

    Ok(output_json)
}

#[tauri::command]
async fn show_app(window: Window) {
    // Show main window
    println!("Showing the main window");
    window
        .get_window("main")
        .expect("no window labeled 'main' found")
        .show()
        .unwrap();
}

lazy_static! {
    static ref MINIMIZE_TO_TRAY: Mutex<bool> = Mutex::new(true);
}

#[tauri::command]
fn set_minimize_to_tray(value: bool) {
    let mut minimize = MINIMIZE_TO_TRAY.lock().unwrap();
    *minimize = value;
}

lazy_static! {
    static ref LOG_LIMIT: Mutex<i32> = Mutex::new(50);
}

#[tauri::command]
fn set_log_limit(value: String) {
    let mut loglim = LOG_LIMIT.lock().unwrap();
    *loglim = value.parse::<i32>().unwrap();
}

async fn connect_to_tunnel(app_handle: &tauri::AppHandle, tunnel_id: &str) {
    println!("Connect systray menu selected tunnel id: {}", tunnel_id);

    // Disable any existing tunnels
    match disable_wiresock().await {
        Ok(()) => {
            // Send the message up to the frontend to handle the tunnel enable. This is because
            // we need the tunnel data which is stored in the frontend for now.
            app_handle
                .emit_all("systray_connect_menu_clicked", tunnel_id)
                .unwrap();
        }
        Err(e) => {
            // The disable_wiresock command failed, handle the error
            eprintln!("Failed to disable WireSock: {}", e);
            // ... your error handling code
        }
    }
}

fn main() {
    // Initialize global job object
    if let Ok(child_process_tracker) = ChildProcessTracker::new() {
        CHILD_PROCESS_TRACKER.set(child_process_tracker).ok();
    }

    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![
            enable_wiresock,
            disable_wiresock,
            install_wiresock,
            get_wiresock_state,
            get_wiresock_version,
            show_app,
            set_minimize_to_tray,
            set_log_limit,
            change_icon,
            change_systray_tooltip,
            add_or_update_systray_menu_item,
            update_systray_connect_menu_items,
            remove_systray_menu_item,
        ])
        .system_tray(SystemTray::new().with_tooltip("TunnlTo: Disconnected"))
        .on_system_tray_event(|app, event| match event {
            SystemTrayEvent::LeftClick {
                position: _,
                size: _,
                ..
            } => {
                if let Some(window) = app.get_window("main") {
                    window.show().unwrap();
                    window.unminimize().unwrap();
                    match window.set_focus() {
                        Ok(_) => println!("Window focus set successfully."),
                        Err(e) => println!("Failed to set window focus: {:?}", e),
                    }
                };
            }
            SystemTrayEvent::MenuItemClick { id, .. } => {
                let connect_menu_items = CONNECT_MENU_ITEMS.lock().unwrap();
                match id.as_str() {
                    "minimize" => {
                        // Hide the app window
                        if let Some(window) = app.get_window("main") {
                            window.hide().unwrap();
                        };
                    }
                    "exit" => {
                        // Minimize the app window
                        if let Some(window) = app.get_window("main") {
                            window.hide().unwrap();
                        };

                        // Save window state to disk
                        let _ = app.save_window_state(StateFlags::all());

                        // Exit the app
                        app.exit(0);
                    }
                    "disconnect" => {
                        // Disable WireSock
                        tauri::async_runtime::spawn(async move {
                            let _ = disable_wiresock().await;
                        });
                    }
                    _ => {
                        // Handle clicks on 'Connect' submenu items
                        if connect_menu_items.contains(&id.to_string()) {
                            let app_handle = app.clone();
                            let id_clone = id.clone();
                            tauri::async_runtime::spawn(async move {
                                connect_to_tunnel(&app_handle, &id_clone).await;
                            });
                        }
                    }
                }
            }
            _ => {}
        })
        .plugin(tauri_plugin_window_state::Builder::default().build())
        .plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
            println!("{}, {argv:?}, {cwd}", app.package_info().name);

            app.emit_all("single-instance", Payload { args: argv, cwd })
                .unwrap();
        }))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--flag1", "--flag2"]), /* arbitrary number of args to pass to your app */
        ))
        .build(tauri::generate_context!())
        .expect("error while running tauri application")
        .run(|app, event| match event {
            tauri::RunEvent::WindowEvent {
                label,
                event: win_event,
                ..
            } => match win_event {
                tauri::WindowEvent::CloseRequested { api, .. } => {
                    let minimize_to_tray = MINIMIZE_TO_TRAY.lock().unwrap();
                    if *minimize_to_tray {
                        let window = app.get_window(label.as_str()).unwrap();
                        window.hide().unwrap();
                        api.prevent_close();
                    }
                }
                _ => {}
            },
            _ => {}
        })
}
