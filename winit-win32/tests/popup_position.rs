#![cfg(target_os = "windows")]

use rwh_06::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::Graphics::Gdi::{
    ClientToScreen, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::windows::EventLoopBuilderExtWindows;
use winit::window::{
    Window, WindowAnchor, WindowAttributes, WindowConstraintAdjustment, WindowGravity, WindowId,
    WindowPositioner, WindowType,
};

fn hwnd(window: &dyn Window) -> windows_sys::Win32::Foundation::HWND {
    let RawWindowHandle::Win32(handle) = window.window_handle().unwrap().as_raw() else {
        unreachable!()
    };
    handle.hwnd.get() as _
}

fn screen_origin(window: &dyn Window) -> PhysicalPosition<i32> {
    // Read native desktop coordinates: checking winit's parent-relative getter alone
    // could hide a matching error in the setter and getter.
    let mut origin = POINT { x: 0, y: 0 };
    assert_ne!(unsafe { ClientToScreen(hwnd(window), &mut origin) }, 0);
    PhysicalPosition::new(origin.x, origin.y)
}

fn popup(el: &dyn ActiveEventLoop, parent: &dyn Window, constrained: bool) -> Box<dyn Window> {
    let constraints = if constrained {
        WindowConstraintAdjustment::FLIP_X
            | WindowConstraintAdjustment::FLIP_Y
            | WindowConstraintAdjustment::SLIDE_X
            | WindowConstraintAdjustment::SLIDE_Y
    } else {
        WindowConstraintAdjustment::empty()
    };
    let attributes = WindowAttributes::default()
        .with_visible(false)
        .with_active(false)
        .with_decorations(false)
        .with_surface_size(LogicalSize::new(180.0, 100.0))
        .with_window_type(WindowType::Popup)
        .with_positioner(WindowPositioner::new(
            WindowAnchor::TopLeft,
            (LogicalPosition::new(20.0, 20.0).into(), LogicalSize::new(1.0, 1.0).into()),
            LogicalPosition::new(0.0, 0.0).into(),
            WindowGravity::BottomRight,
            constraints,
        ));
    // Every caller keeps the parent alive until after the returned popup is dropped.
    let attributes =
        unsafe { attributes.with_parent_window(Some(parent.window_handle().unwrap().as_raw())) };
    el.create_window(attributes).unwrap()
}

fn assert_anchor(parent: &dyn Window, child: &dyn Window) {
    let origin = screen_origin(parent);
    let offset = LogicalPosition::new(20.0, 20.0).to_physical::<i32>(parent.scale_factor());
    assert_eq!(
        screen_origin(child),
        PhysicalPosition::new(origin.x + offset.x, origin.y + offset.y),
        "popup placement changed monitor or used the wrong DPI (parent scale: {}, popup scale: {})",
        parent.scale_factor(),
        child.scale_factor(),
    );
    assert_eq!(
        child.surface_size(),
        LogicalSize::new(180.0, 100.0).to_physical(child.scale_factor())
    );
}

// Run explicitly on a desktop with multiple monitors, ideally with different scales
// and a monitor left of the primary display. Hidden, inactive windows exercise real
// Win32 placement without flashing windows or taking focus from the user's app.
#[test]
#[ignore = "requires an interactive Windows desktop; use multiple monitors to cover the regression"]
fn popup_placement_across_monitors() {
    struct Harness;
    impl ApplicationHandler for Harness {
        fn can_create_surfaces(&mut self, el: &dyn ActiveEventLoop) {
            let monitors: Vec<_> = el.available_monitors().collect();
            assert!(!monitors.is_empty());
            for monitor in &monitors {
                let position = monitor.position().unwrap();
                let parent = el
                    .create_window(
                        WindowAttributes::default()
                            .with_visible(false)
                            .with_active(false)
                            .with_decorations(false)
                            .with_surface_size(PhysicalSize::new(400, 300))
                            .with_position(PhysicalPosition::new(
                                position.x + 200,
                                position.y + 200,
                            )),
                    )
                    .unwrap();
                for constrained in [false, true] {
                    let child = popup(el, parent.as_ref(), constrained);
                    assert_anchor(parent.as_ref(), child.as_ref());
                    let grandchild = popup(el, child.as_ref(), constrained);
                    assert_anchor(child.as_ref(), grandchild.as_ref());
                }
                let child = popup(el, parent.as_ref(), true);
                let grandchild = popup(el, child.as_ref(), true);
                for destination in &monitors {
                    let position = destination.position().unwrap();
                    parent.set_outer_position(
                        PhysicalPosition::new(position.x + 300, position.y + 300).into(),
                    );
                    assert_anchor(parent.as_ref(), child.as_ref());
                    assert_anchor(child.as_ref(), grandchild.as_ref());
                }
                drop(grandchild);
                drop(child);

                // Edge adjustment must still keep the popup inside the selected display's
                // work area, including its taskbar, rather than simply disabling constraints.
                let handle =
                    unsafe { MonitorFromWindow(hwnd(parent.as_ref()), MONITOR_DEFAULTTONEAREST) };
                let mut info = MONITORINFO {
                    cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                assert_ne!(unsafe { GetMonitorInfoW(handle, &mut info) }, 0);
                parent.set_outer_position(
                    PhysicalPosition::new(info.rcWork.right - 60, info.rcWork.bottom - 60).into(),
                );
                let child = popup(el, parent.as_ref(), true);
                let origin = screen_origin(child.as_ref());
                let size = child.outer_size();
                assert!(origin.x >= info.rcWork.left && origin.y >= info.rcWork.top);
                assert!(origin.x + size.width as i32 <= info.rcWork.right);
                assert!(origin.y + size.height as i32 <= info.rcWork.bottom);
            }
            el.exit();
        }
        fn window_event(&mut self, _: &dyn ActiveEventLoop, _: WindowId, _: WindowEvent) {}
    }
    EventLoop::builder().with_any_thread(true).build().unwrap().run_app(Harness).unwrap();
}
