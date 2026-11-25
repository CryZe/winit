//! Checks drop-and-replace within one callback, with a real Wayland popup grab.
//! Click the test window once to supply the input serial required by xdg-shell.
#![cfg(wayland_platform)]

use std::collections::HashSet;
use std::time::{Duration, Instant};

use softbuffer::{Context, Surface};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, OwnedDisplayHandle};
use winit::platform::wayland::{EventLoopBuilderExtWayland, WindowExtWayland};
use winit::raw_window_handle::HasWindowHandle;
use winit::window::{Window, WindowAttributes, WindowId, WindowType};

type TestSurface = Surface<OwnedDisplayHandle, Box<dyn Window>>;

struct Replacement {
    context: Context<OwnedDisplayHandle>,
    toplevel: Option<TestSurface>,
    menu: Option<TestSurface>,
    pending_destroy: HashSet<WindowId>,
    deadline: Instant,
}

#[track_caller]
fn map_window(context: &Context<OwnedDisplayHandle>, window: Box<dyn Window>) -> TestSurface {
    let size = window.surface_size();
    let mut surface = Surface::new(context, window).unwrap();
    surface.resize(size.width.try_into().unwrap(), size.height.try_into().unwrap()).unwrap();
    let mut buffer = surface.buffer_mut().unwrap();
    buffer.fill(0x447799);
    buffer.present().unwrap();
    surface
}

#[track_caller]
fn popup(
    event_loop: &dyn ActiveEventLoop,
    context: &Context<OwnedDisplayHandle>,
    parent: &dyn Window,
    active: bool,
) -> TestSurface {
    let attributes = WindowAttributes::default()
        .with_window_type(WindowType::Popup)
        .with_surface_size(PhysicalSize::new(40, 40))
        .with_active(active);
    // SAFETY: The caller keeps the mapped parent alive until the child is dropped.
    let attributes =
        unsafe { attributes.with_parent_window(Some(parent.window_handle().unwrap().as_raw())) };
    let window = event_loop.create_window(attributes).unwrap();
    assert_eq!(window.uses_popup_grab(), active);
    map_window(context, window)
}

impl ApplicationHandler for Replacement {
    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        let attributes = WindowAttributes::default()
            .with_title("Click to test Wayland submenu replacement")
            .with_surface_size(PhysicalSize::new(320, 160));
        self.toplevel =
            Some(map_window(&self.context, event_loop.create_window(attributes).unwrap()));
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.deadline));
    }

    fn window_event(&mut self, event_loop: &dyn ActiveEventLoop, id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::PointerButton { state: ElementState::Pressed, .. }
                if self.menu.is_none() =>
            {
                let parent = self.toplevel.as_ref().unwrap().window().as_ref();
                assert!(!parent.uses_popup_grab());
                let menu = popup(event_loop, &self.context, parent, true);
                // Passive children (e.g. help for a menu item) do not share the
                // parent's grab and still follow their owner's focus policy.
                let help = popup(event_loop, &self.context, menu.window().as_ref(), false);
                self.pending_destroy.insert(help.window().id());
                drop(help);
                for _ in 0..4 {
                    // Destroy the previous sibling before asking for the next one, all within
                    // one callback. Its Destroyed event must keep its original identity.
                    let child = popup(event_loop, &self.context, menu.window().as_ref(), true);
                    assert!(self.pending_destroy.insert(child.window().id()));
                }
                self.menu = Some(menu);
            },
            WindowEvent::Destroyed => {
                assert!(self.pending_destroy.remove(&id), "unexpected Destroyed for {id:?}");
                if self.pending_destroy.is_empty() {
                    event_loop.exit();
                }
            },
            _ => {},
        }
    }

    fn about_to_wait(&mut self, _: &dyn ActiveEventLoop) {
        assert!(
            Instant::now() < self.deadline,
            "timed out waiting for a click or Destroyed events"
        );
    }
}

#[test]
#[ignore = "requires a Wayland compositor and one pointer press in the test window"]
fn replace_grabbing_submenus_in_one_callback() {
    let event_loop = EventLoop::builder().with_wayland().with_any_thread(true).build().unwrap();
    let context = Context::new(event_loop.owned_display_handle()).unwrap();
    event_loop
        .run_app(Replacement {
            context,
            menu: None,
            toplevel: None,
            pending_destroy: HashSet::new(),
            deadline: Instant::now() + Duration::from_secs(30),
        })
        .unwrap();
}
