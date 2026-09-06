//! Native window + menubar tray. The title shows status + version +
//! capture permission (+ pending update), nothing else.
//!
//! The close button hides to the tray instead of quitting; the tray menu has
//! Open + Check-for-updates + Install-update (when one is known) + Quit.

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use jlocal::status::{self, AppState};

/// Monotone black-on-transparent menubar icon, baked from
/// `web/public/favicon.svg` (see `assets/`). 32px: crisp on Retina, and
/// macOS downscales it into the menubar slot.
const TRAY_PNG: &[u8] = include_bytes!("../assets/tray-32.png");

/// Events the background threads (update poller, tray menu) push into the
/// winit loop. The loop itself stays on `ControlFlow::Wait`.
#[derive(Debug)]
enum UiEvent {
    /// Update state changed (or a manual check finished): refresh the title
    /// and the tray menu from `AppState`.
    Refresh,
    /// A tray menu item (or icon click) fired.
    Menu(MenuAction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuAction {
    Open,
    Quit,
    CheckNow,
    Install,
}

impl MenuAction {
    fn from_id(id: &str) -> Option<Self> {
        match id {
            "jlocal-open" => Some(MenuAction::Open),
            "jlocal-quit" => Some(MenuAction::Quit),
            "jlocal-check" => Some(MenuAction::CheckNow),
            "jlocal-install" => Some(MenuAction::Install),
            _ => None,
        }
    }
}

struct App {
    state: AppState,
    port: u16,
    rt: tokio::runtime::Handle,
    proxy: EventLoopProxy<UiEvent>,
    window: Option<Window>,
    tray: Option<tray_icon::TrayIcon>,
    /// The one menu item we relabel (Open/Check/Quit are static).
    install_item: Option<tray_icon::menu::MenuItem>,
}

impl App {
    fn title(&self) -> String {
        let tag = self.state.update.lock().latest_tag.clone();
        status::window_title(self.port, tag.as_deref())
    }

    /// Refresh everything the background threads can change: window title,
    /// tray tooltip, and the install item (label + enabled).
    fn refresh(&self) {
        let title = self.title();
        if let Some(w) = &self.window {
            w.set_title(&title);
        }
        if let Some(tray) = &self.tray {
            let _ = tray.set_tooltip(Some(title));
        }
        if let Some(item) = &self.install_item {
            let tag = self.state.update.lock().latest_tag.clone();
            match tag {
                Some(tag) => {
                    item.set_text(format!("Install update {tag}"));
                    item.set_enabled(true);
                }
                None => {
                    item.set_text("Install update");
                    item.set_enabled(false);
                }
            }
        }
    }

    fn show_window(&self) {
        if let Some(w) = &self.window {
            w.set_visible(true);
            w.focus_window();
        }
    }

    fn hide_window(&self) {
        if let Some(w) = &self.window {
            w.set_visible(false);
        }
    }

    /// Manual "check now" from the tray: one probe, then a refresh. Best
    /// effort like the poller — a failure only records itself.
    fn check_now(&self) {
        let state = self.state.clone();
        let proxy = self.proxy.clone();
        self.rt.spawn(async move {
            if let Ok(client) = jlocal::update::client() {
                jlocal::update::check_once(&state, &client).await;
            }
            let _ = proxy.send_event(UiEvent::Refresh);
        });
    }

    /// "Install update" from the tray: download the recorded tag's asset,
    /// swap the executable, relaunch. A fetch failure records itself and
    /// refreshes; a successful install exits this process.
    fn install_update(&self) {
        let state = self.state.clone();
        let proxy = self.proxy.clone();
        self.rt.spawn(async move {
            let tag = state.update.lock().latest_tag.clone();
            let Some(tag) = tag else { return };
            let fetched = match jlocal::update::client() {
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
                    // Install exits on success; reaching here means it
                    // failed (already logged). Refresh so the menu stays
                    // in sync.
                    let _ = proxy.send_event(UiEvent::Refresh);
                }
                Err(e) => {
                    state.update.lock().last_error = Some(e.to_string());
                    let _ = proxy.send_event(UiEvent::Refresh);
                }
            }
        });
    }
}

impl ApplicationHandler<UiEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_none() {
            let attrs = winit::window::WindowAttributes::default()
                .with_title(self.title())
                .with_inner_size(winit::dpi::LogicalSize::new(360.0, 140.0));
            match event_loop.create_window(attrs) {
                Ok(w) => self.window = Some(w),
                Err(e) => {
                    eprintln!("jlocal: could not open window ({e}); API still serving.");
                    event_loop.exit();
                    return;
                }
            }
        }
        if self.tray.is_none() {
            match build_tray(&self.title()) {
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

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // Close hides to the tray; Quit lives in the tray menu.
        if matches!(event, WindowEvent::CloseRequested) {
            self.hide_window();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UiEvent) {
        match event {
            UiEvent::Refresh => self.refresh(),
            UiEvent::Menu(MenuAction::Open) => self.show_window(),
            UiEvent::Menu(MenuAction::Quit) => event_loop.exit(),
            UiEvent::Menu(MenuAction::CheckNow) => self.check_now(),
            UiEvent::Menu(MenuAction::Install) => self.install_update(),
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
fn build_tray(title: &str) -> anyhow::Result<(tray_icon::TrayIcon, tray_icon::menu::MenuItem)> {
    use tray_icon::menu::{Menu, MenuItem, PredefinedMenuItem};
    let menu = Menu::new();
    let open_item = MenuItem::with_id("jlocal-open", "Open jlocal", true, None);
    let check_item = MenuItem::with_id("jlocal-check", "Check for updates now", true, None);
    let install_item = MenuItem::with_id("jlocal-install", "Install update", false, None);
    let quit_item = MenuItem::with_id("jlocal-quit", "Quit jlocal", true, None);
    menu.append_items(&[
        &open_item,
        &check_item,
        &install_item,
        &PredefinedMenuItem::separator(),
        &quit_item,
    ])?;
    let tray = tray_icon::TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(title)
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
                        if let Some(action) = MenuAction::from_id(event.id.0.as_str()) {
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
                        ) && proxy.send_event(UiEvent::Menu(MenuAction::Open)).is_err()
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
    // Each landing refreshes the title + tray menu via the loop.
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
        tray: None,
        install_item: None,
    };
    let _ = event_loop.run_app(&mut app);
    drop(rt);
}
