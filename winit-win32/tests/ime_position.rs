//! Native cursor-area persistence across input-context resets and IME sessions.
//!
//! Windows may discard candidate placement when the input method changes. The
//! client has already provided its rectangle; native activation must restore it
//! before the client receives composition events. No keyboard input is injected.

#![cfg(target_os = "windows")]

use std::cell::Cell;
use std::rc::Rc;

use rwh_06::{HasWindowHandle, RawWindowHandle};
use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Input::Ime::{
    CANDIDATEFORM, CFS_DEFAULT, CFS_EXCLUDE, CFS_POINT, COMPOSITIONFORM, HIMC,
    ImmGetCandidateWindow, ImmGetCompositionWindow, ImmGetContext, ImmReleaseContext,
    ImmSetCandidateWindow, ImmSetCompositionWindow,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    SendMessageW, WM_IME_ENDCOMPOSITION, WM_IME_STARTCOMPOSITION,
};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::platform::windows::EventLoopBuilderExtWindows;
use winit::window::{
    ImeCapabilities, ImeEnableRequest, ImeRequest, ImeRequestData, Window, WindowAttributes,
    WindowId,
};

/// The hidden test window outlives this UI-thread context borrow. Releasing it
/// before sending a window message also keeps teardown and reentry explicit.
struct Context {
    hwnd: HWND,
    himc: HIMC,
}

impl Context {
    #[track_caller]
    fn new(hwnd: HWND) -> Self {
        // SAFETY: The fixture keeps this same-thread native window alive until
        // after the returned context is released. IMM retains no Rust borrow.
        let himc = unsafe { ImmGetContext(hwnd) };
        assert!(!himc.is_null(), "the fixture requires an installed Windows IME");
        Self { hwnd, himc }
    }

    #[track_caller]
    fn composition(&self) -> COMPOSITIONFORM {
        // SAFETY: IMM writes synchronously into this live POD output buffer
        // through the same valid UI-thread context as the candidate query.
        unsafe {
            let mut form = std::mem::zeroed();
            assert_ne!(ImmGetCompositionWindow(self.himc, &mut form), 0);
            form
        }
    }

    #[track_caller]
    fn candidate(&self) -> CANDIDATEFORM {
        // SAFETY: This valid native context writes into a live, correctly sized
        // POD form without retaining its pointer. Zero is valid for all fields.
        unsafe {
            let mut form = std::mem::zeroed();
            assert_ne!(ImmGetCandidateWindow(self.himc, 0, &mut form), 0);
            form
        }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        // SAFETY: This releases exactly the borrow acquired for the still-live
        // window by Context::new, on the same UI thread.
        unsafe { ImmReleaseContext(self.hwnd, self.himc) };
    }
}

#[track_caller]
fn hwnd(window: &dyn Window) -> HWND {
    let RawWindowHandle::Win32(handle) = window.window_handle().unwrap().as_raw() else {
        unreachable!("this fixture runs only on Win32")
    };
    handle.hwnd.get() as HWND
}

#[track_caller]
fn discard_ime_placement(window: &dyn Window) {
    let context = Context::new(hwnd(window));
    let mut form = context.candidate();
    form.dwStyle = CFS_DEFAULT;
    // SAFETY: The live UI-thread context reads this POD form synchronously. This
    // reproduces the native state loss observed on Japanese layout activation.
    assert_ne!(unsafe { ImmSetCandidateWindow(context.himc, &form) }, 0);
    assert_eq!(context.candidate().dwStyle, CFS_DEFAULT);
    let mut composition = context.composition();
    composition.dwStyle = CFS_DEFAULT;
    // SAFETY: IMM reads this local form synchronously through the fixture's
    // live context. Discard both native forms before testing their restoration.
    assert_ne!(unsafe { ImmSetCompositionWindow(context.himc, &composition) }, 0);
    assert_eq!(context.composition().dwStyle, CFS_DEFAULT);
}

#[track_caller]
fn start_composition(window: &dyn Window) {
    // SAFETY: Send to only the fixture's own live HWND on its UI thread. This
    // parameterless notification reaches winit's real native window procedure.
    unsafe { SendMessageW(hwnd(window), WM_IME_STARTCOMPOSITION, 0, 0) };
}

#[track_caller]
fn end_composition(window: &dyn Window) {
    // SAFETY: Pair each fixture-generated start notification on its same live
    // native window, with no application/native pointers retained by the call.
    unsafe { SendMessageW(hwnd(window), WM_IME_ENDCOMPOSITION, 0, 0) };
}

#[track_caller]
fn assert_cursor(window: &dyn Window, position: LogicalPosition<f64>, size: LogicalSize<f64>) {
    let context = Context::new(hwnd(window));
    let form = context.candidate();
    let composition = context.composition();
    let position = position.to_physical::<i32>(window.scale_factor());
    let size = size.to_physical::<i32>(window.scale_factor());
    assert_eq!(form.dwStyle, CFS_EXCLUDE, "native activation lost the accepted cursor area");
    assert_eq!(composition.dwStyle, CFS_POINT);
    assert_eq!(
        (composition.ptCurrentPos.x, composition.ptCurrentPos.y),
        (position.x, position.y + size.height)
    );
    assert_eq!((form.ptCurrentPos.x, form.ptCurrentPos.y), (position.x, position.y));
    assert_eq!(
        (form.rcArea.left, form.rcArea.top, form.rcArea.right, form.rcArea.bottom),
        (position.x, position.y, position.x + size.width, position.y + size.height)
    );
}

#[test]
#[ignore = "requires a Windows desktop and installed IME; creates a hidden inactive native window"]
fn native_activation_restores_the_last_requested_cursor_area() {
    struct Harness(Rc<Cell<bool>>);

    impl ApplicationHandler for Harness {
        fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
            let window = event_loop
                .create_window(WindowAttributes::default().with_visible(false).with_active(false))
                .unwrap();
            let initial = LogicalPosition::new(120.0, 80.0);
            let updated = LogicalPosition::new(220.0, 110.0);
            let size = LogicalSize::new(2.0, 24.0);
            window
                .request_ime_update(ImeRequest::Enable(
                    ImeEnableRequest::new(
                        ImeCapabilities::new().with_cursor_area(),
                        ImeRequestData::default().with_cursor_area(initial.into(), size.into()),
                    )
                    .unwrap(),
                ))
                .unwrap();
            assert_cursor(window.as_ref(), initial, size);

            discard_ime_placement(window.as_ref());
            start_composition(window.as_ref());
            assert_cursor(window.as_ref(), initial, size);
            end_composition(window.as_ref());

            // A request updating another property must not discard retained
            // geometry, and a later cursor request must replace the old one.
            window.request_ime_update(ImeRequest::Update(ImeRequestData::default())).unwrap();
            discard_ime_placement(window.as_ref());
            start_composition(window.as_ref());
            assert_cursor(window.as_ref(), initial, size);
            end_composition(window.as_ref());
            window
                .request_ime_update(ImeRequest::Update(
                    ImeRequestData::default().with_cursor_area(updated.into(), size.into()),
                ))
                .unwrap();
            discard_ime_placement(window.as_ref());
            start_composition(window.as_ref());
            assert_cursor(window.as_ref(), updated, size);
            end_composition(window.as_ref());

            // A new requested session without cursor support cannot resurrect
            // geometry belonging to the disabled one.
            window.request_ime_update(ImeRequest::Disable).unwrap();
            window
                .request_ime_update(ImeRequest::Enable(
                    ImeEnableRequest::new(ImeCapabilities::new(), ImeRequestData::default())
                        .unwrap(),
                ))
                .unwrap();
            discard_ime_placement(window.as_ref());
            start_composition(window.as_ref());
            assert_eq!(Context::new(hwnd(window.as_ref())).candidate().dwStyle, CFS_DEFAULT);
            end_composition(window.as_ref());

            self.0.set(true);
            event_loop.exit();
        }

        fn window_event(&mut self, _: &dyn ActiveEventLoop, _: WindowId, _: WindowEvent) {}
    }

    let complete = Rc::new(Cell::new(false));
    let event_loop = EventLoop::builder().with_any_thread(true).build().unwrap();
    event_loop.run_app(Harness(Rc::clone(&complete))).unwrap();
    assert!(complete.get(), "the native assertions must execute before the loop exits");
}
