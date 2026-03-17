use tauri::{
    AppHandle, Manager,
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
};
use std::sync::{Arc, Mutex};
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

const API_BASE: &str = "https://app.teamlens.co/api";

#[derive(Clone, serde::Serialize, serde::Deserialize, Debug, PartialEq)]
enum TrackingState { Active, Paused, Stopped }

#[derive(Clone, serde::Serialize, serde::Deserialize, Debug, PartialEq)]
enum TrackingMode { AlwaysOn, UserControlled, AlwaysOff }

#[derive(Clone)]
struct AgentConfig {
    screenshot_interval_secs: u64,
    screenshot_quality: String, // "low" | "medium" | "high"
    screenshots_enabled: bool,
}

#[derive(Clone)]
struct AgentState {
    state: TrackingState,
    mode: TrackingMode,
    agent_id: String,
    active_app: String,
    productivity: u32,
    config: AgentConfig,
    last_screenshot: Instant,
    last_config_fetch: Instant,
}

fn get_hostname() -> String {
    Command::new("hostname").output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}
fn get_username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "user".to_string())
}

fn get_idle_secs() -> u64 {
    #[cfg(target_os = "linux")]
    { Command::new("xprintidle").output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().unwrap_or(0) / 1000)
        .unwrap_or(0) }
    #[cfg(target_os = "macos")]
    { Command::new("ioreg").args(["-c","IOHIDSystem"]).output()
        .map(|o| {
            for line in String::from_utf8_lossy(&o.stdout).lines() {
                if line.contains("HIDIdleTime") {
                    if let Some(v) = line.split('=').last() {
                        return v.trim().parse::<u64>().unwrap_or(0) / 1_000_000_000;
                    }
                }
            }
            0
        }).unwrap_or(0) }
    #[cfg(target_os = "windows")]
    { 0 } // TODO: GetLastInputInfo
    #[cfg(not(any(target_os="linux",target_os="macos",target_os="windows")))]
    { 0 }
}

fn get_active_app() -> String {
    #[cfg(target_os = "linux")]
    { Command::new("xdotool").args(["getactivewindow","getwindowname"]).output()
        .map(|o| { let s=String::from_utf8_lossy(&o.stdout).trim().to_string(); if s.is_empty(){"Desktop".into()} else {s.chars().take(40).collect()} })
        .unwrap_or_else(|_| "Linux".into()) }
    #[cfg(target_os = "macos")]
    { Command::new("sh").args(["-c","lsappinfo front | grep -o 'name=\"[^\"]*\"' | head -1 | cut -d'\"' -f2"]).output()
        .map(|o| { let s=String::from_utf8_lossy(&o.stdout).trim().to_string(); if s.is_empty(){"Finder".into()} else {s} })
        .unwrap_or_else(|_| "macOS".into()) }
    #[cfg(target_os = "windows")]
    { "Windows".into() }
    #[cfg(not(any(target_os="linux",target_os="macos",target_os="windows")))]
    { "Unknown".into() }
}

// Feature 1: Get foreground application name using AppleScript
fn get_foreground_app_name() -> String {
    #[cfg(target_os = "macos")]
    {
        let app = Command::new("osascript")
            .arg("-e")
            .arg("tell application \"System Events\" to get name of first application process whose frontmost is true")
            .output()
            .map(|o| {
                let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if s.is_empty() { "Unknown".to_string() } else { s }
            })
            .unwrap_or_else(|_| "Unknown".to_string());
            
        if app == "Google Chrome" {
            let url = Command::new("osascript").args(["-e", "tell application \"Google Chrome\" to get URL of active tab of front window"]).output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
            if !url.is_empty() { return format!("Chrome: {}", url); }
        } else if app == "Safari" {
            let url = Command::new("osascript").args(["-e", "tell application \"Safari\" to get URL of front document"]).output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
            if !url.is_empty() { return format!("Safari: {}", url); }
        } else if app == "Brave Browser" {
            let url = Command::new("osascript").args(["-e", "tell application \"Brave Browser\" to get URL of active tab of front window"]).output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
            if !url.is_empty() { return format!("Brave: {}", url); }
        }
        app
    }
    #[cfg(not(target_os = "macos"))]
    {
        "Unknown".into()
    }
}

// Feature 2: Native screenshot compression using image crate
fn take_screenshot(quality: &str) -> Option<String> {
    use image::{ImageFormat, GenericImageView};
    use std::io::Cursor;

    let tmp_png = std::env::temp_dir().join("tl_screenshot.png");
    let tmp_png_str = tmp_png.to_string_lossy().to_string();

    #[cfg(target_os = "macos")]
    {
        // Capture as PNG first
        let py_script = format!("import Quartz.CoreGraphics as CG; import Quartz; import CoreFoundation as CF; url=CF.CFURLCreateWithFileSystemPath(None, '{}', CF.kCFURLPOSIXPathStyle, False); dest=Quartz.CGImageDestinationCreateWithURL(url, 'public.png', 1, None); img=CG.CGDisplayCreateImage(CG.CGMainDisplayID()); Quartz.CGImageDestinationAddImage(dest, img, None); Quartz.CGImageDestinationFinalize(dest)", tmp_png_str);
        let _ = Command::new("python3")
            .args(["-c", &py_script])
            .output()
            .ok()?;
    }
    #[cfg(target_os = "linux")]
    {
        // Linux: try scrot
        if Command::new("scrot").args([&tmp_png_str]).output().is_err() {
            let _ = Command::new("import").args(["-window", "root", &tmp_png_str]).output();
        }
    }
    #[cfg(target_os = "windows")]
    {
        return None; // TODO: PowerShell screenshot
    }

    // Read PNG and process with image crate
    let img = image::open(&tmp_png).ok()?;
    let _ = std::fs::remove_file(&tmp_png);

    // Resize based on quality
    let resized = match quality {
        "low" => {
            let (w, h) = img.dimensions();
            if w > 640 {
                img.resize(640, 640 * h / w, image::imageops::FilterType::Lanczos3)
            } else {
                img
            }
        }
        "medium" => {
            let (w, h) = img.dimensions();
            if w > 1920 {
                img.resize(1920, 1920 * h / w, image::imageops::FilterType::Lanczos3)
            } else {
                img
            }
        }
        _ => img, // high quality - no resize
    };

    // Encode as JPEG with quality ~60
    let mut jpeg_bytes = Vec::new();
    let mut cursor = Cursor::new(&mut jpeg_bytes);

    resized.write_to(&mut cursor, ImageFormat::Jpeg).ok()?;

    // Base64 encode
    Some(base64_encode(&jpeg_bytes))
}

// Simple base64 encoder (no external deps)
fn base64_encode(input: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[(n >> 18) as usize] as char);
        out.push(CHARS[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { CHARS[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { CHARS[(n & 63) as usize] as char } else { '=' });
    }
    out
}

fn fetch_agent_config() -> AgentConfig {
    let out = Command::new("curl").args(["-s",&format!("{}/agent/config",API_BASE),"--max-time","8"]).output().ok();
    if let Some(o) = out {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&o.stdout) {
            return AgentConfig {
                screenshot_interval_secs: v["screenshot_interval_secs"].as_u64().unwrap_or(60),
                screenshot_quality: v["screenshot_quality"].as_str().unwrap_or("medium").to_string(),
                screenshots_enabled: v["screenshots_enabled"].as_bool().unwrap_or(true),
            };
        }
    }
    AgentConfig { screenshot_interval_secs: 60, screenshot_quality: "medium".into(), screenshots_enabled: true }
}

// Feature 3: Offline buffer storage
fn get_buffer_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    let dir = std::path::Path::new(&home).join(".teamlens");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("buffer.json")
}

fn save_to_buffer(payload: &str) {
    let path = get_buffer_path();
    let mut items: Vec<String> = if let Ok(data) = std::fs::read_to_string(&path) {
        serde_json::from_str(&data).unwrap_or_else(|_| Vec::new())
    } else {
        Vec::new()
    };
    items.push(payload.to_string());
    if let Ok(json) = serde_json::to_string(&items) {
        let _ = std::fs::write(&path, json);
    }
}

fn flush_buffer() -> bool {
    let path = get_buffer_path();
    let items: Vec<String> = if let Ok(data) = std::fs::read_to_string(&path) {
        serde_json::from_str(&data).unwrap_or_else(|_| Vec::new())
    } else {
        return true;
    };

    if items.is_empty() {
        return true;
    }

    // Try to send each buffered item
    let mut all_success = true;
    let mut remaining = Vec::new();

    for item in items {
        let result = Command::new("curl")
            .args([
                "-s", "-X", "POST", &format!("{}/ingest", API_BASE),
                "-H", "Content-Type: application/json",
                "-d", &item,
                "--max-time", "15",
            ])
            .output();

        if result.is_err() {
            all_success = false;
            remaining.push(item);
        }
    }

    // Save remaining items back to buffer
    if remaining.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else {
        if let Ok(json) = serde_json::to_string(&remaining) {
            let _ = std::fs::write(&path, json);
        }
    }

    all_success
}

fn post_ingest(agent_id: &str, app: &str, app_name: &str, idle: bool, prod: u32, kb: u32, screenshot_b64: Option<String>) {
    // First, try to flush any buffered items
    let _ = flush_buffer();

    let screenshot_field = screenshot_b64.map(|s| format!(r#","screenshot_b64":"{}""#, s)).unwrap_or_default();
    let body = format!(
        r#"{{"agent_id":"{aid}","active_app":"{app}","app_name":"{appname}","window_title":"{app}","keyboard_activity":{kb},"mouse_activity":{kb},"is_idle":{idle},"productivity":{prod}{ss}}}"#,
        aid=agent_id, app=app.replace('"',"'"), appname=app_name.replace('"',"'"), kb=kb, idle=idle, prod=prod, ss=screenshot_field
    );

    let result = Command::new("curl").args([
        "-s","-X","POST",&format!("{}/ingest",API_BASE),
        "-H","Content-Type: application/json","-d",&body,"--max-time","15",
    ]).output();

    // If request failed, save to buffer
    if result.is_err() {
        save_to_buffer(&body);
    }
}

fn post_control(agent_id: &str, action: &str) -> bool {
    let body = format!(r#"{{"agent_id":"{}","action":"{}"}}"#, agent_id, action);
    Command::new("curl").args(["-s","-X","POST",&format!("{}/agent/control",API_BASE),
        "-H","Content-Type: application/json","-d",&body,"--max-time","8",
    ]).output().map(|o| String::from_utf8_lossy(&o.stdout).contains("ok")).unwrap_or(false)
}

fn get_command(agent_id: &str) -> (String, String, bool) {
    let out = Command::new("curl").args(["-s",&format!("{}/agent/command?agent_id={}",API_BASE,agent_id),"--max-time","8"]).output().ok();
    if let Some(o) = out {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&o.stdout) {
            return (
                v["command"].as_str().unwrap_or("none").to_string(),
                v["mode"].as_str().unwrap_or("always_on").to_string(),
                v["paused"].as_bool().unwrap_or(false),
            );
        }
    }
    ("none".into(),"always_on".into(),false)
}

fn rebuild_menu(app: &AppHandle, state: &TrackingState, mode: &TrackingMode, app_name: &str, prod: u32, interval: u64) -> tauri::Result<Menu<tauri::Wry>> {
    let menu = Menu::new(app)?;
    let status_text = match state {
        TrackingState::Active => format!("🟢 Tracking Active — {}%", prod),
        TrackingState::Paused => "🟡 Tracking Paused".to_string(),
        TrackingState::Stopped => "🔴 Tracking Stopped".to_string(),
    };
    menu.append(&MenuItem::with_id(app,"status",&status_text,false,None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app,"app",&format!("   {}",app_name.chars().take(35).collect::<String>()),false,None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app,"interval",&format!("   📸 Screenshots every {}s",interval),false,None::<&str>)?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;

    match mode {
        TrackingMode::AlwaysOn => {
            menu.append(&MenuItem::with_id(app,"managed","🔒 Managed by Admin",false,None::<&str>)?)?;
        }
        TrackingMode::AlwaysOff => {
            menu.append(&MenuItem::with_id(app,"off","⛔ Tracking Disabled by Admin",false,None::<&str>)?)?;
        }
        TrackingMode::UserControlled => {
            match state {
                TrackingState::Active => {
                    menu.append(&MenuItem::with_id(app,"pause","⏸  Pause Tracking",true,None::<&str>)?)?;
                    menu.append(&MenuItem::with_id(app,"stop","⏹  Stop Tracking",true,None::<&str>)?)?;
                }
                TrackingState::Paused => {
                    menu.append(&MenuItem::with_id(app,"resume","▶️  Resume Tracking",true,None::<&str>)?)?;
                    menu.append(&MenuItem::with_id(app,"stop","⏹  Stop Tracking",true,None::<&str>)?)?;
                }
                TrackingState::Stopped => {
                    menu.append(&MenuItem::with_id(app,"start","▶️  Start Tracking",true,None::<&str>)?)?;
                }
            }
        }
    }

    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(app,"dashboard","📊 Open Dashboard",true,None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app,"quit","Quit TeamLens Agent",true,None::<&str>)?)?;
    Ok(menu)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let hostname = get_hostname();
    let username = get_username();
    let agent_id = format!("agent-{}-{}", hostname, username);

    let initial_config = fetch_agent_config();
    let shared_state = Arc::new(Mutex::new(AgentState {
        state: TrackingState::Active,
        mode: TrackingMode::AlwaysOn,
        agent_id: agent_id.clone(),
        active_app: "Starting…".to_string(),
        productivity: 0,
        config: initial_config,
        last_screenshot: Instant::now() - Duration::from_secs(9999),
        last_config_fetch: Instant::now() - Duration::from_secs(9999),
    }));

    let state_for_bg = Arc::clone(&shared_state);
    let agent_id_bg = agent_id.clone();

    tauri::Builder::default()
        .setup(move |app| {
            let menu = rebuild_menu(app.handle(), &TrackingState::Active, &TrackingMode::AlwaysOn, "Starting…", 0, 60)?;
            let _tray = TrayIconBuilder::new()
                .menu(&menu)
                .tooltip("TeamLens Agent — Starting…")
                .on_menu_event(|app, event| {
                    let shared = app.state::<Arc<Mutex<AgentState>>>();
                    let agent_id = { shared.lock().unwrap().agent_id.clone() };
                    match event.id.as_ref() {
                        "pause" => { if post_control(&agent_id,"pause") { shared.lock().unwrap().state = TrackingState::Paused; } }
                        "resume"|"start" => { if post_control(&agent_id,"resume") { shared.lock().unwrap().state = TrackingState::Active; } }
                        "stop" => { if post_control(&agent_id,"stop") { shared.lock().unwrap().state = TrackingState::Stopped; } }
                        "dashboard" => { let _ = open::that("https://app.teamlens.co"); }
                        "quit" => std::process::exit(0),
                        _ => {}
                    }
                })
                .build(app)?;

            app.manage(shared_state);

            let app_handle = app.handle().clone();
            thread::spawn(move || {
                let mut tick = 0u32;
                loop {
                    let now = Instant::now();
                    let idle = get_idle_secs();
                    let active_app = get_active_app();
                    let app_name = get_foreground_app_name();
                    let is_idle = idle > 60;
                    // Hardware-accurate activity tracking based on IOHIDSystem idle time
                    let kb: u32 = if idle <= 5 { 95 } else if idle <= 15 { 75 } else if idle <= 60 { 40 } else if idle <= 120 { 15 } else { 0 };
                    let prod: u32 = if is_idle { 0 } else if kb > 0 { kb } else { 5 };

                    // Poll admin command every 30s (every 3 ticks at 10s)
                    let (cmd, mode_str, paused) = if tick % 3 == 0 { get_command(&agent_id_bg) } else {
                        let s = state_for_bg.lock().unwrap();
                        let m = match s.mode { TrackingMode::AlwaysOn=>"always_on", TrackingMode::UserControlled=>"user_controlled", TrackingMode::AlwaysOff=>"always_off" };
                        let p = matches!(s.state, TrackingState::Paused);
                        ("none".into(), m.into(), p)
                    };

                    // Refresh config from server every 5 minutes
                    let new_config = {
                        let mut s = state_for_bg.lock().unwrap();
                        if now.duration_since(s.last_config_fetch) > Duration::from_secs(300) {
                            s.last_config_fetch = now;
                            true
                        } else { false }
                    };
                    if new_config {
                        let cfg = fetch_agent_config();
                        state_for_bg.lock().unwrap().config = cfg;
                    }

                    // Apply admin command + update state
                    {
                        let mut s = state_for_bg.lock().unwrap();
                        s.mode = match mode_str.as_str() { "user_controlled"=>TrackingMode::UserControlled, "always_off"=>TrackingMode::AlwaysOff, _=>TrackingMode::AlwaysOn };
                        if cmd == "resume" { s.state = TrackingState::Active; }
                        if cmd == "stop" { s.state = TrackingState::Stopped; }
                        if paused && matches!(s.mode, TrackingMode::UserControlled) { s.state = TrackingState::Paused; }
                        s.active_app = active_app.clone();
                        s.productivity = prod;
                    }

                    let (cur_state, cur_mode, cfg, last_ss) = {
                        let s = state_for_bg.lock().unwrap();
                        (s.state.clone(), s.mode.clone(), s.config.clone(), s.last_screenshot)
                    };

                    // Post ingest + screenshot if active
                    if matches!(cur_state, TrackingState::Active) {
                        // Screenshot if enabled and interval elapsed
                        // SMART IDLE CUTOFF: No screenshot if untouched for > 60 seconds
                        let should_screenshot = cfg.screenshots_enabled
                            && idle <= 60 
                            && now.duration_since(last_ss) >= Duration::from_secs(cfg.screenshot_interval_secs);

                        let screenshot = if should_screenshot {
                            let ss = take_screenshot(&cfg.screenshot_quality);
                            if ss.is_some() {
                                state_for_bg.lock().unwrap().last_screenshot = now;
                            }
                            ss
                        } else { None };

                        post_ingest(&agent_id_bg, &active_app, &app_name, is_idle, prod, kb, screenshot);
                    }

                    // Update tray menu every tick
                    let interval = { state_for_bg.lock().unwrap().config.screenshot_interval_secs };
                    if let Some(tray) = app_handle.tray_by_id("teamlens-tray") {
                        let tooltip = match cur_state {
                            TrackingState::Active => format!("TeamLens — Active {}%", prod),
                            TrackingState::Paused => "TeamLens — Paused".to_string(),
                            TrackingState::Stopped => "TeamLens — Stopped".to_string(),
                        };
                        let _ = tray.set_tooltip(Some(&tooltip));
                        if let Ok(menu) = rebuild_menu(&app_handle, &cur_state, &cur_mode, &active_app, prod, interval) {
                            let _ = tray.set_menu(Some(menu));
                        }
                    }

                    tick += 1;
                    thread::sleep(Duration::from_secs(10));
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error running TeamLens tray app");
}
