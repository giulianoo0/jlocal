//! Minimal native window. The title shows status + version, nothing else.

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

use crate::status::AppState;

struct App {
    title: String,
    window: Option<Window>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = winit::window::WindowAttributes::default()
            .with_title(self.title.clone())
            .with_inner_size(winit::dpi::LogicalSize::new(360.0, 140.0));
        match event_loop.create_window(attrs) {
            Ok(w) => self.window = Some(w),
            Err(e) => {
                eprintln!("jlocal: could not open window ({e}); API still serving.");
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        if matches!(event, WindowEvent::CloseRequested) {
            event_loop.exit();
        }
    }
}

/// Runs the window on the calling (main) thread. Owns the tokio runtime so
/// dropping it after the loop shuts the loopback API down cleanly.
pub fn run(rt: tokio::runtime::Runtime, state: AppState, port: u16) {
    let _ = &state;
    let title = format!(
        "jlocal {} — connected (127.0.0.1:{port})",
        crate::status::VERSION
    );
    let event_loop = match EventLoop::new() {
        Ok(el) => el,
        Err(e) => {
            eprintln!("jlocal: no windowing available ({e}); use --no-ui.");
            return;
        }
    };
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut app = App {
        title,
        window: None,
    };
    let _ = event_loop.run_app(&mut app);
    drop(rt);
}
