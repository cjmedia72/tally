use tauri::{
    image::Image,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    AppHandle, Emitter, LogicalSize, Manager, WebviewWindow, WindowEvent,
};

mod claude;
mod codex;
mod history;
mod plans;
mod pricing;
mod snapshot;

const COLLAPSED: (f64, f64) = (272.0, 122.0);
const EXPANDED: (f64, f64) = (640.0, 510.0);
const APP_ICON_PNG: &[u8] = include_bytes!("../icons/icon.png");

/// Returns a fresh usage snapshot. `refresh_ms` is the user's UI refresh
/// interval — the backend cache TTL is tied to this so each UI poll gets
/// data at most that old. Floor 30s to avoid hammering the API if a user
/// picks an extreme value.
#[tauri::command]
fn get_snapshot(refresh_ms: Option<u64>) -> Result<snapshot::UsageSnapshot, String> {
    let snap = snapshot::build(refresh_ms.unwrap_or(120_000)).map_err(|e| e.to_string())?;
    if let Err(e) = history::record_snapshot(&snap) {
        eprintln!("[tally] usage history write failed: {e}");
    }
    Ok(snap)
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
        .invoke_handler(tauri::generate_handler![
            get_snapshot,
            resize_window,
            set_window_size,
            hide_window,
            quit_app,
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

            // Boot diagnostic — both sources should now be LIVE
            match snapshot::build(120_000) {
                Ok(snap) => {
                    let cl = snap.claude.as_ref()
                        .map(|c| format!("5h={:.0}% wk={:.0}%", c.five_hour.used_percent, c.weekly.used_percent))
                        .unwrap_or_else(|| "not connected".to_string());
                    let cx = snap.codex.as_ref()
                        .map(|c| format!("5h={:.0}% wk={:.0}%", c.five_hour.used_percent, c.weekly.used_percent))
                        .unwrap_or_else(|| "not connected".to_string());
                    eprintln!(
                        "[tally] boot LIVE: claude({}) | codex({}) | roi={:.1}x",
                        cl, cx, snap.roi.leverage
                    );
                }
                Err(e) => eprintln!("[tally] snapshot error: {}", e),
            }

            // System tray
            let show_item = MenuItem::with_id(app, "show", "Show / Hide", true, None::<&str>)?;
            let refresh_item = MenuItem::with_id(app, "refresh", "Refresh now", true, None::<&str>)?;
            let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show_item, &refresh_item, &quit_item])?;
            let icon = app.default_window_icon().expect("icon missing").clone();

            let _tray = TrayIconBuilder::with_id("main-tray")
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
                .build(app)?;

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
