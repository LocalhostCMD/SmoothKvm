//! The SmoothKvm app shell (Windows): a tray icon that is always there, and a native window
//! that exists only while you have the panel open (so the idle app uses almost no memory).
use crate::{app, capture_win, daemon, store};
use std::io;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use tao::dpi::{LogicalSize, PhysicalPosition};
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
static TOLD_TRAY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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

/// The tray panel: a small borderless window above (or below) the tray icon, showing the compact view of
/// the app page. It exists only while it is open and closes when you click anywhere else.
fn open_flyout(target: &tao::event_loop::EventLoopWindowTarget<UserEvent>, ctx: &mut WebContext, at: PhysicalPosition<f64>) -> io::Result<(Window, WebView)> {
    let err = |e: String| io::Error::new(io::ErrorKind::Other, e);
    let window = WindowBuilder::new()
        .with_title("SmoothKvm")
        .with_inner_size(LogicalSize::new(330.0, 430.0))
        .with_decorations(false)
        .with_resizable(false)
        .with_always_on_top(true)
        .with_visible(false)
        .with_window_icon(WinIcon::from_rgba(icon_rgba(false), 32, 32).ok())
        .build(target)
        .map_err(|e| err(e.to_string()))?;
    {
        use tao::platform::windows::WindowExtWindows;
        let _ = window.set_skip_taskbar(true);
    }
    // next to the tray icon, kept inside the monitor it is on (the taskbar can be on any side)
    let size = window.outer_size();
    let (w, h) = (size.width as f64, size.height as f64);
    let mon = target
        .available_monitors()
        .find(|m| {
            let (p, s) = (m.position(), m.size());
            at.x >= p.x as f64 && at.x < p.x as f64 + s.width as f64 && at.y >= p.y as f64 && at.y < p.y as f64 + s.height as f64
        })
        .or_else(|| target.primary_monitor());
    let (mut x, mut y) = (at.x - w / 2.0, at.y - h - 14.0);
    if let Some(m) = mon {
        let (p, s) = (m.position(), m.size());
        let (l, t, r, b) = (p.x as f64 + 8.0, p.y as f64 + 8.0, p.x as f64 + s.width as f64 - 8.0, p.y as f64 + s.height as f64 - 8.0);
        if y < t {
            y = at.y + 14.0; // taskbar at the top
        }
        x = x.clamp(l, (r - w).max(l));
        y = y.clamp(t, (b - h - 44.0).max(t));
    }
    window.set_outer_position(PhysicalPosition::new(x, y));
    let url = format!("http://127.0.0.1:{}/?view=tray#{}", crate::httpd::port(), store::panel_token()?);
    let webview = WebViewBuilder::new_with_web_context(ctx)
        .with_url(&url)
        .with_navigation_handler(|u| u.starts_with(&format!("http://127.0.0.1:{}", crate::httpd::port())))
        .with_new_window_req_handler(|_, _| NewWindowResponse::Deny)
        .build(&window)
        .map_err(|e| err(e.to_string()))?;
    window.set_visible(true);
    window.set_focus();
    Ok((window, webview))
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

pub fn run(show: bool) -> io::Result<()> {
    daemon::start_backend()?;
    crate::shared::info(&format!("SmoothKvm {} started. It runs while its tray icon is there; quit from the tray menu.", crate::VERSION));
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
    let mut flyout: Option<(Window, WebView)> = None;
    let mut flyout_closed = Instant::now() - Duration::from_secs(5);
    let mut shown_paused = false;
    let mut shown_alerts = 0usize;
    let mut shown_update: Option<String> = None;
    // Opened by you: show the window. Started at sign-in: just the tray icon (always visible there).
    if show {
        post(UserEvent::Show);
    }
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
            // Click the tray icon: the small panel. Double-click: the full window.
            Event::UserEvent(UserEvent::Tray(TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, position, .. })) => {
                if flyout.is_some() {
                    flyout = None;
                } else if flyout_closed.elapsed() > Duration::from_millis(350) {
                    // (a click on the icon while the panel is open first takes its focus away, which closes it)
                    match open_flyout(target, &mut ctx, position) {
                        Ok(f) => flyout = Some(f),
                        Err(e) => {
                            eprintln!("cannot open the tray panel: {e}");
                            post(UserEvent::Show);
                        }
                    }
                }
            }
            Event::UserEvent(UserEvent::Tray(TrayIconEvent::DoubleClick { button: MouseButton::Left, .. })) => {
                flyout = None;
                post(UserEvent::Show);
            }
            // The tray panel closes as soon as you click somewhere else.
            Event::WindowEvent { window_id, event: WindowEvent::Focused(false), .. } if flyout.as_ref().map(|(w, _)| w.id()) == Some(window_id) => {
                flyout = None;
                flyout_closed = Instant::now();
            }
            Event::WindowEvent { window_id, event: WindowEvent::CloseRequested, .. } => {
                if flyout.as_ref().map(|(w, _)| w.id()) == Some(window_id) {
                    flyout = None;
                } else if store::load_config().close_quits {
                    shutdown();
                    *flow = ControlFlow::Exit;
                } else {
                    // keeps working from the tray; the window (and its memory) goes away
                    if window.take().is_some() && !TOLD_TRAY.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        crate::shared::info("The window was closed; SmoothKvm keeps running from its tray icon (right-click it to quit)");
                    }
                }
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
