//! Native macOS regression checks for visibility versus keyboard focus.
//!
//! Run explicitly with `cargo run -p winit --example visibility_focus` on a Mac
//! desktop. The example opens test-owned windows and exits automatically. It
//! requires no keyboard injection, Accessibility permission or saved app state.
//! Ordinary test runs only compile this example and never open its windows.

#[cfg(macos_platform)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::time::{Duration, Instant};

    use winit::{
        application::ApplicationHandler,
        dpi::LogicalSize,
        event::WindowEvent,
        event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
        raw_window_handle::HasWindowHandle,
        window::{Window, WindowAttributes, WindowId, WindowType},
    };

    // Native activation may finish after the request returns. Pump the event
    // loop between focus transitions instead of assuming synchronous activation
    // inside AppKit's focus callback, sleeping, or injecting keyboard input.
    const FOCUS_TIMEOUT: Duration = Duration::from_secs(5);
    const CASES: [&str; 3] =
        ["inactive ordinary window", "inactive popup", "initially active window"];

    #[track_caller]
    fn check_visibility(owner: &dyn Window, child: &dyn Window, kind: &str) {
        assert!(owner.has_focus(), "owner must initially be key: {kind}");

        for _ in 0..2 {
            child.set_visible(true);
            assert_eq!(child.is_visible(), Some(true), "show: {kind}");
            assert!(owner.has_focus(), "show must retain owner focus: {kind}");
            assert!(!child.has_focus(), "show must not focus the child: {kind}");
        }

        child.set_visible(false);
        assert_eq!(child.is_visible(), Some(false), "hide: {kind}");
        child.set_visible(true);
        assert!(owner.has_focus(), "reshow must retain owner focus: {kind}");
        assert!(!child.has_focus(), "reshow must not focus the child: {kind}");
    }

    #[derive(Debug)]
    enum Step {
        Create,
        InitialFocus,
        RefocusOwner,
        CheckVisibility,
        ExplicitFocus,
        RestoreOwner,
    }

    struct Probe {
        owner: Option<Box<dyn Window>>,
        child: Option<Box<dyn Window>>,
        deadline: Instant,
        case: usize,
        step: Step,
    }

    impl ApplicationHandler for Probe {
        fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
            self.owner = Some(
                event_loop
                    .create_window(
                        WindowAttributes::default()
                            .with_title("winit visibility/focus regression")
                            .with_surface_size(LogicalSize::new(320.0, 180.0)),
                    )
                    .expect("native fixture owner"),
            );
            event_loop.set_control_flow(ControlFlow::WaitUntil(self.deadline));
        }

        fn window_event(
            &mut self,
            event_loop: &dyn ActiveEventLoop,
            _: WindowId,
            event: WindowEvent,
        ) {
            if matches!(event, WindowEvent::CloseRequested) {
                panic!("native visibility/focus check interrupted");
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(self.deadline));
        }

        fn about_to_wait(&mut self, event_loop: &dyn ActiveEventLoop) {
            let Some(owner) = &self.owner else { return };

            loop {
                assert!(
                    Instant::now() < self.deadline,
                    "focus transition timed out: {} / {:?}",
                    CASES[self.case],
                    self.step
                );

                match self.step {
                    Step::Create if owner.has_focus() => {
                        let initially_active = self.case == 2;
                        let mut attributes = WindowAttributes::default()
                            .with_title(CASES[self.case])
                            .with_surface_size(LogicalSize::new(160.0, 90.0))
                            .with_active(initially_active)
                            .with_visible(initially_active);

                        if self.case == 1 {
                            let parent = owner.window_handle().expect("live native owner").as_raw();
                            // SAFETY: The owner stays alive on this UI thread throughout the
                            // child window's lifetime. RestoreOwner releases the child first.
                            attributes = unsafe { attributes.with_parent_window(Some(parent)) }
                                .with_window_type(WindowType::Popup);
                        }

                        self.child =
                            Some(event_loop.create_window(attributes).expect("native child"));
                        self.step = if initially_active {
                            Step::InitialFocus
                        } else {
                            Step::CheckVisibility
                        };
                    },
                    Step::InitialFocus if self.child.as_ref().unwrap().has_focus() => {
                        self.child.as_ref().unwrap().set_visible(false);
                        owner.focus_window();
                        self.step = Step::RefocusOwner;
                    },
                    Step::RefocusOwner if owner.has_focus() => self.step = Step::CheckVisibility,
                    Step::CheckVisibility => {
                        let child = self.child.as_ref().unwrap();
                        check_visibility(owner.as_ref(), child.as_ref(), CASES[self.case]);
                        child.focus_window();
                        self.step = Step::ExplicitFocus;
                    },
                    Step::ExplicitFocus if self.child.as_ref().unwrap().has_focus() => {
                        owner.focus_window();
                        self.step = Step::RestoreOwner;
                    },
                    Step::RestoreOwner if owner.has_focus() => {
                        // AppKit's parent handle is borrowed: release child resources first.
                        drop(self.child.take());
                        self.case += 1;

                        if self.case == CASES.len() {
                            drop(self.owner.take());
                            event_loop.exit();
                            return;
                        }

                        self.step = Step::Create;
                    },
                    _ => break,
                }

                self.deadline = Instant::now() + FOCUS_TIMEOUT;
            }

            event_loop.set_control_flow(ControlFlow::WaitUntil(self.deadline));
        }
    }

    let event_loop = EventLoop::new()?;
    event_loop.run_app(Probe {
        owner: None,
        child: None,
        deadline: Instant::now() + FOCUS_TIMEOUT,
        case: 0,
        step: Step::Create,
    })?;
    Ok(())
}

#[cfg(not(macos_platform))]
fn main() {
    panic!("This native visibility/focus regression example requires macOS.");
}
