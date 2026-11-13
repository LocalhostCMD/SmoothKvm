//! The SmoothKvm app shell (Windows): a tray icon that is always there, and a native window
//! that exists only while you have the panel open (so the idle app uses almost no memory).
use crate::{app, capture_win, daemon, store};
use std::io;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::{Icon as WinIcon, Window, WindowBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon as TrayIcon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use wry::{NewWindowResponse, WebContext, WebView, WebViewBuilder};

enum UserEvent {
    Show,
    Quit,
    Menu(MenuEvent),
    Tray(TrayIconEvent),
}

static PROXY: OnceLock<Mutex<EventLoopProxy<UserEvent>>> = OnceLock::new();

fn post(ev: UserEvent) {
    if let Some(p) = PROXY.get() {
        if let Ok(p) = p.lock() {
            let _ = p.send_event(ev);
        }
    }
}

/// Ask the running app to bring its window to the front (called by the panel server).
pub fn show_window() {
    post(UserEvent::Show);
}

pub fn quit_app() {
    post(UserEvent::Quit);
}

/// A round app icon, blue normally and red while the emergency stop is active.
fn icon_rgba(paused: bool) -> Vec<u8> {
    let (r, g, b) = if paused { (0xE0u8, 0x3A, 0x2F) } else { (0x3B, 0x6D, 0xF0) };
    let mut px = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32 {
        for x in 0..32 {
            let (dx, dy) = (x as f32 - 15.5, y as f32 - 15.5);
            let d = (dx * dx + dy * dy).sqrt();
            if d < 5.0 {
                px.extend_from_slice(&[255, 255, 255, 255]);
            } else if d < 14.5 {
                px.extend_from_slice(&[r, g, b, 255]);
            } else if d < 15.5 {
                px.extend_from_slice(&[r, g, b, ((15.5 - d) * 255.0) as u8]);
            } else {
                px.extend_from_slice(&[0, 0, 0, 0]);
            }
        }
    }
    px
}

fn tray_icon(paused: bool) -> TrayIcon {
    TrayIcon::from_rgba(icon_rgba(paused), 32, 32).expect("icon")
}

fn open_window(target: &tao::event_loop::EventLoopWindowTarget<UserEvent>, ctx: &mut WebContext) -> io::Result<(Window, WebView)> {
    let err = |e: String| io::Error::new(io::ErrorKind::Other, e);
    let window = WindowBuilder::new()
        .with_title("SmoothKvm")
        .with_inner_size(LogicalSize::new(980.0, 800.0))
        .with_min_inner_size(LogicalSize::new(560.0, 520.0))
        .with_window_icon(WinIcon::from_rgba(icon_rgba(false), 32, 32).ok())
        .build(target)
        .map_err(|e| err(e.to_string()))?;
    let url = app::panel_url()?;
    let webview = WebViewBuilder::new_with_web_context(ctx)
        .with_url(&url)
        // The window only ever shows the local panel.
        .with_navigation_handler(|u| u.starts_with(&format!("http://127.0.0.1:{}", crate::httpd::port())))
        .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
        // Files dropped on the window are sent to the other computer (never opened in the webview).
        .with_drag_drop_handler(|e| {
            if let wry::DragDropEvent::Drop { paths, .. } = e {
                if !paths.is_empty() {
                    match store::Identity::load_or_create().map(std::sync::Arc::new) {
                        Ok(ident) => match crate::xfer::send_async(None, paths, ident) {
                            Ok(t) => crate::shared::info(&format!("Sending dropped files to {}", t.name)),
                            Err(e) => crate::shared::warn(&format!("Could not send the dropped files: {e}")),
                        },
                        Err(e) => crate::shared::warn(&format!("Could not send the dropped files: {e}")),
                    }
                }
            }
            true
        })
        .build(&window)
        .map_err(|e| err(e.to_string()))?;
    Ok((window, webview))
}

pub fn run(_show: bool) -> io::Result<()> {
    daemon::start_backend()?;
    crate::shared::info(&format!("SmoothKvm {} started. It runs only while this window/tray icon is open; closing the window quits it.", crate::VERSION));
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let _ = PROXY.set(Mutex::new(event_loop.create_proxy()));
    {
        let p = event_loop.create_proxy();
        MenuEvent::set_event_handler(Some(move |e| {
            let _ = p.send_event(UserEvent::Menu(e));
        }));
        let p = event_loop.create_proxy();
        TrayIconEvent::set_event_handler(Some(move |e| {
            let _ = p.send_event(UserEvent::Tray(e));
        }));
    }
    let open_item = MenuItem::new("Open SmoothKvm", true, None);
    let stop_item = MenuItem::new("Emergency stop", true, None);
    let update_item = MenuItem::new("Check for updates", true, None);
    let quit_item = MenuItem::new("Quit SmoothKvm", true, None);
    let menu = Menu::new();
    menu.append_items(&[&open_item, &stop_item, &update_item, &PredefinedMenuItem::separator(), &quit_item])
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("SmoothKvm")
        .with_icon(tray_icon(false))
        .build()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;

    let mut ctx = WebContext::new(Some(store::config_dir().join("webview")));
    let mut window: Option<(Window, WebView)> = None;
    let mut shown_paused = false;
    let mut shown_alerts = 0usize;
    let mut shown_update: Option<String> = None;
    // Always open the window: the app is never running invisibly.
    post(UserEvent::Show);
    event_loop.run(move |event, target, flow| {
        *flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(500));
        match event {
            // Keep the tray in sync (the triple-press shortcut can change the state too).
            Event::NewEvents(StartCause::ResumeTimeReached { .. }) => {
                let upd = crate::update::available();
                if upd != shown_update {
                    update_item.set_text(match &upd {
                        Some(v) => format!("Update to SmoothKvm {v}"),
                        None => "Check for updates".to_string(),
                    });
                    if let Some(v) = &upd {
                        let _ = tray.set_tooltip(Some(format!("SmoothKvm - version {v} is available")));
                    }
                    shown_update = upd;
                }
                let alerts = crate::shared::alert_count();
                if alerts != shown_alerts {
                    shown_alerts = alerts;
                    let _ = tray.set_tooltip(Some(if alerts > 0 { format!("SmoothKvm - {alerts} security alert(s), open the app to see them") } else { "SmoothKvm".to_string() }));
                }
                let paused = capture_win::is_paused();
                if paused != shown_paused {
                    shown_paused = paused;
                    stop_item.set_text(if paused { "Resume screen switching" } else { "Emergency stop" });
                    let _ = tray.set_icon(Some(tray_icon(paused)));
                    let _ = tray.set_tooltip(Some(if paused { "SmoothKvm - STOPPED (screen switching paused)" } else { "SmoothKvm" }));
                }
            }
            Event::UserEvent(UserEvent::Show) => match &window {
                Some((w, _)) => {
                    w.set_visible(true);
                    w.set_minimized(false);
                    w.set_focus();
                }
                None => match open_window(target, &mut ctx) {
                    Ok(w) => window = Some(w),
                    Err(e) => eprintln!("cannot open window: {e}"),
                },
            },
            Event::UserEvent(UserEvent::Quit) => {
                shutdown();
                *flow = ControlFlow::Exit
            }
            Event::UserEvent(UserEvent::Menu(e)) => {
                if e.id == *open_item.id() {
                    post(UserEvent::Show);
                } else if e.id == *stop_item.id() {
                    if capture_win::is_paused() {
                        capture_win::Controller.resume();
                    } else {
                        capture_win::Controller.emergency_stop();
                    }
                } else if e.id == *update_item.id() {
                    if crate::update::available().is_some() {
                        // download, verify the signature, install silently, restart
                        if let Err(e) = crate::update::apply_async() {
                            crate::shared::warn(&format!("Update: {e}"));
                        }
                    } else {
                        std::thread::spawn(|| {
                            let _ = crate::update::check();
                        });
                    }
                    post(UserEvent::Show);
                } else if e.id == *quit_item.id() {
                    shutdown();
                    *flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UserEvent::Tray(TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. })) => {
                post(UserEvent::Show);
            }
            // Closing the window QUITS the app: nothing keeps running in the background.
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                shutdown();
                *flow = ControlFlow::Exit;
            }
            _ => {}
        }
    })
}

/// Stop everything cleanly: release every key/button on the other computers, then let the process end
/// (which also removes the keyboard/mouse hooks and closes every connection and listener).
fn shutdown() {
    crate::shared::info("SmoothKvm is quitting: releasing all keys and closing every connection");
    capture_win::Controller.emergency_stop();
    std::thread::sleep(Duration::from_millis(250));
    std::process::exit(0);
}
