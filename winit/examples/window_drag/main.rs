//! Manual window-move and borderless-resize regression fixture.
//!
//! Run `cargo run -p winit --example window_drag` on a desktop. Tap or hold and
//! release the center without moving, then drag it. On macOS taps must not log
//! "Window move completed without beginning"; real moves retain native behavior.
//! Also click once, then immediately press again, hold, and drag the second press:
//! on macOS it must remain stationary and quiet, like native background dragging.
//! Drag each edge/corner, including outside the window, and check size limits.
//! Escape exits. The fixture owns one window and no saved settings or input hooks.

use std::error::Error;

use softbuffer::{Context, Surface};
use tracing::{error, info};
use winit::{
    application::ApplicationHandler,
    dpi::{LogicalPosition, LogicalSize},
    event::{ElementState, MouseButton, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop, OwnedDisplayHandle},
    keyboard::{Key, NamedKey},
    window::{ResizeDirection, Window, WindowAttributes, WindowId},
};

#[path = "../util/fill.rs"]
mod fill;

#[path = "../util/tracing.rs"]
mod tracing_init;

/// Width of the logical-point resize grab strip around the client area.
const BORDER: f64 = 8.0;
const BACKGROUND: u32 = 0xff205060;

#[derive(Default)]
struct App {
    surface: Option<Surface<OwnedDisplayHandle, Box<dyn Window>>>,
}

fn resize_direction(
    point: LogicalPosition<f64>,
    size: LogicalSize<f64>,
) -> Option<ResizeDirection> {
    use ResizeDirection::{East, North, NorthEast, NorthWest, South, SouthEast, SouthWest, West};

    match (
        point.x < BORDER,
        point.x >= size.width - BORDER,
        point.y < BORDER,
        point.y >= size.height - BORDER,
    ) {
        (true, false, true, false) => Some(NorthWest),
        (false, true, true, false) => Some(NorthEast),
        (true, false, false, true) => Some(SouthWest),
        (false, true, false, true) => Some(SouthEast),
        (true, false, false, false) => Some(West),
        (false, true, false, false) => Some(East),
        (false, false, true, false) => Some(North),
        (false, false, false, true) => Some(South),
        _ => None,
    }
}

impl ApplicationHandler for App {
    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        let attributes = WindowAttributes::default()
            .with_title("winit window drag regression")
            .with_decorations(false)
            .with_transparent(true)
            .with_resizable(true)
            .with_surface_size(LogicalSize::new(400.0, 260.0))
            .with_min_surface_size(LogicalSize::new(200.0, 160.0))
            .with_max_surface_size(LogicalSize::new(800.0, 600.0));
        let window = event_loop.create_window(attributes).expect("fixture window creation");
        let context = Context::new(event_loop.owned_display_handle()).expect("fixture display");
        self.surface = Some(Surface::new(&context, window).expect("fixture surface"));
    }

    fn window_event(&mut self, event_loop: &dyn ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let Some(surface) = self.surface.as_mut() else { return };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && event.logical_key == Key::Named(NamedKey::Escape) =>
            {
                event_loop.exit();
            },
            WindowEvent::PointerButton { state, button, position, .. } => {
                if button.mouse_button() != Some(MouseButton::Left) {
                    return;
                }
                info!("primary button: {state:?}");
                if state == ElementState::Pressed {
                    let window = surface.window();
                    let scale = window.scale_factor();
                    let direction = resize_direction(
                        position.to_logical(scale),
                        window.surface_size().to_logical(scale),
                    );
                    let result = match direction {
                        Some(direction) => window.drag_resize_window(direction),
                        None => window.drag_window(),
                    };
                    if let Err(error) = result {
                        error!("window drag request failed: {error}");
                    }
                }
            },
            WindowEvent::SurfaceResized(size) => {
                info!("resized: {size:?}");
                fill::resize(surface, size);
                surface.window().request_redraw();
            },
            WindowEvent::Moved(position) => info!("moved: {position:?}"),
            WindowEvent::RedrawRequested => {
                surface.window().pre_present_notify();
                fill::fill_with_color(surface, BACKGROUND);
            },
            _ => (),
        }
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    tracing_init::init();
    EventLoop::new()?.run_app(App::default())?;
    Ok(())
}
