//! Native status window + menubar tray.
//!
//! The window is a small card drawn by hand (see `paint`): what the app is
//! connected to, whether the OS lets it record the screen, whether the
//! torrent engine booted, and whether a newer release is waiting — with the
//! buttons to check and install. The close button hides it to the tray (on
//! macOS the app also leaves the Dock until it is opened again); Quit lives
//! in the tray menu and in the window.

use std::rc::Rc;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{CursorIcon, Window, WindowId};

use jlocal::status::{self, AppState};

use crate::paint::{Canvas, Rect, Weight};

/// Monotone black-on-transparent menubar icon, baked from
/// `web/public/favicon.svg` (see `assets/`). 32px: crisp on Retina, and
/// macOS downscales it into the menubar slot.
const TRAY_PNG: &[u8] = include_bytes!("../assets/tray-32.png");

const WINDOW_W: f32 = 400.0;
const WINDOW_H: f32 = 258.0;
const PAD: f32 = 22.0;
const ROW_H: f32 = 30.0;

const BG: u32 = 0x0e0f14;
const TEXT: u32 = 0xf2f2f5;
const DIM: u32 = 0x8b8e99;
const PILL: u32 = 0x1d1f29;
const PILL_HOVER: u32 = 0x282b38;
const GREEN: u32 = 0x3dd68c;
const AMBER: u32 = 0xf5b84b;
const PRIMARY: u32 = 0xff7a45;
const DANGER: u32 = 0xf0434e;

/// Events the background threads (update poller, tray menu) push into the
/// winit loop. The loop itself stays on `ControlFlow::Wait`.
#[derive(Debug)]
enum UiEvent {
    /// Update state changed (or a check/install finished): redraw the
    /// window and relabel the tray menu from `AppState`.
    Refresh,
    /// A tray menu item (or icon click) fired.
    Menu(Action),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Open,
    Quit,
    CheckNow,
    Install,
}

impl Action {
    fn from_id(id: &str) -> Option<Self> {
        match id {
            "jlocal-open" => Some(Action::Open),
            "jlocal-quit" => Some(Action::Quit),
            "jlocal-check" => Some(Action::CheckNow),
            "jlocal-install" => Some(Action::Install),
            _ => None,
        }
    }
}

/// One clickable pill, as laid out on the last draw.
struct Button {
    rect: Rect,
    action: Action,
}

struct App {
    state: AppState,
    port: u16,
    rt: tokio::runtime::Handle,
    proxy: EventLoopProxy<UiEvent>,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    tray: Option<tray_icon::TrayIcon>,
    /// The one menu item we relabel (Open/Check/Quit are static).
    install_item: Option<tray_icon::menu::MenuItem>,
    buttons: Vec<Button>,
    cursor: Option<(f32, f32)>,
}

/// What the window says about updates, in one line.
fn update_line(update: &jlocal::update::UpdateState) -> (String, u32) {
    if update.busy {
        return ("verificando…".into(), DIM);
    }
    if let Some(tag) = &update.latest_tag {
        return (format!("{tag} disponível"), PRIMARY);
    }
    if update.last_error.is_some() {
        return ("não deu para verificar".into(), AMBER);
    }
    if update.last_checked_unix == 0 {
        return ("ainda não verificada".into(), DIM);
    }
    ("em dia".into(), GREEN)
}

impl App {
    fn tooltip(&self) -> String {
        let tag = self.state.update.lock().latest_tag.clone();
        status::window_title(self.port, tag.as_deref())
    }

    /// Refresh everything the background threads can change: the window,
    /// the tray tooltip, and the install item (label + enabled).
    fn refresh(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
        if let Some(tray) = &self.tray {
            let _ = tray.set_tooltip(Some(self.tooltip()));
        }
        if let Some(item) = &self.install_item {
            let tag = self.state.update.lock().latest_tag.clone();
            match tag {
                Some(tag) => {
                    item.set_text(format!("Instalar atualização {tag}"));
                    item.set_enabled(true);
                }
                None => {
                    item.set_text("Instalar atualização");
                    item.set_enabled(false);
                }
            }
        }
    }

    fn show_window(&self) {
        set_dock_presence(true);
        if let Some(w) = &self.window {
            w.set_visible(true);
            w.focus_window();
            w.request_redraw();
        }
    }

    fn hide_window(&self) {
        if let Some(w) = &self.window {
            w.set_visible(false);
        }
        set_dock_presence(false);
    }

    fn set_busy(&self, busy: bool) {
        self.state.update.lock().busy = busy;
        let _ = self.proxy.send_event(UiEvent::Refresh);
    }

    /// Manual check: one probe, then a refresh. Best effort like the
    /// poller — a failure only records itself.
    fn check_now(&self) {
        if self.state.update.lock().busy {
            return;
        }
        self.set_busy(true);
        let state = self.state.clone();
        let proxy = self.proxy.clone();
        self.rt.spawn(async move {
            if let Ok(client) = jlocal::update::client() {
                jlocal::update::check_once(&state, &client).await;
            }
            state.update.lock().busy = false;
            let _ = proxy.send_event(UiEvent::Refresh);
        });
    }

    /// Download the recorded tag's asset, swap the executable, relaunch.
    /// A fetch failure records itself and refreshes; a successful install
    /// exits this process.
    fn install_update(&self) {
        if self.state.update.lock().busy {
            return;
        }
        let state = self.state.clone();
        let proxy = self.proxy.clone();
        let tag = state.update.lock().latest_tag.clone();
        let Some(tag) = tag else { return };
        self.set_busy(true);
        self.rt.spawn(async move {
            let fetched = match jlocal::update::download_client() {
                Ok(client) => jlocal::update::fetch_binary(&client, &tag).await,
                Err(e) => Err(e),
            };
            match fetched {
                Ok(binary) => {
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Err(e) = jlocal::update::install_and_relaunch(&binary) {
                            eprintln!("jlocal: update install failed: {e:#}");
                        }
                    })
                    .await;
                }
                Err(e) => state.update.lock().last_error = Some(e.to_string()),
            }
            // Install exits on success; reaching here means it failed
            // (already logged), so the window must say so.
            state.update.lock().busy = false;
            let _ = proxy.send_event(UiEvent::Refresh);
        });
    }

    fn act(&self, action: Action, event_loop: &ActiveEventLoop) {
        match action {
            Action::Open => self.show_window(),
            Action::Quit => event_loop.exit(),
            Action::CheckNow => self.check_now(),
            Action::Install => self.install_update(),
        }
    }

    fn hovered(&self) -> Option<Action> {
        let (x, y) = self.cursor?;
        self.buttons
            .iter()
            .find(|b| b.rect.contains(x, y))
            .map(|b| b.action)
    }

    fn draw(&mut self) {
        let hovered = self.hovered();
        let (Some(window), Some(surface)) = (&self.window, self.surface.as_mut()) else {
            return;
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (
            std::num::NonZeroU32::new(size.width),
            std::num::NonZeroU32::new(size.height),
        ) else {
            return;
        };
        if surface.resize(w, h).is_err() {
            return;
        }
        let scale = window.scale_factor() as f32;
        let mut canvas = Canvas::new(size.width as usize, size.height as usize, scale, BG);
        self.buttons = layout(&mut canvas, &self.state, self.port, hovered);
        if let Ok(mut buffer) = surface.buffer_mut() {
            buffer.copy_from_slice(&canvas.pixels);
            let _ = buffer.present();
        }
    }
}

/// Paints the card and returns the buttons where they landed.
fn layout(
    canvas: &mut Canvas,
    state: &AppState,
    port: u16,
    hovered: Option<Action>,
) -> Vec<Button> {
    let mut y = PAD;
    canvas.text("jlocal", PAD, y, 17.0, Weight::Semibold, TEXT);
    let version = status::VERSION;
    let vw = canvas.text_width(version, 12.0, Weight::Regular);
    canvas.text(
        version,
        WINDOW_W - PAD - vw,
        y + 4.0,
        12.0,
        Weight::Regular,
        DIM,
    );
    y += 40.0;

    let capture = jlocal::permissions::screen_capture_granted();
    let update = state.update.lock().clone();
    let (update_text, update_color) = update_line(&update);
    let rows: [(&str, String, u32); 4] = [
        ("Site", format!("conectado · 127.0.0.1:{port}"), GREEN),
        (
            "Gravação de tela",
            if capture {
                "permitida".into()
            } else {
                "bloqueada · Ajustes › Privacidade".into()
            },
            if capture { GREEN } else { AMBER },
        ),
        (
            "Torrent",
            if state.caps.torrent {
                "pronto".into()
            } else {
                "indisponível".into()
            },
            if state.caps.torrent { GREEN } else { AMBER },
        ),
        ("Atualização", update_text, update_color),
    ];
    for (label, value, color) in rows {
        canvas.dot(PAD + 4.0, y + 9.5, 3.5, color);
        canvas.text(label, PAD + 18.0, y, 13.0, Weight::Regular, TEXT);
        let vw = canvas.text_width(&value, 12.5, Weight::Regular);
        canvas.text(
            &value,
            WINDOW_W - PAD - vw,
            y + 1.0,
            12.5,
            Weight::Regular,
            DIM,
        );
        y += ROW_H;
    }

    let mut buttons = Vec::new();
    let by = WINDOW_H - PAD - 32.0;
    let mut bx = PAD;
    let mut pill =
        |canvas: &mut Canvas, label: &str, action: Action, accent: Option<u32>, enabled: bool| {
            let tw = canvas.text_width(label, 12.5, Weight::Semibold);
            let rect = Rect {
                x: bx,
                y: by,
                w: tw + 28.0,
                h: 32.0,
            };
            let hover = enabled && hovered == Some(action);
            let (fill, ink) = match accent {
                Some(color) if hover => (color, 0xffffff),
                Some(color) => (color, 0xffffff),
                None if hover => (PILL_HOVER, TEXT),
                None => (PILL, if enabled { TEXT } else { DIM }),
            };
            canvas.round_rect(rect, 16.0, fill);
            canvas.text(label, rect.x + 14.0, by + 8.0, 12.5, Weight::Semibold, ink);
            bx += rect.w + 8.0;
            if enabled {
                buttons.push(Button { rect, action });
            }
        };
    let busy = update.busy;
    match &update.latest_tag {
        Some(tag) => pill(
            canvas,
            &format!("Instalar {tag}"),
            Action::Install,
            Some(PRIMARY),
            !busy,
        ),
        None => pill(
            canvas,
            "Verificar atualizações",
            Action::CheckNow,
            None,
            !busy,
        ),
    }
    let qw = canvas.text_width("Sair", 12.5, Weight::Semibold) + 28.0;
    let quit = Rect {
        x: WINDOW_W - PAD - qw,
        y: by,
        w: qw,
        h: 32.0,
    };
    let quit_hover = hovered == Some(Action::Quit);
    canvas.round_rect(quit, 16.0, if quit_hover { 0x3a1a1f } else { PILL });
    canvas.text(
        "Sair",
        quit.x + 14.0,
        by + 8.0,
        12.5,
        Weight::Semibold,
        DANGER,
    );
    buttons.push(Button {
        rect: quit,
        action: Action::Quit,
    });
    buttons
}

/// On macOS a hidden window must not leave a Dock tile behind: the app
/// becomes an accessory (menubar only) until it is opened again.
#[cfg(target_os = "macos")]
fn set_dock_presence(visible: bool) {
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
    let Some(mtm) = objc2_foundation::MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    let policy = if visible {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Accessory
    };
    app.setActivationPolicy(policy);
    if visible {
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
}

#[cfg(not(target_os = "macos"))]
fn set_dock_presence(_visible: bool) {}

impl ApplicationHandler<UiEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            let attrs = winit::window::WindowAttributes::default()
                .with_title("jlocal")
                .with_resizable(false)
                .with_inner_size(winit::dpi::LogicalSize::new(WINDOW_W, WINDOW_H));
            match event_loop.create_window(attrs) {
                Ok(w) => {
                    let window = Rc::new(w);
                    let surface = softbuffer::Context::new(window.clone())
                        .and_then(|context| softbuffer::Surface::new(&context, window.clone()));
                    match surface {
                        Ok(surface) => self.surface = Some(surface),
                        Err(e) => eprintln!(
                            "jlocal: window has no drawing surface ({e}); it stays blank."
                        ),
                    }
                    self.window = Some(window);
                }
                Err(e) => {
                    eprintln!("jlocal: could not open window ({e}); API still serving.");
                    event_loop.exit();
                    return;
                }
            }
        }
        if self.tray.is_none() {
            match build_tray(&self.tooltip()) {
                Ok((tray, install_item)) => {
                    self.tray = Some(tray);
                    self.install_item = Some(install_item);
                    self.refresh();
                }
                Err(e) => {
                    eprintln!("jlocal: no menubar tray ({e:#}); window only.");
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            // Close hides to the tray; Quit lives in the menu and the window.
            WindowEvent::CloseRequested => self.hide_window(),
            WindowEvent::RedrawRequested => self.draw(),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let scale = self
                    .window
                    .as_ref()
                    .map(|w| w.scale_factor())
                    .unwrap_or(1.0);
                let before = self.hovered();
                self.cursor = Some(((position.x / scale) as f32, (position.y / scale) as f32));
                let after = self.hovered();
                if let Some(w) = &self.window {
                    w.set_cursor(if after.is_some() {
                        CursorIcon::Pointer
                    } else {
                        CursorIcon::Default
                    });
                    if before != after {
                        w.request_redraw();
                    }
                }
            }
            WindowEvent::CursorLeft { .. } => {
                self.cursor = None;
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                if let Some(action) = self.hovered() {
                    self.act(action, event_loop);
                }
            }
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UiEvent) {
        match event {
            UiEvent::Refresh => self.refresh(),
            UiEvent::Menu(action) => self.act(action, event_loop),
        }
    }
}

/// Decode the baked PNG into a template tray icon.
fn tray_image() -> anyhow::Result<tray_icon::Icon> {
    let img = image::load_from_memory(TRAY_PNG)?.to_rgba8();
    let (width, height) = (img.width(), img.height());
    tray_icon::Icon::from_rgba(img.into_raw(), width, height).map_err(anyhow::Error::from)
}

/// Build the tray icon + menu. Returns the icon and the install item (the
/// only one we relabel — Open/Check/Quit are static and owned by the menu).
fn build_tray(tooltip: &str) -> anyhow::Result<(tray_icon::TrayIcon, tray_icon::menu::MenuItem)> {
    use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
    let menu = Menu::new();
    let open_item = MenuItem::with_id("jlocal-open", "Abrir jlocal", true, None);
    let check_item = MenuItem::with_id("jlocal-check", "Verificar atualizações", true, None);
    let install_item = MenuItem::with_id("jlocal-install", "Instalar atualização", false, None);
    let quit_item = MenuItem::with_id("jlocal-quit", "Sair do jlocal", true, None);
    menu.append_items(&[
        &open_item,
        &check_item,
        &install_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])?;
    let tray = tray_icon::TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(tooltip)
        .with_icon(tray_image()?)
        .with_icon_as_template(true)
        .build()?;
    Ok((tray, install_item))
}

/// Forward tray menu picks + icon clicks into the winit loop. Runs on its
/// own thread, blocking on the tray channels; ends when the loop is gone
/// (send fails) or the channels close.
fn forward_tray_events(proxy: EventLoopProxy<UiEvent>) {
    if std::thread::Builder::new()
        .name("jlocal-tray-events".into())
        .spawn(move || {
            let menu_rx = tray_icon::menu::MenuEvent::receiver();
            let tray_rx = tray_icon::TrayIconEvent::receiver();
            loop {
                crossbeam_channel::select! {
                    recv(menu_rx) -> event => {
                        let Ok(event) = event else { break };
                        if let Some(action) = Action::from_id(event.id.0.as_str()) {
                            if proxy.send_event(UiEvent::Menu(action)).is_err() {
                                break;
                            }
                        }
                    }
                    recv(tray_rx) -> event => {
                        let Ok(event) = event else { break };
                        // Left-click opens; everything else lives in the menu.
                        if matches!(
                            event,
                            tray_icon::TrayIconEvent::Click {
                                button: tray_icon::MouseButton::Left,
                                ..
                            }
                        ) && proxy.send_event(UiEvent::Menu(Action::Open)).is_err()
                        {
                            break;
                        }
                    }
                }
            }
        })
        .is_err()
    {
        eprintln!("jlocal: could not spawn tray event thread; menu clicks will not register.");
    }
}

/// Runs the window on the calling (main) thread. Owns the tokio runtime so
/// dropping it after the loop shuts the loopback API down cleanly.
pub fn run(rt: tokio::runtime::Runtime, state: AppState, port: u16) {
    let event_loop = match EventLoop::<UiEvent>::with_user_event().build() {
        Ok(el) => el,
        Err(e) => {
            eprintln!("jlocal: no windowing available ({e}); use --no-ui.");
            return;
        }
    };
    event_loop.set_control_flow(ControlFlow::Wait);
    // Self-update poller: boot + every 6h, best-effort, never blocks boot.
    // Each landing redraws the window + relabels the tray via the loop.
    let proxy = event_loop.create_proxy();
    {
        let state = state.clone();
        let proxy = proxy.clone();
        rt.spawn(jlocal::update::run_poller(state, move || {
            let _ = proxy.send_event(UiEvent::Refresh);
        }));
    }
    forward_tray_events(proxy.clone());
    let mut app = App {
        state,
        port,
        rt: rt.handle().clone(),
        proxy,
        window: None,
        surface: None,
        tray: None,
        install_item: None,
        buttons: Vec::new(),
        cursor: None,
    };
    let _ = event_loop.run_app(&mut app);
    drop(rt);
}
