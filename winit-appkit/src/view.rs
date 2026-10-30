//! Owns AppKit view input, native gestures and surface notifications.
//! Window policy and application text storage remain with callers; view methods run
//! on the main thread and must not retain borrows across native or application callbacks.

#![allow(clippy::unnecessary_cast)]
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use dpi::{LogicalPosition, PhysicalSize};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{AnyThread, DefinedClass, MainThreadMarker, Message, define_class, msg_send};
use objc2_app_kit::{
    NSApplication, NSCursor, NSDragOperation, NSDraggingSession, NSEvent, NSEventPhase,
    NSEventType, NSResponder, NSTextInputClient, NSTrackingArea, NSTrackingAreaOptions, NSView,
    NSViewLayerContentsRedrawPolicy, NSWindow, NSWindowStyleMask,
};
use objc2_core_foundation::CGRect;
use objc2_foundation::{
    NSArray, NSAttributedString, NSAttributedStringKey, NSCopying, NSMutableAttributedString,
    NSNotFound, NSObject, NSPoint, NSRange, NSRect, NSSize, NSString, NSUInteger,
};
use tracing::{debug_span, trace, trace_span};
use winit_core::{
    error::NotSupportedError,
    event::{
        DeviceEvent, ElementState, Ime, KeyEvent, Modifiers, MouseButton, MouseScrollDelta,
        PointerKind, PointerSource, TouchPhase, WindowEvent,
    },
    keyboard::{Key, KeyCode, KeyLocation, ModifiersState, NamedKey},
    window::{ImeCapabilities, ResizeDirection},
};

use crate::{
    OptionAsAlt,
    app_state::AppState,
    cursor::{default_cursor, invisible_cursor},
    event::{
        code_to_key, code_to_location, create_key_event, event_mods, lalt_pressed, ralt_pressed,
        scancode_to_physicalkey,
    },
    resize::ResizeDrag,
    window::window_id,
};

// NSEvent::pressedMouseButtons assigns bit zero to the primary button.
const PRIMARY_BUTTON_MASK: usize = 1;

#[derive(Debug)]
struct CursorState {
    visible: bool,
    cursor: Retained<NSCursor>,
}

impl Default for CursorState {
    fn default() -> Self {
        Self { visible: true, cursor: default_cursor() }
    }
}

#[derive(Debug, Eq, PartialEq, Clone, Copy, Default)]
enum ImeState {
    #[default]
    /// No IME session has been announced; ordinary text uses `KeyboardInput`.
    Disabled,

    /// The ground state of enabled IME input. It means that both Preedit and regular keyboard
    /// input could be start from it.
    Ground,

    /// The IME is in preedit.
    Preedit,

    /// Text was committed while interpreting the current key, which must be consumed.
    Committed,
}

/// Tracks the synchronous key interpretation that can produce an `insertText:` callback.
///
/// AppKit also inserts text independently of a key event, for example from the character
/// palette. Such insertions have no `KeyboardInput` event to carry their text and must not
/// depend on a preceding marked-text session.
#[derive(Debug, Default)]
struct TextInput {
    state: Cell<ImeState>,
    /// Retains a composition commit even if AppKit calls unmarkText: first.
    pending_commit: Cell<bool>,
    interpreting_key: Cell<bool>,
}

impl TextInput {
    /// Return whether this insertion should be delivered as an IME commit, updating the
    /// state before application callbacks can run. Ordinary key text is delivered by
    /// `KeyboardInput`; a composition consumes its confirming key instead.
    fn insert(&self, requested: bool, text: &str) -> bool {
        if !requested || text.chars().next().is_some_and(char::is_control) {
            return false;
        }

        let interpreting_key = self.interpreting_key.get();
        if interpreting_key
            && (!self.pending_commit.get() || self.state.get() == ImeState::Disabled)
        {
            return false;
        }

        self.pending_commit.set(false);
        self.state.set(if interpreting_key { ImeState::Committed } else { ImeState::Ground });
        true
    }

    fn interpret_key(&self, interpret: impl FnOnce()) {
        // AppKit calls the text-input methods synchronously from interpretKeyEvents:.
        // Restore the enclosing scope if a native callback reenters key handling, and do
        // not hold a RefCell borrow across AppKit or application callbacks.
        struct Restore<'a> {
            cell: &'a Cell<bool>,
            previous: bool,
        }

        impl Drop for Restore<'_> {
            fn drop(&mut self) {
                self.cell.set(self.previous);
            }
        }

        let _restore =
            Restore { cell: &self.interpreting_key, previous: self.interpreting_key.replace(true) };
        interpret();
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq)]
    struct ModLocationMask: u8 {
        const LEFT     = 0b0001;
        const RIGHT    = 0b0010;
    }
}
impl ModLocationMask {
    fn from_location(loc: KeyLocation) -> ModLocationMask {
        match loc {
            KeyLocation::Left => ModLocationMask::LEFT,
            KeyLocation::Right => ModLocationMask::RIGHT,
            _ => unreachable!(),
        }
    }
}

fn key_to_modifier(key: &Key) -> Option<ModifiersState> {
    match key {
        Key::Named(NamedKey::Alt) => Some(ModifiersState::ALT),
        Key::Named(NamedKey::Control) => Some(ModifiersState::CONTROL),
        Key::Named(NamedKey::Meta) => Some(ModifiersState::META),
        Key::Named(NamedKey::Shift) => Some(ModifiersState::SHIFT),
        _ => None,
    }
}

fn get_right_modifier_code(key: &Key) -> KeyCode {
    match key {
        Key::Named(NamedKey::Alt) => KeyCode::AltRight,
        Key::Named(NamedKey::Control) => KeyCode::ControlRight,
        Key::Named(NamedKey::Shift) => KeyCode::ShiftRight,
        Key::Named(NamedKey::Meta) => KeyCode::MetaRight,
        _ => unreachable!(),
    }
}

fn get_left_modifier_code(key: &Key) -> KeyCode {
    match key {
        Key::Named(NamedKey::Alt) => KeyCode::AltLeft,
        Key::Named(NamedKey::Control) => KeyCode::ControlLeft,
        Key::Named(NamedKey::Shift) => KeyCode::ShiftLeft,
        Key::Named(NamedKey::Meta) => KeyCode::MetaLeft,
        _ => unreachable!(),
    }
}

#[derive(Debug)]
pub struct ViewState {
    /// Strong reference to the global application state.
    app_state: Rc<AppState>,

    /// An explicit policy for winit's view, independent of the panel's ability
    /// to become key. None preserves AppKit's inherited behavior until a caller
    /// supplies an override; all reads and writes stay on the UI thread.
    needs_panel_to_become_key: Cell<Option<bool>>,

    /// This is for a dragging session that we initiated
    dragging_session: RefCell<Option<Retained<NSDraggingSession>>>,

    /// This is for a dragging session that we initiated
    drag_operations: Cell<NSDragOperation>,

    /// Borderless resize tracking is confined to this view. Copying the geometry
    /// out of the cell keeps native frame callbacks free to reenter the view.
    resize_drag: Cell<Option<ResizeDrag>>,

    cursor_state: RefCell<CursorState>,
    ime_position: Cell<NSPoint>,
    ime_size: Cell<NSSize>,
    modifiers: Cell<Modifiers>,
    phys_modifiers: RefCell<HashMap<Key, ModLocationMask>>,
    text_input: TextInput,
    input_source: RefCell<String>,

    /// True iff the application wants IME events.
    ///
    /// Can be set using `set_ime_allowed`
    ime_capabilities: Cell<Option<ImeCapabilities>>,

    /// True if the current key event should be forwarded
    /// to the application, even during IME
    forward_key_to_app: Cell<bool>,
    marked_text: RefCell<Retained<NSMutableAttributedString>>,

    /// Set by `acceptsFirstMouse:` so the next `mouseDown:` can tag its
    /// [`WindowEvent::PointerButton`] with `is_macos_activation_click: true`.
    next_click_is_activation: Cell<bool>,

    /// The state of the `Option` as `Alt`.
    option_as_alt: Cell<OptionAsAlt>,
}

define_class!(
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[ivars = ViewState]
    #[name = "WinitView"]
    #[derive(Debug)]
    pub(super) struct WinitView;

    /// This documentation attribute makes rustfmt work for some reason?
    impl WinitView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            // `winit` uses the upper-left corner as the origin.
            true
        }

        // Not a normal method on `NSView`, it's triggered by `NSViewFrameDidChangeNotification`.
        #[unsafe(method(viewFrameDidChangeNotification:))]
        fn frame_did_change(&self, _notification: Option<&AnyObject>) {
            let _entered = debug_span!("NSViewFrameDidChangeNotification").entered();

            // Emit resize event here rather than from windowDidResize because:
            // 1. When a new window is created as a tab, the frame size may change without a window
            //    resize occurring.
            // 2. Even when a window resize does occur on a new tabbed window, it contains the wrong
            //    size (includes tab height).
            self.surface_resized();
            // During live resize, AppKit may not let the normal event loop reach its next redraw
            // point before stretching the current layer contents. Redraw immediately after the
            // app has observed the new surface size.
            self.redraw_during_live_resize();
        }

        #[unsafe(method(viewDidChangeBackingProperties))]
        fn view_did_change_backing_properties(&self) {
            let _entered = debug_span!("viewDidChangeBackingProperties").entered();
            // Moving between displays or changing scale can alter the drawable backing size
            // without a matching frame-size change.
            self.surface_resized();
            self.redraw_during_live_resize();
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _rect: NSRect) {
            let _entered = debug_span!("drawRect:").entered();

            self.ivars().app_state.handle_redraw(window_id(&self.window()));

            // This is a direct subclass of NSView, no need to call superclass' drawRect:
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            let _entered = trace_span!("acceptsFirstResponder").entered();
            true
        }

        // AppKit consults the hit view for conditional panel activation and
        // first-responder-on-click policy. Winit installs this private view inside
        // a plain contentView wrapper; changing the wrapper cannot override this
        // read-only method. On macOS 27.2, the inherited response on WinitView was
        // true while the plain wrapper reported false, despite NSView's documented
        // default being false. Preserve the actual inherited response rather than
        // assuming a constant default for every system and view configuration.
        //
        // Changing canBecomeKeyWindow instead would also prevent native keyboard
        // controls and explicit focus requests from making the panel key.
        // https://developer.apple.com/documentation/appkit/nsview/needspaneltobecomekey
        // https://developer.apple.com/documentation/appkit/nspanel/becomeskeyonlyifneeded
        // https://developer.apple.com/documentation/appkit/nswindow/canbecomekey
        #[unsafe(method(needsPanelToBecomeKey))]
        fn panel_needs_key(&self) -> bool {
            let _entered = trace_span!("needsPanelToBecomeKey").entered();

            if let Some(needs_key) = self.ivars().needs_panel_to_become_key.get() {
                needs_key
            } else {
                // SAFETY: This live main-thread NSView forwards the same query
                // synchronously to its superclass without changing ownership.
                unsafe { msg_send![super(self), needsPanelToBecomeKey] }
            }
        }

        // This is necessary to prevent a beefy terminal error on MacBook Pros:
        // IMKInputSession [0x7fc573576ff0
        // presentFunctionRowItemTextInputViewWithEndpoint:completionHandler:] : [self
        // textInputContext]=0x7fc573558e10 *NO* NSRemoteViewController to client, NSError=Error
        // Domain=NSCocoaErrorDomain Code=4099 "The connection from pid 0 was invalidated from this
        // process." UserInfo={NSDebugDescription=The connection from pid 0 was invalidated from
        // this process.}, com.apple.inputmethod.EmojiFunctionRowItem TODO: Add an API
        // extension for using `NSTouchBar`
        #[unsafe(method_id(touchBar))]
        fn touch_bar(&self) -> Option<Retained<NSObject>> {
            let _entered = debug_span!("touchBar").entered();
            None
        }

        #[unsafe(method(resetCursorRects))]
        fn reset_cursor_rects(&self) {
            let _entered = debug_span!("resetCursorRects").entered();
            let bounds = self.bounds();
            let cursor_state = self.ivars().cursor_state.borrow();
            // We correctly invoke `addCursorRect` only from inside `resetCursorRects`
            if cursor_state.visible {
                self.addCursorRect_cursor(bounds, &cursor_state.cursor);
            } else {
                self.addCursorRect_cursor(bounds, &invisible_cursor());
            }
        }
    }

    unsafe impl NSTextInputClient for WinitView {
        #[unsafe(method(hasMarkedText))]
        fn has_marked_text(&self) -> bool {
            let _entered = debug_span!("hasMarkedText").entered();
            self.ivars().marked_text.borrow().length() > 0
        }

        #[unsafe(method(markedRange))]
        fn marked_range(&self) -> NSRange {
            let _entered = debug_span!("markedRange").entered();
            let length = self.ivars().marked_text.borrow().length();
            if length > 0 {
                NSRange::new(0, length)
            } else {
                // Documented to return `{NSNotFound, 0}` if there is no marked range.
                NSRange::new(NSNotFound as NSUInteger, 0)
            }
        }

        #[unsafe(method(selectedRange))]
        fn selected_range(&self) -> NSRange {
            let _entered = debug_span!("selectedRange").entered();
            // Documented to return `{NSNotFound, 0}` if there is no selection.
            NSRange::new(NSNotFound as NSUInteger, 0)
        }

        #[unsafe(method(setMarkedText:selectedRange:replacementRange:))]
        fn set_marked_text(
            &self,
            string: &NSObject,
            selected_range: NSRange,
            _replacement_range: NSRange,
        ) {
            // TODO: Use _replacement_range, requires changing the event to report surrounding text.
            let _entered = debug_span!("setMarkedText:selectedRange:replacementRange:").entered();

            let (marked_text, string) = if let Some(string) =
                string.downcast_ref::<NSAttributedString>()
            {
                (NSMutableAttributedString::from_attributed_nsstring(string), string.string())
            } else if let Some(string) = string.downcast_ref::<NSString>() {
                (NSMutableAttributedString::from_nsstring(string), string.copy())
            } else {
                // This method is guaranteed to get either a `NSString` or a `NSAttributedString`.
                panic!("unexpected text {string:?}")
            };

            // Update marked text.
            *self.ivars().marked_text.borrow_mut() = marked_text;

            // Notify IME is active if application still doesn't know it.
            if self.ivars().text_input.state.get() == ImeState::Disabled {
                *self.ivars().input_source.borrow_mut() = self.current_input_source();
                self.queue_event(WindowEvent::Ime(Ime::Enabled));
            }

            if self.hasMarkedText() {
                self.ivars().text_input.state.set(ImeState::Preedit);
            } else {
                // In case the preedit was cleared, set IME into the Ground state.
                self.ivars().text_input.state.set(ImeState::Ground);
            }
            self.ivars().text_input.pending_commit.set(true);

            let string = string.to_string();
            let cursor_range = if string.is_empty() {
                // An empty string basically means that there's no preedit, so indicate that by
                // sending a `None` cursor range.
                None
            } else {
                // Convert the selected range from UTF-16 code unit indices to UTF-8 byte
                // offsets. `utf16_to_utf8_offset` is defensive: it snaps an offset that would
                // split a surrogate pair down to the character boundary and clamps an
                // out-of-bounds offset to the string length, so no `NSRangeException` is
                // possible and the resulting range can never be inverted (`lower <= upper`).
                // IMEs are known to send both mid-surrogate and out-of-bounds offsets (e.g.
                // native Pinyin, see https://github.com/alacritty/alacritty/issues/8791).
                let lowerbound_utf8 = utf16_to_utf8_offset(&string, selected_range.location);
                let upperbound_utf8 = utf16_to_utf8_offset(&string, selected_range.end());
                Some((lowerbound_utf8, upperbound_utf8))
            };

            // Send WindowEvent for updating marked text
            self.queue_event(WindowEvent::Ime(Ime::Preedit(string, cursor_range)));
        }

        #[unsafe(method(unmarkText))]
        fn unmark_text(&self) {
            let _entered = debug_span!("unmarkText").entered();
            *self.ivars().marked_text.borrow_mut() = NSMutableAttributedString::new();

            let input_context = self.inputContext().expect("input context");
            input_context.discardMarkedText();

            self.queue_event(WindowEvent::Ime(Ime::Preedit(String::new(), None)));
            if self.is_ime_enabled() {
                // Leave the Preedit self.ivars()
                self.ivars().text_input.state.set(ImeState::Ground);
            } else {
                tracing::warn!("Expected to have IME enabled when receiving unmarkText");
            }
        }

        #[unsafe(method_id(validAttributesForMarkedText))]
        fn valid_attributes_for_marked_text(&self) -> Retained<NSArray<NSAttributedStringKey>> {
            let _entered = trace_span!("validAttributesForMarkedText").entered();
            let underline_style = NSString::from_str("NSUnderlineStyle");
            let marked_clause = NSString::from_str("NSMarkedClauseSegment");
            NSArray::from_slice(&[&*underline_style, &*marked_clause])
        }

        #[unsafe(method_id(attributedSubstringForProposedRange:actualRange:))]
        fn attributed_substring_for_proposed_range(
            &self,
            _range: NSRange,
            _actual_range: *mut NSRange,
        ) -> Option<Retained<NSAttributedString>> {
            let _entered =
                trace_span!("attributedSubstringForProposedRange:actualRange:").entered();
            None
        }

        #[unsafe(method(characterIndexForPoint:))]
        fn character_index_for_point(&self, _point: NSPoint) -> NSUInteger {
            let _entered = debug_span!("characterIndexForPoint:").entered();
            0
        }

        #[unsafe(method(firstRectForCharacterRange:actualRange:))]
        fn first_rect_for_character_range(
            &self,
            _range: NSRange,
            _actual_range: *mut NSRange,
        ) -> NSRect {
            let _entered = debug_span!("firstRectForCharacterRange:actualRange:").entered();

            // Guard when the view is no longer in a window during teardown.
            let Some(window) = (**self).window() else {
                return CGRect::ZERO;
            };

            // Return value is expected to be in screen coordinates, so we need a conversion
            let rect = NSRect::new(self.ivars().ime_position.get(), self.ivars().ime_size.get());
            let view_rect = self.convertRect_toView(rect, None);
            window.convertRectToScreen(view_rect)
        }

        #[unsafe(method(insertText:replacementRange:))]
        fn insert_text(&self, string: &NSObject, _replacement_range: NSRange) {
            // TODO: Use _replacement_range, requires changing the event to report surrounding text.
            let _entered = debug_span!("insertText:replacementRange:").entered();

            let string = if let Some(string) = string.downcast_ref::<NSAttributedString>() {
                string.string().to_string()
            } else if let Some(string) = string.downcast_ref::<NSString>() {
                string.to_string()
            } else {
                // This method is guaranteed to get either a `NSString` or a `NSAttributedString`.
                panic!("unexpected text {string:?}")
            };

            let has_marked = self.hasMarkedText();
            let pending_commit = self.ivars().text_input.pending_commit.get();
            let ime_enabled = self.is_ime_enabled();
            let ime_requested = self.ivars().ime_capabilities.get().is_some();

            trace!(
                has_marked,
                pending_commit,
                ime_enabled,
                ime_requested,
                interpreting_key = self.ivars().text_input.interpreting_key.get(),
                "Native text insertion"
            );

            if !self.ivars().text_input.insert(ime_requested, &string) {
                return;
            }

            // Key interpretation clears marked text in keyDown:'s epilogue, after AppKit
            // has finished its callbacks. A standalone insertion has no such epilogue.
            if !self.ivars().text_input.interpreting_key.get() {
                *self.ivars().marked_text.borrow_mut() = NSMutableAttributedString::new();
            }

            // The character palette can insert without ever starting marked text. Announce
            // the session before committing, just as setMarkedText: does for composition.
            if !ime_enabled {
                *self.ivars().input_source.borrow_mut() = self.current_input_source();
                self.queue_event(WindowEvent::Ime(Ime::Enabled));
            }

            // Every commit clears the application's preedit, including when no marked
            // text preceded it, as required by the Ime::Commit event contract.
            self.queue_event(WindowEvent::Ime(Ime::Preedit(String::new(), None)));
            self.queue_event(WindowEvent::Ime(Ime::Commit(string)));
        }

        // Basically, we're sent this message whenever a keyboard event that doesn't generate a
        // "human readable" character happens, i.e. newlines, tabs, and Ctrl+C.
        #[unsafe(method(doCommandBySelector:))]
        fn do_command_by_selector(&self, command: Sel) {
            let _entered = debug_span!("doCommandBySelector:").entered();

            // We shouldn't forward any character from just committed text, since we'll end up
            // sending it twice with some IMEs like Korean one. We'll also always send
            // `Enter` in that case, which is not desired given it was used to confirm
            // IME input.
            if self.ivars().text_input.state.get() == ImeState::Committed {
                return;
            }

            self.ivars().forward_key_to_app.set(true);

            if self.hasMarkedText() && self.ivars().text_input.state.get() == ImeState::Preedit {
                // Leave preedit so that we also report the key-up for this key.
                self.ivars().text_input.state.set(ImeState::Ground);
            }

            // Send command action to user if they requested it.
            let window_id = window_id(&self.window());
            self.ivars().app_state.maybe_queue_with_handler(move |app, event_loop| {
                if let Some(handler) = app.macos_handler() {
                    handler.standard_key_binding(
                        event_loop,
                        window_id,
                        command.name().to_str().unwrap(),
                    );
                }
            });

            // The documentation for `-[NSTextInputClient doCommandBySelector:]` clearly states that
            // we should not be forwarding this event up the responder chain, so no calling `super`
            // here either.
        }
    }

    /// This documentation attribute makes rustfmt work for some reason?
    impl WinitView {
        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            let _entered = debug_span!("keyDown:").entered();
            {
                let mut prev_input_source = self.ivars().input_source.borrow_mut();
                let current_input_source = self.current_input_source();
                if *prev_input_source != current_input_source && self.is_ime_enabled() {
                    *prev_input_source = current_input_source;
                    drop(prev_input_source);
                    self.ivars().text_input.state.set(ImeState::Disabled);
                    self.queue_event(WindowEvent::Ime(Ime::Disabled));
                }
            }

            // Get the characters from the event.
            let old_ime_state = self.ivars().text_input.state.get();
            self.ivars().forward_key_to_app.set(false);
            let event = replace_event(event, self.option_as_alt());

            // The `interpretKeyEvents` function might call
            // `setMarkedText`, `insertText`, and `doCommandBySelector`.
            // It's important that we call this before queuing the KeyboardInput, because
            // we must send the `KeyboardInput` event during IME if it triggered
            // `doCommandBySelector`. (doCommandBySelector means that the keyboard input
            // is not handled by IME and should be handled by the application)
            if self.ivars().ime_capabilities.get().is_some() {
                let events_for_nsview = NSArray::from_slice(&[&*event]);
                self.ivars().text_input.interpret_key(|| {
                    self.interpretKeyEvents(&events_for_nsview);
                });

                // Wait for all callbacks from AppKit before clearing the marked text.
                if self.ivars().text_input.state.get() == ImeState::Committed {
                    *self.ivars().marked_text.borrow_mut() = NSMutableAttributedString::new();
                }
            }

            self.update_modifiers(&event, false);

            let had_ime_input = match self.ivars().text_input.state.get() {
                ImeState::Committed => {
                    // Allow normal input after the commit.
                    self.ivars().text_input.state.set(ImeState::Ground);
                    true
                },
                ImeState::Preedit => true,
                // `key_down` could result in preedit clear, so compare old and current state.
                _ => old_ime_state != self.ivars().text_input.state.get(),
            };

            if !had_ime_input || self.ivars().forward_key_to_app.get() {
                let key_event = create_key_event(&event, true, event.isARepeat());
                self.queue_event(WindowEvent::KeyboardInput {
                    device_id: None,
                    event: key_event,
                    is_synthetic: false,
                });
            }
        }

        #[unsafe(method(keyUp:))]
        fn key_up(&self, event: &NSEvent) {
            let _entered = debug_span!("keyUp:").entered();

            let event = replace_event(event, self.option_as_alt());
            self.update_modifiers(&event, false);

            // We want to send keyboard input when we are currently in the ground state.
            if matches!(self.ivars().text_input.state.get(), ImeState::Ground | ImeState::Disabled)
            {
                self.queue_event(WindowEvent::KeyboardInput {
                    device_id: None,
                    event: create_key_event(&event, false, false),
                    is_synthetic: false,
                });
            }
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, event: &NSEvent) {
            let _entered = debug_span!("flagsChanged:").entered();

            self.update_modifiers(event, true);
        }

        #[unsafe(method(insertTab:))]
        fn insert_tab(&self, _sender: Option<&AnyObject>) {
            let _entered = debug_span!("insertTab:").entered();
            let window = self.window();
            if let Some(first_responder) = window.firstResponder() {
                if *first_responder == ***self {
                    window.selectNextKeyView(Some(self))
                }
            }
        }

        #[unsafe(method(insertBackTab:))]
        fn insert_back_tab(&self, _sender: Option<&AnyObject>) {
            let _entered = debug_span!("insertBackTab:").entered();
            let window = self.window();
            if let Some(first_responder) = window.firstResponder() {
                if *first_responder == ***self {
                    window.selectPreviousKeyView(Some(self))
                }
            }
        }

        // Allows us to receive Cmd-. (the shortcut for closing a dialog)
        // https://bugs.eclipse.org/bugs/show_bug.cgi?id=300620#c6
        #[unsafe(method(cancelOperation:))]
        fn cancel_operation(&self, _sender: Option<&AnyObject>) {
            let mtm = MainThreadMarker::from(self);
            let _entered = debug_span!("cancelOperation:").entered();

            let event = NSApplication::sharedApplication(mtm)
                .currentEvent()
                .expect("could not find current event");

            self.update_modifiers(&event, false);
            let event = create_key_event(&event, true, event.isARepeat());

            self.queue_event(WindowEvent::KeyboardInput {
                device_id: None,
                event,
                is_synthetic: false,
            });
        }

        // In the past (?), `mouseMoved:` events were not generated when the
        // user hovered over a window from a separate window, and as such the
        // application might not know the location of the mouse in the event.
        //
        // To fix this, we emit `mouse_motion` inside of mouse click, mouse
        // scroll, magnify and other gesture event handlers, to ensure that
        // the application's state of where the mouse click was located is up
        // to date.
        //
        // See https://github.com/rust-windowing/winit/pull/1490 for history.

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, event: &NSEvent) {
            let _entered = debug_span!("mouseDown:").entered();
            self.cancel_resize();
            if self.handle_event(event) {
                return;
            }
            self.mouse_motion(event);
            self.mouse_click(event, ElementState::Pressed);
        }

        #[unsafe(method(mouseUp:))]
        fn mouse_up(&self, event: &NSEvent) {
            let _entered = debug_span!("mouseUp:").entered();
            self.cancel_resize();
            if self.handle_event(event) {
                return;
            }
            self.mouse_motion(event);
            self.mouse_click(event, ElementState::Released);
        }

        #[unsafe(method(rightMouseDown:))]
        fn right_mouse_down(&self, event: &NSEvent) {
            let _entered = debug_span!("rightMouseDown:").entered();
            if self.handle_event(event) {
                return;
            }
            self.mouse_motion(event);
            self.mouse_click(event, ElementState::Pressed);
        }

        #[unsafe(method(rightMouseUp:))]
        fn right_mouse_up(&self, event: &NSEvent) {
            let _entered = debug_span!("rightMouseUp:").entered();
            if self.handle_event(event) {
                return;
            }
            self.mouse_motion(event);
            self.mouse_click(event, ElementState::Released);
        }

        #[unsafe(method(otherMouseDown:))]
        fn other_mouse_down(&self, event: &NSEvent) {
            let _entered = debug_span!("otherMouseDown:").entered();
            if self.handle_event(event) {
                return;
            }
            self.mouse_motion(event);
            self.mouse_click(event, ElementState::Pressed);
        }

        #[unsafe(method(otherMouseUp:))]
        fn other_mouse_up(&self, event: &NSEvent) {
            let _entered = debug_span!("otherMouseUp:").entered();
            if self.handle_event(event) {
                return;
            }
            self.mouse_motion(event);
            self.mouse_click(event, ElementState::Released);
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, event: &NSEvent) {
            let _entered = debug_span!("mouseMoved:").entered();
            if self.handle_event(event) {
                return;
            }
            self.mouse_motion(event);
        }

        #[unsafe(method(mouseDragged:))]
        fn mouse_dragged(&self, event: &NSEvent) {
            let _entered = debug_span!("mouseDragged:").entered();
            if self.resize_drag(event) {
                return;
            }
            if self.handle_event(event) {
                return;
            }
            self.mouse_motion(event);
        }

        #[unsafe(method(rightMouseDragged:))]
        fn right_mouse_dragged(&self, event: &NSEvent) {
            let _entered = debug_span!("rightMouseDragged:").entered();
            if self.handle_event(event) {
                return;
            }
            self.mouse_motion(event);
        }

        #[unsafe(method(otherMouseDragged:))]
        fn other_mouse_dragged(&self, event: &NSEvent) {
            let _entered = debug_span!("otherMouseDragged:").entered();
            if self.handle_event(event) {
                return;
            }
            self.mouse_motion(event);
        }

        #[unsafe(method(mouseEntered:))]
        fn mouse_entered(&self, event: &NSEvent) {
            let _entered = debug_span!("mouseEntered:").entered();

            let position = self.mouse_view_point(event).to_physical(self.scale_factor());

            self.queue_event(WindowEvent::PointerEntered {
                device_id: None,
                primary: true,
                position,
                kind: PointerKind::Mouse,
            });
        }

        #[unsafe(method(mouseExited:))]
        fn mouse_exited(&self, event: &NSEvent) {
            let _entered = debug_span!("mouseExited:").entered();

            let position = self.mouse_view_point(event).to_physical(self.scale_factor());

            self.queue_event(WindowEvent::PointerLeft {
                device_id: None,
                primary: true,
                position: Some(position),
                kind: PointerKind::Mouse,
            });
        }

        #[unsafe(method(scrollWheel:))]
        fn scroll_wheel(&self, event: &NSEvent) {
            let _entered = debug_span!("scrollWheel:").entered();

            self.mouse_motion(event);

            let delta = {
                let (x, y) = (event.scrollingDeltaX(), event.scrollingDeltaY());
                if event.hasPreciseScrollingDeltas() {
                    let delta = LogicalPosition::new(x, y).to_physical(self.scale_factor());
                    MouseScrollDelta::PixelDelta(delta)
                } else {
                    MouseScrollDelta::LineDelta(x as f32, y as f32)
                }
            };

            // The "momentum phase," if any, has higher priority than touch phase (the two should
            // be mutually exclusive anyhow, which is why the API is rather incoherent). If no
            // momentum phase is recorded (or rather, the started/ended cases of the
            // momentum phase) then we report the touch phase.
            #[allow(non_upper_case_globals)]
            let phase = match event.momentumPhase() {
                NSEventPhase::MayBegin | NSEventPhase::Began => TouchPhase::Started,
                NSEventPhase::Ended | NSEventPhase::Cancelled => TouchPhase::Ended,
                _ => match event.phase() {
                    NSEventPhase::MayBegin | NSEventPhase::Began => TouchPhase::Started,
                    NSEventPhase::Ended | NSEventPhase::Cancelled => TouchPhase::Ended,
                    _ => TouchPhase::Moved,
                },
            };

            self.update_modifiers(event, false);

            self.ivars().app_state.maybe_queue_with_handler(move |app, event_loop| {
                app.device_event(event_loop, None, DeviceEvent::MouseWheel { delta })
            });
            self.queue_event(WindowEvent::MouseWheel { device_id: None, delta, phase });
        }

        #[unsafe(method(magnifyWithEvent:))]
        fn magnify_with_event(&self, event: &NSEvent) {
            let _entered = debug_span!("magnifyWithEvent:").entered();

            self.mouse_motion(event);

            #[allow(non_upper_case_globals)]
            let phase = match event.phase() {
                NSEventPhase::Began => TouchPhase::Started,
                NSEventPhase::Changed => TouchPhase::Moved,
                NSEventPhase::Cancelled => TouchPhase::Cancelled,
                NSEventPhase::Ended => TouchPhase::Ended,
                _ => return,
            };

            self.queue_event(WindowEvent::PinchGesture {
                device_id: None,
                delta: event.magnification(),
                phase,
            });
        }

        #[unsafe(method(smartMagnifyWithEvent:))]
        fn smart_magnify_with_event(&self, event: &NSEvent) {
            let _entered = debug_span!("smartMagnifyWithEvent:").entered();

            self.mouse_motion(event);

            self.queue_event(WindowEvent::DoubleTapGesture { device_id: None });
        }

        #[unsafe(method(rotateWithEvent:))]
        fn rotate_with_event(&self, event: &NSEvent) {
            let _entered = debug_span!("rotateWithEvent:").entered();

            self.mouse_motion(event);

            #[allow(non_upper_case_globals)]
            let phase = match event.phase() {
                NSEventPhase::Began => TouchPhase::Started,
                NSEventPhase::Changed => TouchPhase::Moved,
                NSEventPhase::Cancelled => TouchPhase::Cancelled,
                NSEventPhase::Ended => TouchPhase::Ended,
                _ => return,
            };

            self.queue_event(WindowEvent::RotationGesture {
                device_id: None,
                delta: event.rotation(),
                phase,
            });
        }

        #[unsafe(method(pressureChangeWithEvent:))]
        fn pressure_change_with_event(&self, event: &NSEvent) {
            let _entered = debug_span!("pressureChangeWithEvent:").entered();

            self.queue_event(WindowEvent::TouchpadPressure {
                device_id: None,
                pressure: event.pressure(),
                stage: event.stage() as i64,
            });
        }

        // Allows us to receive Ctrl-Tab and Ctrl-Esc.
        // Note that this *doesn't* help with any missing Cmd inputs.
        // https://github.com/chromium/chromium/blob/a86a8a6bcfa438fa3ac2eba6f02b3ad1f8e0756f/ui/views/cocoa/bridged_content_view.mm#L816
        #[unsafe(method(_wantsKeyDownForEvent:))]
        fn wants_key_down_for_event(&self, _event: &NSEvent) -> bool {
            let _entered = debug_span!("_wantsKeyDownForEvent:").entered();
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, event: Option<&NSEvent>) -> bool {
            let _entered = debug_span!("acceptsFirstMouse:").entered();
            // Always accept the click. The following `mouseDown:` is tagged with
            // `is_macos_activation_click: true` so the application can decide per click
            // whether to act on it.
            //
            // AppKit may pass nil here for synthetic queries that are not tied to a
            // specific click; only set the flag when we actually have an event.
            if event.is_some() {
                self.ivars().next_click_is_activation.set(true);
            }
            true
        }
    }
);

impl WinitView {
    pub(super) fn new(
        app_state: &Rc<AppState>,
        option_as_alt: OptionAsAlt,
        mtm: MainThreadMarker,
    ) -> Retained<Self> {
        let this = mtm.alloc().set_ivars(ViewState {
            app_state: Rc::clone(app_state),
            needs_panel_to_become_key: Cell::new(None),
            dragging_session: Default::default(),
            drag_operations: Cell::new(NSDragOperation::empty()),
            resize_drag: Cell::new(None),
            cursor_state: Default::default(),
            ime_position: Default::default(),
            ime_size: Default::default(),
            modifiers: Default::default(),
            phys_modifiers: Default::default(),
            text_input: Default::default(),
            input_source: Default::default(),
            ime_capabilities: Default::default(),
            forward_key_to_app: Default::default(),
            marked_text: Default::default(),
            next_click_is_activation: Default::default(),
            option_as_alt: Cell::new(option_as_alt),
        });
        let this: Retained<Self> = unsafe { msg_send![super(this), init] };
        *this.ivars().input_source.borrow_mut() = this.current_input_source();

        // Ask AppKit to redisplay the layer while the view is being resized so layer-backed
        // surfaces keep painting.
        this.setLayerContentsRedrawPolicy(NSViewLayerContentsRedrawPolicy::DuringViewResize);

        // `MouseEnteredAndExited` enables receiving events through `mouseEntered:` and
        // `mouseExited:`.
        //
        // `MouseMoved` enables receiving events through `mouseMoved:`
        //
        // We do not set `CursorUpdate` because it is part of the "flexible" alternative to
        // `cursorRect` based cursor image updates, and we currently still use
        // `cursorRect`s. We also can't really switch to this approach because "The
        // cursorUpdate(with:) message is not sent when the NSTrackingCursorUpdate option is
        // specified along with [`ActiveAlways`]."
        //
        // `ActiveAlways` indicates we want to receive events when the window is not
        // focused ("key window" in Cocoa terms), which matches the behavior on other
        // platforms.
        //
        // We do not set `AssumeInside` because we want to avoid emitting `Left` events without a
        // correspondering `Entered` to our consumers, and not setting this flag tells AppKit to
        // handle this for us by synthesizing entry and exit events in some cases.
        //
        // `InVisibleRect` instructs the tracking area's `owner` (our `NSView`) to ignore the value
        // we provide in `rect` and keep the tracking area's bounds up to date with the
        // current view bounds automatically.
        //
        // We do not set `EnabledDuringMouseDrag` to match the platform behavior on Windows
        // and Wayland, since neither emit events while being dragged over with an empty
        // cursor without focus.
        //
        // See also https://developer.apple.com/documentation/appkit/nstrackingareaoptions.

        // Safety: the type of `owner` should be `NSView` and is.
        // The type of `user_info` is irrelevant because it is None.
        this.addTrackingArea(&*unsafe {
            NSTrackingArea::initWithRect_options_owner_userInfo(
                NSTrackingArea::alloc(),
                NSRect::ZERO,
                NSTrackingAreaOptions::MouseEnteredAndExited
                    | NSTrackingAreaOptions::MouseMoved
                    | NSTrackingAreaOptions::ActiveAlways
                    | NSTrackingAreaOptions::InVisibleRect,
                Some(&this),
                None,
            )
        });

        this
    }

    fn window(&self) -> Retained<NSWindow> {
        (**self).window().expect("view must be installed in a window")
    }

    fn queue_event(&self, event: WindowEvent) {
        let window_id = window_id(&self.window());
        self.ivars().app_state.maybe_queue_with_handler(move |app, event_loop| {
            app.window_event(event_loop, window_id, event);
        });
    }

    pub(super) fn surface_resized(&self) {
        let Some(window) = (**self).window() else {
            return;
        };
        let window_id = window_id(&window);
        let view = self.retain();
        self.ivars().app_state.maybe_queue_with_handler(move |app, event_loop| {
            let size = view.surface_size();
            app.window_event(event_loop, window_id, WindowEvent::SurfaceResized(size));
        });
    }

    /// Returns the drawable size from the view's backing-coordinate bounds.
    pub(super) fn surface_size(&self) -> PhysicalSize<u32> {
        // The view bounds are authoritative for full-size content views and during live resize.
        // Deriving this from the window frame can exclude custom titlebar content or be stale.
        let backing_bounds = self.convertRectToBacking(self.bounds());
        PhysicalSize::new(
            backing_bounds.size.width.round().max(0.0) as u32,
            backing_bounds.size.height.round().max(0.0) as u32,
        )
    }

    fn redraw_during_live_resize(&self) {
        let Some(window) = (**self).window() else {
            return;
        };
        let resizing = self.ivars().resize_drag.get().is_some();
        if window.inLiveResize() || resizing {
            self.ivars().app_state.handle_redraw(window_id(&window));
        }
    }

    /// Begin only from the owning view's native primary-button event. AppKit
    /// continues delivering its drag/release to that view outside the window,
    /// so no global monitor, polling or pointer warp is needed.
    pub(crate) fn begin_resize(
        &self,
        direction: ResizeDirection,
        increments: NSSize,
    ) -> Result<(), NotSupportedError> {
        let Some(window) = (**self).window() else {
            return Err(NotSupportedError::new("resize requires an attached view"));
        };
        let event = self.window_drag_press(&window)?;
        self.ivars().resize_drag.set(Some(ResizeDrag::new(
            direction,
            window.frame(),
            window.convertPointToScreen(event.locationInWindow()),
            increments,
        )));
        Ok(())
    }

    pub(crate) fn cancel_resize(&self) {
        self.ivars().resize_drag.set(None);
    }

    fn window_drag_press(&self, window: &NSWindow) -> Result<Retained<NSEvent>, NotSupportedError> {
        let application = NSApplication::sharedApplication(MainThreadMarker::from(self));
        let Some(event) = application.currentEvent() else {
            return Err(NotSupportedError::new("window drag requires a left mouse press"));
        };
        if event.r#type() != NSEventType::LeftMouseDown
            || event.windowNumber() != window.windowNumber()
        {
            return Err(NotSupportedError::new(
                "window drag requires this window's left mouse press",
            ));
        }
        Ok(event)
    }

    pub(crate) fn begin_window_move(&self) -> Result<(), NotSupportedError> {
        let Some(window) = (**self).window() else {
            return Err(NotSupportedError::new("window move requires an attached view"));
        };
        let press = self.window_drag_press(&window)?;
        // Match AppKit's background dragging: multi-click presses do not start
        // window moves. Passing one to the explicit drag API instead emits
        // "Window move completed without beginning" on macOS 27.2, even in a
        // plain NSView without winit. Keep the original single-click press;
        // synthesizing a different click count would change native semantics.
        // https://developer.apple.com/documentation/appkit/nswindow/performdrag(with:)
        if press.clickCount() != 1 {
            return Ok(());
        }

        // A queued press can arrive after physical release. The native API
        // cannot start that gesture either, so check the live primary-button
        // state immediately before handing the original press to AppKit.
        let primary_down = NSEvent::pressedMouseButtons() & PRIMARY_BUTTON_MASK != 0;
        trace!(primary_down, "native window move request");
        if primary_down {
            window.performWindowDragWithEvent(&press);
        }
        Ok(())
    }

    fn resize_drag(&self, event: &NSEvent) -> bool {
        let resize = self.ivars().resize_drag.get();
        let Some(drag) = resize else {
            return false;
        };
        let Some(window) = (**self).window() else {
            self.cancel_resize();
            return false;
        };
        let style = window.styleMask();
        if !window.isVisible()
            || !style.contains(NSWindowStyleMask::Resizable)
            || style.intersects(NSWindowStyleMask::Titled | NSWindowStyleMask::FullScreen)
        {
            self.cancel_resize();
            return false;
        }

        let frame = drag.frame_at(
            window.convertPointToScreen(event.locationInWindow()),
            window.minSize(),
            window.maxSize(),
        );
        if frame != window.frame() {
            // Native frame updates publish the normal surface-size/move
            // callbacks. The resize state is copied before this reentrant call.
            window.setFrame_display(frame, true);
        }
        true
    }

    pub(crate) fn scale_factor(&self) -> f64 {
        self.window().backingScaleFactor() as f64
    }

    fn is_ime_enabled(&self) -> bool {
        !matches!(self.ivars().text_input.state.get(), ImeState::Disabled)
    }

    fn current_input_source(&self) -> String {
        self.inputContext()
            .expect("input context")
            .selectedKeyboardInputSource()
            .map(|input_source| input_source.to_string())
            .unwrap_or_default()
    }

    pub(super) fn cursor_icon(&self) -> Retained<NSCursor> {
        self.ivars().cursor_state.borrow().cursor.clone()
    }

    pub(super) fn set_cursor_icon(&self, icon: Retained<NSCursor>) {
        let mut cursor_state = self.ivars().cursor_state.borrow_mut();
        cursor_state.cursor = icon;
    }

    /// Set whether the cursor should be visible or not.
    ///
    /// Returns whether the state changed.
    pub(super) fn set_cursor_visible(&self, visible: bool) -> bool {
        let mut cursor_state = self.ivars().cursor_state.borrow_mut();
        if visible != cursor_state.visible {
            cursor_state.visible = visible;
            true
        } else {
            false
        }
    }
    pub(super) fn enable_ime(&self, capabilities: ImeCapabilities) {
        // This seems reasonable but the prior behavior of `set_ime_allowed` doesn't do this
        // (it was also broken but let's not break things worse)

        // if self.ivars().ime_capabilities.get().is_none() {
        //     self.ivars().text_input.state.set(ImeState::Ground);
        // }

        // why are we disabling things in an enable fn? who knows. it's what the previous one did
        // though
        if self.ivars().text_input.state.get() != ImeState::Disabled {
            self.ivars().text_input.state.set(ImeState::Disabled);
            self.queue_event(WindowEvent::Ime(Ime::Disabled));
        }
        self.ivars().ime_capabilities.set(Some(capabilities));
        *self.ivars().marked_text.borrow_mut() = NSMutableAttributedString::new();
        self.ivars().text_input.pending_commit.set(false);
    }
    pub(super) fn disable_ime(&self) {
        // see above
        self.ivars().ime_capabilities.set(None);
        if self.ivars().text_input.state.get() != ImeState::Disabled {
            self.ivars().text_input.state.set(ImeState::Disabled);
            self.queue_event(WindowEvent::Ime(Ime::Disabled));
        }
        // we probably don't need to do this, but again this mirrors the prior behavior of
        // `set_ime_allowed`
        *self.ivars().marked_text.borrow_mut() = NSMutableAttributedString::new();
        self.ivars().text_input.pending_commit.set(false);
    }

    pub(super) fn ime_capabilities(&self) -> Option<ImeCapabilities> {
        self.ivars().ime_capabilities.get()
    }

    pub(super) fn set_ime_cursor_area(&self, position: NSPoint, size: NSSize) {
        self.ivars().ime_position.set(position);
        self.ivars().ime_size.set(size);
        let input_context = self.inputContext().expect("input context");
        input_context.invalidateCharacterCoordinates();
    }

    /// Reset modifiers and emit a synthetic ModifiersChanged event if deemed necessary.
    pub(super) fn reset_modifiers(&self) {
        if !self.ivars().modifiers.get().state().is_empty() {
            self.ivars().modifiers.set(Modifiers::default());
            self.queue_event(WindowEvent::ModifiersChanged(self.ivars().modifiers.get()));
        }
    }

    pub(crate) fn set_needs_panel_to_become_key(&self, needs_key: bool) {
        self.ivars().needs_panel_to_become_key.set(Some(needs_key));
    }

    pub(super) fn set_option_as_alt(&self, value: OptionAsAlt) {
        self.ivars().option_as_alt.set(value)
    }

    pub(super) fn option_as_alt(&self) -> OptionAsAlt {
        self.ivars().option_as_alt.get()
    }

    /// Update modifiers if `event` has something different
    fn update_modifiers(&self, ns_event: &NSEvent, is_flags_changed_event: bool) {
        use ElementState::{Pressed, Released};

        let current_modifiers = event_mods(ns_event);
        let prev_modifiers = self.ivars().modifiers.get();
        self.ivars().modifiers.set(current_modifiers);

        // This function was called form the flagsChanged event, which is triggered
        // when the user presses/releases a modifier even if the same kind of modifier
        // has already been pressed.
        //
        // When flags changed event has key code of zero it means that event doesn't carry any key
        // event, thus we can't generate regular presses based on that. The `ModifiersChanged`
        // later will work though, since the flags are attached to the event and contain valid
        // information.
        'send_event: {
            if is_flags_changed_event && ns_event.keyCode() != 0 {
                let scancode = ns_event.keyCode();
                let physical_key = scancode_to_physicalkey(scancode as u32);

                let logical_key = code_to_key(physical_key, scancode);
                // Ignore processing of unknown modifiers because we can't determine whether
                // it was pressed or release reliably.
                //
                // Furthermore, sometimes normal keys are reported inside flagsChanged:, such as
                // when holding Caps Lock while pressing another key, see:
                // https://github.com/alacritty/alacritty/issues/8268
                let Some(event_modifier) = key_to_modifier(&logical_key) else {
                    break 'send_event;
                };

                let mut event = KeyEvent {
                    location: code_to_location(physical_key),
                    logical_key: logical_key.clone(),
                    physical_key,
                    repeat: false,
                    // We'll correct this later.
                    state: Pressed,
                    text: None,
                    text_with_all_modifiers: None,
                    key_without_modifiers: logical_key.clone(),
                };

                let location_mask = ModLocationMask::from_location(event.location);

                let mut phys_mod_state = self.ivars().phys_modifiers.borrow_mut();
                let phys_mod =
                    phys_mod_state.entry(logical_key).or_insert(ModLocationMask::empty());

                let is_active = current_modifiers.state().contains(event_modifier);
                let mut events = VecDeque::with_capacity(2);

                // There is no API for getting whether the button was pressed or released
                // during this event. For this reason we have to do a bit of magic below
                // to come up with a good guess whether this key was pressed or released.
                // (This is not trivial because there are multiple buttons that may affect
                // the same modifier)
                if !is_active {
                    event.state = Released;
                    if phys_mod.contains(ModLocationMask::LEFT) {
                        let mut event = event.clone();
                        event.location = KeyLocation::Left;
                        event.physical_key = get_left_modifier_code(&event.logical_key).into();
                        events.push_back(WindowEvent::KeyboardInput {
                            device_id: None,
                            event,
                            is_synthetic: false,
                        });
                    }
                    if phys_mod.contains(ModLocationMask::RIGHT) {
                        event.location = KeyLocation::Right;
                        event.physical_key = get_right_modifier_code(&event.logical_key).into();
                        events.push_back(WindowEvent::KeyboardInput {
                            device_id: None,
                            event,
                            is_synthetic: false,
                        });
                    }
                    *phys_mod = ModLocationMask::empty();
                } else {
                    if *phys_mod == location_mask {
                        // Here we hit a contradiction:
                        // The modifier state was "changed" to active,
                        // yet the only pressed modifier key was the one that we
                        // just got a change event for.
                        // This seemingly means that the only pressed modifier is now released,
                        // but at the same time the modifier became active.
                        //
                        // But this scenario is possible if we released modifiers
                        // while the application was not in focus. (Because we don't
                        // get informed of modifier key events while the application
                        // is not focused)

                        // In this case we prioritize the information
                        // about the current modifier state which means
                        // that the button was pressed.
                        event.state = Pressed;
                    } else {
                        phys_mod.toggle(location_mask);
                        let is_pressed = phys_mod.contains(location_mask);
                        event.state = if is_pressed { Pressed } else { Released };
                    }

                    events.push_back(WindowEvent::KeyboardInput {
                        device_id: None,
                        event,
                        is_synthetic: false,
                    });
                }

                drop(phys_mod_state);

                for event in events {
                    self.queue_event(event);
                }
            }
        }

        if prev_modifiers == current_modifiers {
            return;
        }

        self.queue_event(WindowEvent::ModifiersChanged(self.ivars().modifiers.get()));
    }

    pub(crate) fn set_dragging_session(
        &self,
        drag: Retained<NSDraggingSession>,
        action_mask: NSDragOperation,
    ) {
        let vars = self.ivars();
        vars.dragging_session.replace(Some(drag));
        vars.drag_operations.set(action_mask);
    }

    pub(crate) fn drag_operations(&self) -> NSDragOperation {
        self.ivars().drag_operations.get()
    }

    pub(crate) fn clear_dragging_session(&self, drag: &NSDraggingSession) -> bool {
        let vars = self.ivars();
        vars.drag_operations.set(NSDragOperation::empty());
        let mut dragging_session = vars.dragging_session.borrow_mut();
        dragging_session
            .take_if(|session| session.draggingSequenceNumber() == drag.draggingSequenceNumber())
            .is_some()
    }

    /// Returns true iff the event was consumed by the input context.
    fn handle_event(&self, event: &NSEvent) -> bool {
        self.inputContext().map(|input_context| input_context.handleEvent(event)).unwrap_or(false)
    }

    fn mouse_click(&self, event: &NSEvent, button_state: ElementState) {
        let position = self.mouse_view_point(event).to_physical(self.scale_factor());
        let button = mouse_button(event);

        self.update_modifiers(event, false);

        // `acceptsFirstMouse:` fires only for the left button, so the activation flag is
        // applied to the left press and held until the matching left release. This lets
        // apps short-circuit both events with a single `if is_macos_activation_click` check
        // without having to track gesture state themselves.
        let is_macos_activation_click = matches!(button, MouseButton::Left)
            && match button_state {
                ElementState::Pressed => self.ivars().next_click_is_activation.get(),
                ElementState::Released => self.ivars().next_click_is_activation.replace(false),
            };

        self.queue_event(WindowEvent::PointerButton {
            device_id: None,
            primary: true,
            state: button_state,
            position,
            button: button.into(),
            is_macos_activation_click,
        });
    }

    fn mouse_motion(&self, event: &NSEvent) {
        let view_point = self.mouse_view_point(event);
        let frame = self.frame();

        if view_point.x.is_sign_negative()
            || view_point.y.is_sign_negative()
            || view_point.x > frame.size.width
            || view_point.y > frame.size.height
        {
            let mouse_buttons_down = NSEvent::pressedMouseButtons();
            if mouse_buttons_down == 0 {
                // Point is outside of the client area (view) and no buttons are pressed
                return;
            }
        }

        self.update_modifiers(event, false);

        self.queue_event(WindowEvent::PointerMoved {
            device_id: None,
            primary: true,
            position: view_point.to_physical(self.scale_factor()),
            source: PointerSource::Mouse,
        });
    }

    fn mouse_view_point(&self, event: &NSEvent) -> LogicalPosition<f64> {
        let window_point = event.locationInWindow();
        let view_point = self.convertPoint_fromView(window_point, None);

        LogicalPosition::new(view_point.x, view_point.y)
    }
}

/// Get the mouse button from the NSEvent.
fn mouse_button(event: &NSEvent) -> MouseButton {
    // The buttonNumber property only makes sense for the mouse events:
    // NSLeftMouse.../NSRightMouse.../NSOtherMouse...
    // For the other events, it's always set to 0.
    // MacOS only defines the left, right and middle buttons, 3..=31 are left as generic buttons,
    // but 3 and 4 are very commonly used as Back and Forward by hardware vendors and applications.
    let b: isize = event.buttonNumber();
    b.try_into()
        .ok()
        .and_then(MouseButton::try_from_u8)
        .expect("expected MacOS button number in the range 0..=31")
}

// NOTE: to get option as alt working we need to rewrite events
// we're getting from the operating system, which makes it
// impossible to provide such events as extra in `KeyEvent`.
fn replace_event(event: &NSEvent, option_as_alt: OptionAsAlt) -> Retained<NSEvent> {
    let ev_mods = event_mods(event).state();
    let ignore_alt_characters = match option_as_alt {
        OptionAsAlt::OnlyLeft if lalt_pressed(event) => true,
        OptionAsAlt::OnlyRight if ralt_pressed(event) => true,
        OptionAsAlt::Both if ev_mods.alt_key() => true,
        _ => false,
    } && !ev_mods.control_key()
        && !ev_mods.meta_key();

    if ignore_alt_characters {
        let ns_chars =
            event.charactersIgnoringModifiers().expect("expected characters to be non-null");

        NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
                event.r#type(),
                event.locationInWindow(),
                event.modifierFlags(),
                event.timestamp(),
                event.windowNumber(),
                None,
                &ns_chars,
                &ns_chars,
                event.isARepeat(),
                event.keyCode(),
            )
            .unwrap()
    } else {
        event.copy()
    }
}

/// Convert a UTF-16 code unit offset into the corresponding UTF-8 byte offset within `s`.
///
/// IMEs are not required to send well-formed offsets, so this is defensive: an offset that
/// would split a surrogate pair is snapped down to the start of that character, and an
/// out-of-bounds offset is clamped to the end of the string (e.g. native Pinyin sends
/// out-of-bounds indices, see <https://github.com/alacritty/alacritty/issues/8791>).
///
/// The mapping is monotone non-decreasing, so applying it to the location and end of an
/// `NSRange` (where `location <= end`) can never produce an inverted byte range.
fn utf16_to_utf8_offset(s: &str, utf16_offset: usize) -> usize {
    let mut utf16_pos = 0;
    for (utf8_pos, ch) in s.char_indices() {
        if utf16_pos >= utf16_offset {
            return utf8_pos;
        }
        utf16_pos += ch.len_utf16();
        // The target offset lands strictly inside this character's UTF-16 representation,
        // i.e. it splits a surrogate pair: snap down to the character boundary.
        if utf16_pos > utf16_offset {
            return utf8_pos;
        }
    }
    s.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn character_palette_commits_without_preedit_and_leaves_next_key_alone() {
        let input = TextInput::default();

        assert!(input.insert(true, "😀"));
        assert_eq!(input.state.get(), ImeState::Ground);
        assert!(!input.pending_commit.get());

        input.interpret_key(|| assert!(!input.insert(true, "a")));
        assert_eq!(input.state.get(), ImeState::Ground);
        assert!(input.insert(true, "😀"));
    }

    #[test]
    fn ordinary_key_text_uses_keyboard_input_before_and_after_ime_activation() {
        let input = TextInput::default();

        for state in [ImeState::Disabled, ImeState::Ground] {
            input.state.set(state);
            input.interpret_key(|| {
                assert!(!input.insert(true, "a"));
                assert!(!input.insert(true, " "));
            });
            assert_eq!(input.state.get(), state);
        }
    }

    #[test]
    fn composition_commit_consumes_only_its_confirming_key() {
        let input = TextInput::default();

        // An IME may unmark before inserting. Both Preedit and Ground must retain
        // the pending composition, independently of whether marked text remains.
        for state in [ImeState::Preedit, ImeState::Ground] {
            input.state.set(state);
            input.pending_commit.set(true);
            input.interpret_key(|| {
                assert!(input.insert(true, "😀"));
                assert_eq!(input.state.get(), ImeState::Committed);
                assert!(!input.pending_commit.get());
            });

            input.state.set(ImeState::Ground);
            input.interpret_key(|| assert!(!input.insert(true, "a")));
        }
    }

    #[test]
    fn asynchronous_composition_commit_does_not_consume_an_unrelated_key() {
        let input = TextInput::default();
        input.state.set(ImeState::Preedit);
        input.pending_commit.set(true);

        assert!(input.insert(true, "😀"));
        assert_eq!(input.state.get(), ImeState::Ground);
        assert!(!input.pending_commit.get());
        input.interpret_key(|| assert!(!input.insert(true, "a")));
    }

    #[test]
    fn disabled_input_and_control_commands_do_not_commit() {
        let input = TextInput::default();

        assert!(!input.insert(false, "😀"));
        assert!(!input.insert(true, "\r"));
        assert_eq!(input.state.get(), ImeState::Disabled);

        input.state.set(ImeState::Preedit);
        input.pending_commit.set(true);
        input.interpret_key(|| {
            assert!(!input.insert(false, "😀"));
            assert!(!input.insert(true, "\r"));
        });
        assert!(input.pending_commit.get());
        assert_eq!(input.state.get(), ImeState::Preedit);
    }

    #[test]
    fn nested_key_interpretation_restores_the_enclosing_context() {
        let input = TextInput::default();

        input.interpret_key(|| {
            input.interpret_key(|| assert!(!input.insert(true, "a")));
            assert!(!input.insert(true, "😀"));
        });
        assert!(input.insert(true, "😀"));
    }

    /// Apply the UTF-16 -> UTF-8 conversion to both ends of a `selectedRange {loc, len}`,
    /// mirroring what `set_marked_text` does for the emitted `Ime::Preedit` cursor range.
    fn convert(s: &str, loc: usize, len: usize) -> (usize, usize) {
        (utf16_to_utf8_offset(s, loc), utf16_to_utf8_offset(s, loc + len))
    }

    #[test]
    fn mid_surrogate_offset_snaps_down() {
        // "😀a": 😀 is one char = 2 UTF-16 units = 4 UTF-8 bytes; offset 1 is mid-pair.
        assert_eq!(utf16_to_utf8_offset("\u{1F600}a", 1), 0);
        // Offset 2 is the boundary just after the pair.
        assert_eq!(utf16_to_utf8_offset("\u{1F600}a", 2), 4);
    }

    #[test]
    fn no_longer_inverted() {
        // "a😀b" with selectedRange {1,1}: previously emitted (1, 0) -- lower > upper, a
        // slice-panic vector. The boundary-snapping conversion keeps lower <= upper.
        assert_eq!(convert("a\u{1F600}b", 1, 1), (1, 1));
    }

    #[test]
    fn prefix_preserved_on_mid_pair_collapse() {
        // "a😀b" with selectedRange {2,0}: previously collapsed to (0, 0), discarding the
        // valid "a" prefix; now snaps to the char boundary after "a".
        assert_eq!(convert("a\u{1F600}b", 2, 0), (1, 1));
    }

    #[test]
    fn out_of_bounds_clamps_to_len() {
        // Subsumes the #4494 `.min(len)` clamp: an out-of-bounds offset maps to the string
        // length instead of triggering an NSRangeException.
        assert_eq!(convert("\u{1F600}a", 99, 0), (5, 5));
    }

    #[test]
    fn well_formed_inputs_are_identity() {
        // The common case (well-formed boundary indices) must be byte-for-byte unchanged.
        assert_eq!(convert("a\u{1F600}b", 3, 0), (5, 5));
        assert_eq!(convert("a\u{1F600}b", 4, 0), (6, 6));
        // BMP multi-byte (Japanese): each char is 1 UTF-16 unit and 3 UTF-8 bytes.
        assert_eq!(convert("\u{3053}\u{3093}", 1, 1), (3, 6));
    }

    #[test]
    fn monotone_non_decreasing() {
        // Sweep every UTF-16 offset (including out-of-bounds) over a string mixing BMP and
        // non-BMP characters and assert the conversion never goes backwards, which is what
        // guarantees `lower <= upper` for any `NSRange`.
        let s = "a\u{1F600}b\u{3053}\u{1F4A9}c";
        let mut prev = 0;
        for off in 0..=20 {
            let cur = utf16_to_utf8_offset(s, off);
            assert!(cur >= prev, "non-monotone at offset {off}: {cur} < {prev}");
            assert!(cur <= s.len(), "offset {off} mapped past end: {cur} > {}", s.len());
            prev = cur;
        }
    }
}
