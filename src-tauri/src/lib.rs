use std::sync::Mutex;

use tauri::{
    image::Image,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, LogicalSize, Manager, WebviewWindow, WindowEvent,
};

mod claude;
mod codex;
mod history;
mod oauth_errors;
mod plans;
mod pricing;
mod snapshot;
mod updater;

const COLLAPSED: (f64, f64) = (272.0, 122.0);
const EXPANDED: (f64, f64) = (640.0, 510.0);
const DEFAULT_REFRESH_MS: u64 = 120_000;
const MIN_REFRESH_MS: u64 = 60_000;
const APP_ICON_PNG: &[u8] = include_bytes!("../icons/icon.png");

struct ShellState {
    tray: Mutex<Option<TrayIcon>>,
}

/// Returns a fresh usage snapshot. `refresh_ms` is the user's UI refresh
/// interval — the backend cache TTL is tied to this so each UI poll gets
/// data at most that old. Floor 60s to avoid hammering the API if legacy
/// localStorage or a bad caller provides an extreme value.
/// Run on a worker thread via `spawn_blocking` so the JSONL walk (currently
/// up to ~900+ files for heavy Claude/Codex users) and any HTTP calls never
/// block the WebView UI thread. Prior sync version caused 5-30s freezes on
/// fresh systems where NTFS cache was cold and Defender real-time scanning
/// was hot — UI thread held captive during the walk meant Windows flagged
/// the window "Not Responding" and dropped click/paint events.
#[tauri::command]
async fn get_snapshot(refresh_ms: Option<u64>) -> Result<snapshot::UsageSnapshot, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let refresh_ms = refresh_ms.unwrap_or(DEFAULT_REFRESH_MS).max(MIN_REFRESH_MS);
        let snap = snapshot::build(refresh_ms).map_err(|e| e.to_string())?;
        if let Err(e) = history::record_snapshot(&snap) {
            eprintln!("[tally] usage history write failed: {e}");
        }
        Ok::<snapshot::UsageSnapshot, String>(snap)
    })
    .await
    .map_err(|e| format!("get_snapshot worker join failed: {e}"))?
}

#[tauri::command]
fn resize_window(window: WebviewWindow, expanded: bool) -> Result<(), String> {
    let (w, h) = if expanded { EXPANDED } else { COLLAPSED };
    let _ = window.set_resizable(true);
    window
        .set_size(LogicalSize::new(w, h))
        .map_err(|e| e.to_string())?;
    let _ = window.set_resizable(false);
    pin_top_right(&window).map_err(|e| e.to_string())?;
    Ok(())
}

/// Set window to a specific size (used for content-fit auto-sizing).
#[tauri::command]
fn set_window_size(window: WebviewWindow, width: f64, height: f64) -> Result<(), String> {
    let _ = window.set_resizable(true);
    window
        .set_size(LogicalSize::new(width.max(240.0), height.max(110.0)))
        .map_err(|e| e.to_string())?;
    let _ = window.set_resizable(false);
    pin_top_right(&window).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn hide_window(window: WebviewWindow) -> Result<(), String> {
    window.hide().map_err(|e| e.to_string())
}

#[tauri::command]
fn quit_app(app: AppHandle) {
    app.exit(0);
}

#[tauri::command]
fn check_for_update() -> Result<updater::UpdateInfo, String> {
    updater::check(env!("CARGO_PKG_VERSION")).map_err(|e| e.to_string())
}

#[tauri::command]
fn open_update_url(url: String) -> Result<(), String> {
    let allowed = [
        "https://github.com/EcomCJ/Tally/releases/",
        "https://github.com/cjmedia72/tally/releases/",
    ];
    if !allowed.iter().any(|prefix| url.starts_with(prefix)) {
        return Err("Update URL is outside the Tally GitHub releases page.".to_string());
    }

    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        std::process::Command::new("cmd")
            .args(["/C", "start", "", &url])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| e.to_string())?;
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(&url)
            .spawn()
            .map_err(|e| e.to_string())?;
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(&url)
            .spawn()
            .map_err(|e| e.to_string())?;
    }

    Ok(())
}

#[tauri::command]
fn set_shell_visibility(
    app: AppHandle,
    window: WebviewWindow,
    show_tray: bool,
    show_taskbar: bool,
) -> Result<(), String> {
    if !show_tray && !show_taskbar {
        return Err("At least one app surface must stay enabled.".to_string());
    }

    window
        .set_skip_taskbar(!show_taskbar)
        .map_err(|e| e.to_string())?;

    let state = app.state::<ShellState>();
    let mut tray = state.tray.lock().map_err(|e| e.to_string())?;
    match (show_tray, tray.as_ref()) {
        (true, None) => {
            *tray = Some(build_tray(&app).map_err(|e| e.to_string())?);
        }
        (false, Some(existing)) => {
            existing.set_visible(false).map_err(|e| e.to_string())?;
        }
        (true, Some(existing)) => {
            existing.set_visible(true).map_err(|e| e.to_string())?;
        }
        (false, None) => {}
    }
    Ok(())
}

fn pin_top_right(window: &WebviewWindow) -> tauri::Result<()> {
    let monitor = match window.current_monitor()? {
        Some(m) => m,
        None => match window.primary_monitor()? {
            Some(m) => m,
            None => return Ok(()),
        },
    };
    let scale = monitor.scale_factor();
    let mon_size = monitor.size();
    let mon_pos = monitor.position();
    let win_size = window.outer_size()?;
    let margin = (24.0 * scale) as i32;
    let x = mon_pos.x + mon_size.width as i32 - win_size.width as i32 - margin;
    let y = mon_pos.y + margin;
    window.set_position(tauri::PhysicalPosition::new(x, y))?;
    Ok(())
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(ShellState {
            tray: Mutex::new(None),
        })
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            resize_window,
            set_window_size,
            hide_window,
            quit_app,
            check_for_update,
            open_update_url,
            set_shell_visibility,
        ])
        .setup(|app| {
            let window = app.get_webview_window("main").expect("main window missing");
            if let Ok(icon) = Image::from_bytes(APP_ICON_PNG) {
                let _ = window.set_icon(icon);
            }

            // Force frameless + correct size at runtime (config alone is sometimes ignored on Win11).
            let _ = window.set_decorations(false);
            let _ = window.set_resizable(false); // user-resize off, kills DWM resize border
            let _ = window.set_size(LogicalSize::new(COLLAPSED.0, COLLAPSED.1));
            let _ = window.set_always_on_top(true);
            let _ = window.set_skip_taskbar(true);
            let _ = pin_top_right(&window);
            let _ = window.show();

            let tray = build_tray(app.handle())?;
            *app.state::<ShellState>().tray.lock().unwrap() = Some(tray);

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                let _ = window.hide();
                api.prevent_close();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

fn build_tray(app: &AppHandle) -> tauri::Result<TrayIcon> {
    let show_item = MenuItem::with_id(app, "show", "Show / Hide", true, None::<&str>)?;
    let refresh_item = MenuItem::with_id(app, "refresh", "Refresh now", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_item, &refresh_item, &quit_item])?;
    let icon = app.default_window_icon().expect("icon missing").clone();

    TrayIconBuilder::with_id("main-tray")
        .menu(&menu)
        .icon(icon)
        .tooltip("TALLY - Ai Usage Monitor")
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => toggle_main(app),
            "refresh" => {
                let _ = app.emit("refresh-now", ());
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                toggle_main(tray.app_handle());
            }
        })
        .build(app)
}

fn toggle_main(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let visible = win.is_visible().unwrap_or(false);
        if visible {
            let _ = win.hide();
        } else {
            let _ = pin_top_right(&win);
            let _ = win.show();
            let _ = win.set_focus();
        }
    }
}
