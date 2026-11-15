use std::ops::Deref;

use dpi::{LogicalPosition, LogicalSize};
use sctk::globals::GlobalData;
use sctk::reexports::client::globals::{BindError, GlobalList};
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Dispatch, Proxy, QueueHandle, delegate_dispatch};
use sctk::reexports::protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use sctk::reexports::protocols::wp::text_input::zv3::client::zwp_text_input_v3::{
    ContentHint, ContentPurpose, Event as TextInputEvent, ZwpTextInputV3,
};
use tracing::warn;
use winit_core::event::{Ime, WindowEvent};
use winit_core::window::{
    ImeCapabilities, ImeHint, ImePurpose, ImeRequestData, ImeSurroundingText,
};

use crate::state::WinitState;

#[derive(Debug)]
pub struct TextInputState {
    text_input_manager: ZwpTextInputManagerV3,
}

impl TextInputState {
    pub fn new(
        globals: &GlobalList,
        queue_handle: &QueueHandle<WinitState>,
    ) -> Result<Self, BindError> {
        let text_input_manager = globals.bind(queue_handle, 1..=1, GlobalData)?;
        Ok(Self { text_input_manager })
    }
}

impl Deref for TextInputState {
    type Target = ZwpTextInputManagerV3;

    fn deref(&self) -> &Self::Target {
        &self.text_input_manager
    }
}

impl Dispatch<ZwpTextInputManagerV3, GlobalData, WinitState> for TextInputState {
    fn event(
        _state: &mut WinitState,
        _proxy: &ZwpTextInputManagerV3,
        _event: <ZwpTextInputManagerV3 as Proxy>::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &QueueHandle<WinitState>,
    ) {
    }
}

// Lock order: a window's state before a text input's data. The application updates its input
// method state with the window locked, so the dispatcher below never holds a text input's data
// while locking a window.
impl Dispatch<ZwpTextInputV3, TextInputData, WinitState> for TextInputState {
    fn event(
        state: &mut WinitState,
        text_input: &ZwpTextInputV3,
        event: <ZwpTextInputV3 as Proxy>::Event,
        data: &TextInputData,
        _conn: &Connection,
        _qhandle: &QueueHandle<WinitState>,
    ) {
        let windows = state.windows.get_mut();
        match event {
            TextInputEvent::Enter { surface } => {
                let window_id = crate::make_wid(&surface);
                data.inner.lock().unwrap().surface = Some(surface);

                let mut window = match windows.get(&window_id) {
                    Some(window) => window.lock().unwrap(),
                    None => return,
                };

                if let Some(text_input_state) = window.text_input_state() {
                    text_input.set_state(Some(text_input_state), true);
                    // The input method doesn't have to reply anything, so a synthetic event
                    // carrying an empty state notifies the application about its presence.
                    state.events_sink.push_window_event(WindowEvent::Ime(Ime::Enabled), window_id);
                }

                window.text_input_entered(text_input);
            },
            TextInputEvent::Leave { surface } => {
                {
                    let mut text_input_data = data.inner.lock().unwrap();
                    text_input_data.surface = None;
                    text_input_data.last_preedit_empty = true;
                }

                // Always issue a disable.
                text_input.set_state(None, false);

                let window_id = crate::make_wid(&surface);

                // XXX this check is essential, because `leave` could have a
                // reference to nil surface...
                let mut window = match windows.get(&window_id) {
                    Some(window) => window.lock().unwrap(),
                    None => return,
                };

                window.text_input_left(text_input);

                state.events_sink.push_window_event(WindowEvent::Ime(Ime::Disabled), window_id);
            },
            TextInputEvent::PreeditString { text, cursor_begin, cursor_end } => {
                let text = text.unwrap_or_default();
                let cursor_begin = usize::try_from(cursor_begin)
                    .ok()
                    .and_then(|idx| text.is_char_boundary(idx).then_some(idx));
                let cursor_end = usize::try_from(cursor_end)
                    .ok()
                    .and_then(|idx| text.is_char_boundary(idx).then_some(idx));

                data.inner.lock().unwrap().pending_preedit =
                    Some(Preedit { text, cursor_begin, cursor_end })
            },
            TextInputEvent::CommitString { text } => {
                let mut text_input_data = data.inner.lock().unwrap();
                text_input_data.pending_preedit = None;
                text_input_data.pending_commit = text;
            },
            TextInputEvent::DeleteSurroundingText { before_length, after_length } => {
                data.inner.lock().unwrap().pending_delete = Some(DeleteSurroundingText {
                    before: before_length as usize,
                    after: after_length as usize,
                });
            },
            TextInputEvent::Done { serial } => {
                let window_id = match data.inner.lock().unwrap().surface.as_ref() {
                    Some(surface) => crate::make_wid(surface),
                    None => return,
                };

                let window = windows.get(&window_id).map(|window| window.lock().unwrap());
                let client_state = window.as_ref().and_then(|window| window.text_input_state());
                let mut text_input_data = data.inner.lock().unwrap();

                // The serial says how many of our commits the input method knew of when it
                // produced these events, whatever happens to them below.
                text_input_data.sync.done(serial);

                // Just in case some IME sends an event for the disabled window.
                if window.is_some() && client_state.is_none() {
                    return;
                }

                // The events are sent to the user separately, so
                // CAUTION: events must always arrive in the order compatible with the application
                // order specified by the text-input-v3 protocol:
                //
                // As of version 1:
                // 1. Replace existing preedit string with the cursor.
                // 2. Delete requested surrounding text.
                // 3. Insert commit string with the cursor at its end.
                // 4. Calculate surrounding text to send.
                // 5. Insert new preedit text in cursor position.
                // 6. Place cursor inside preedit text.

                let delete = text_input_data.pending_delete.take();
                if let Some(DeleteSurroundingText { before, after }) = delete {
                    // The counts are relative to the selection in the surrounding text the input
                    // method knew, which the application may have changed since; it decides from
                    // that text whether the request still applies.
                    let surrounding_text = text_input_data.sync.surrounding_text().cloned();
                    state.events_sink.push_window_event(
                        WindowEvent::Ime(Ime::DeleteSurrounding {
                            before_bytes: before,
                            after_bytes: after,
                            surrounding_text,
                        }),
                        window_id,
                    );
                }

                // Clear preedit, unless all we'll be doing next is sending a new preedit and
                // the last preedit wasn't empty.
                if text_input_data.pending_commit.is_some()
                    || (text_input_data.pending_preedit.is_none()
                        && !text_input_data.last_preedit_empty)
                {
                    state.events_sink.push_window_event(
                        WindowEvent::Ime(Ime::Preedit(String::new(), None)),
                        window_id,
                    );
                    text_input_data.last_preedit_empty = true;
                }

                let commit = text_input_data.pending_commit.take();
                let preedit = text_input_data.pending_preedit.take();
                let composing = preedit.as_ref().is_some_and(|preedit| !preedit.text.is_empty());
                text_input_data.sync.edited(
                    delete.map(|DeleteSurroundingText { before, after }| (before, after)),
                    commit.as_deref(),
                    composing,
                );

                // Send `Commit`.
                if let Some(text) = commit {
                    state
                        .events_sink
                        .push_window_event(WindowEvent::Ime(Ime::Commit(text)), window_id);
                }

                // Send preedit.
                if let Some(preedit) = preedit {
                    let cursor_range =
                        preedit.cursor_begin.map(|b| (b, preedit.cursor_end.unwrap_or(b)));

                    text_input_data.last_preedit_empty = false;
                    state.events_sink.push_window_event(
                        WindowEvent::Ime(Ime::Preedit(preedit.text, cursor_range)),
                        window_id,
                    );
                }

                // State the application changed while the input method was behind goes out now
                // that it has caught up, as Chromium does.
                if let Some(client_state) = client_state {
                    if text_input_data.sync.flush_due() {
                        send_state(text_input, &mut text_input_data, Some(client_state), false);
                    }
                }
            },
            _ => {},
        }
    }
}

pub trait ZwpTextInputV3Ext {
    /// Applies the entire state atomically to the input method. It will skip the "enable" request
    /// if `already_enabled` is `true`.
    ///
    /// The text input's data must not be locked by the caller.
    fn set_state(&self, state: Option<&ClientState>, send_enable: bool);
}

impl ZwpTextInputV3Ext for ZwpTextInputV3 {
    fn set_state(&self, state: Option<&ClientState>, send_enable: bool) {
        let data = self.data::<TextInputData>().expect("text inputs are created with their data");
        send_state(self, &mut data.inner.lock().unwrap(), state, send_enable);
    }
}

/// Sends `state` to the input method, or holds it back while the input method has not caught up
/// with an earlier commit (see [`Sync`]). Enabling and disabling always go out at once: they
/// reset the input method's state, as on a focus change.
fn send_state(
    text_input: &ZwpTextInputV3,
    text_input_data: &mut TextInputDataInner,
    state: Option<&ClientState>,
    send_enable: bool,
) {
    let state = match state {
        Some(state) => state,
        None => {
            text_input.disable();
            text_input.commit();
            text_input_data.sync.committed(None);
            return;
        },
    };

    if !send_enable && !text_input_data.sync.may_send() {
        return;
    }

    if send_enable {
        text_input.enable();
    }

    if let Some(content_type) = state.content_type() {
        text_input.set_content_type(content_type.hint, content_type.purpose);
    }

    if let Some((position, size)) = state.cursor_area() {
        let (x, y) = (position.x as i32, position.y as i32);
        let (width, height) = (size.width as i32, size.height as i32);
        // The same cursor can be applied on different seats.
        // It's the compositor's responsibility to make sure that any present popups don't
        // overlap.
        text_input.set_cursor_rectangle(x, y, width, height);
    }

    let surrounding = state.surrounding_text();
    if let Some(surrounding) = surrounding {
        text_input.set_surrounding_text(
            surrounding.text().into(),
            surrounding.cursor() as i32,
            surrounding.anchor() as i32,
        );
    }

    text_input.commit();
    text_input_data.sync.committed(surrounding.cloned());
}

/// What the input method knows of our state, following text-input-v3's serials.
///
/// Every `commit` request applies our pending state, and every `done` event carries the number of
/// commits the input method had seen when it produced the events before it. While that number is
/// behind, the protocol asks clients to hold back new state until a `done` with a matching serial
/// arrives, and Chromium does so (`ZwpTextInputV3Impl::SendPendingImeData`). Otherwise the input
/// method computes its edits against text that has already changed under it.
///
/// It also keeps the surrounding text the input method last received, updated by the edits the
/// input method has made since, because `delete_surrounding_text` counts from the selection in
/// that text rather than from the application's current one. The application compares it with
/// its current text to tell whether a deletion still applies. This relies on compositors sending
/// `done` after the commits they forward, as Chromium does too: a compositor that never answers
/// a commit leaves later state held back until the next enable.
#[derive(Debug, Default)]
struct Sync {
    /// The number of `commit` requests sent, wrapping as the serial does.
    commits: u32,
    /// The serial of the last `done` event.
    done_serial: u32,
    /// Whether state was held back while the input method was behind.
    held_back: bool,
    /// The surrounding text the input method knows, if it was given one and our updates of it
    /// could follow its edits.
    surrounding_text: Option<ImeSurroundingText>,
}

impl Sync {
    /// Records a `done` event.
    fn done(&mut self, serial: u32) {
        self.done_serial = serial;
    }

    /// Whether new state may go out now. Otherwise it is marked as held back.
    fn may_send(&mut self) -> bool {
        let caught_up = self.commits == self.done_serial;
        self.held_back |= !caught_up;
        caught_up
    }

    /// Whether held-back state should go out now.
    fn flush_due(&self) -> bool {
        self.held_back && self.commits == self.done_serial
    }

    /// Records a `commit` request carrying `surrounding_text`.
    fn committed(&mut self, surrounding_text: Option<ImeSurroundingText>) {
        self.commits = self.commits.wrapping_add(1);
        self.held_back = false;
        self.surrounding_text = surrounding_text;
    }

    fn surrounding_text(&self) -> Option<&ImeSurroundingText> {
        self.surrounding_text.as_ref()
    }

    /// Follows the input method's own edits in the surrounding text it knows, in the protocol's
    /// order: the deletion around the selection, the commit string in its place, and a new
    /// preedit, which replaces the selection and is not part of surrounding text. Until the
    /// application sends its next state, a following deletion counts from there. If the edits do
    /// not fit the text, it is no longer known.
    fn edited(&mut self, delete: Option<(usize, usize)>, commit: Option<&str>, composing: bool) {
        if delete.is_none() && commit.is_none() && !composing {
            return;
        }

        self.surrounding_text = self.surrounding_text.take().and_then(|surrounding| {
            let selection_start = surrounding.cursor().min(surrounding.anchor());
            let selection_end = surrounding.cursor().max(surrounding.anchor());
            let (before, after) = delete.unwrap_or_default();
            let start = selection_start.checked_sub(before)?;
            let end = selection_end.checked_add(after)?;
            let mut text = surrounding.into_text();

            if end > text.len() || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
                return None;
            }

            let commit = commit.unwrap_or_default();
            text.replace_range(start..end, commit);
            let cursor = start + commit.len();
            ImeSurroundingText::new(text, cursor, cursor).ok()
        });
    }
}

/// The Data associated with the text input.
#[derive(Default)]
pub struct TextInputData {
    inner: std::sync::Mutex<TextInputDataInner>,
}

pub struct TextInputDataInner {
    /// The `WlSurface` we're performing input to.
    surface: Option<WlSurface>,

    /// The commit to submit on `done`.
    pending_commit: Option<String>,

    /// The preedit to submit on `done`.
    pending_preedit: Option<Preedit>,

    /// The text around the cursor to delete on `done`
    pending_delete: Option<DeleteSurroundingText>,

    /// Last preedit empty.
    last_preedit_empty: bool,

    /// What the input method knows of our state.
    sync: Sync,
}

impl Default for TextInputDataInner {
    fn default() -> Self {
        Self {
            surface: None,
            pending_commit: None,
            pending_preedit: None,
            pending_delete: None,
            last_preedit_empty: true,
            sync: Sync::default(),
        }
    }
}

/// The state of the preedit.
#[derive(Clone)]
struct Preedit {
    text: String,
    cursor_begin: Option<usize>,
    cursor_end: Option<usize>,
}

/// The delete request
#[derive(Clone)]
struct DeleteSurroundingText {
    /// Bytes before cursor
    before: usize,
    /// Bytes after cursor
    after: usize,
}

/// State change requested by the application.
///
/// This is a version that uses text_input abstractions translated from the ones used in
/// winit::core::window::ImeStateChange.
///
/// Fields that are initially set to None are unsupported capabilities
/// and trying to set them raises an error.
#[derive(Debug, PartialEq, Clone)]
pub struct ClientState {
    capabilities: ImeCapabilities,
    content_type: ContentType,
    /// The IME cursor area which should not be covered by the input method popup.
    cursor_area: (LogicalPosition<u32>, LogicalSize<u32>),

    /// The `ImeSurroundingText` struct is based on the Wayland model.
    /// When this changes, another struct might be needed.
    surrounding_text: ImeSurroundingText,
}

impl ClientState {
    pub fn new(
        capabilities: ImeCapabilities,
        request_data: ImeRequestData,
        scale_factor: f64,
    ) -> Self {
        let mut this = Self {
            capabilities,
            content_type: Default::default(),
            cursor_area: Default::default(),
            surrounding_text: ImeSurroundingText::new(String::new(), 0, 0).unwrap(),
        };

        let unsupported_flags = capabilities
            .without_hint_and_purpose()
            .without_cursor_area()
            .without_surrounding_text();

        if unsupported_flags != ImeCapabilities::new() {
            warn!(
                "Backend doesn't support all requested IME capabilities: {:?}.\n Ignoring.",
                unsupported_flags
            );
        }

        this.update(request_data, scale_factor);
        this
    }

    pub fn capabilities(&self) -> ImeCapabilities {
        self.capabilities
    }

    /// Updates the fields of the state which are present in update_fields.
    pub fn update(&mut self, request_data: ImeRequestData, scale_factor: f64) {
        if let Some((hint, purpose)) =
            request_data.hint_and_purpose.filter(|_| self.capabilities.hint_and_purpose())
        {
            self.content_type = (hint, purpose).into();
        }

        if let Some((position, size)) = request_data.cursor_area {
            if self.capabilities.cursor_area() {
                let position: LogicalPosition<u32> = position.to_logical(scale_factor);
                let size: LogicalSize<u32> = size.to_logical(scale_factor);
                self.cursor_area = (position, size);
            } else {
                warn!("discarding IME cursor area update without capability enabled.");
            }
        }

        if let Some(surrounding) = request_data.surrounding_text {
            if self.capabilities.surrounding_text() {
                self.surrounding_text = surrounding;
            } else {
                warn!("discarding IME surrounding text update without capability enabled.");
            }
        }
    }

    pub fn content_type(&self) -> Option<ContentType> {
        self.capabilities.hint_and_purpose().then_some(self.content_type)
    }

    pub fn cursor_area(&self) -> Option<(LogicalPosition<u32>, LogicalSize<u32>)> {
        self.capabilities.cursor_area().then_some(self.cursor_area)
    }

    pub fn surrounding_text(&self) -> Option<&ImeSurroundingText> {
        self.capabilities.surrounding_text().then_some(&self.surrounding_text)
    }
}

/// Arguments to content_type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentType {
    /// Text input hint.
    hint: ContentHint,
    /// Text input purpose.
    purpose: ContentPurpose,
}

/// The two options influence each other, so they must be converted together.
impl From<(ImeHint, ImePurpose)> for ContentType {
    fn from((hint, purpose): (ImeHint, ImePurpose)) -> Self {
        let purpose = match purpose {
            ImePurpose::Password => ContentPurpose::Password,
            ImePurpose::Terminal => ContentPurpose::Terminal,
            ImePurpose::Phone => ContentPurpose::Phone,
            ImePurpose::Number => ContentPurpose::Number,
            ImePurpose::Url => ContentPurpose::Url,
            ImePurpose::Email => ContentPurpose::Email,
            ImePurpose::Pin => ContentPurpose::Pin,
            ImePurpose::Date => ContentPurpose::Date,
            ImePurpose::Time => ContentPurpose::Time,
            ImePurpose::DateTime => ContentPurpose::Datetime,
            _ => ContentPurpose::Normal,
        };

        let base_hint = match purpose {
            // Before the hint API was introduced, password  purpose guaranteed the
            // sensitive hint. Keep this behaviour for the sake of backwards compatibility.
            ContentPurpose::Password | ContentPurpose::Pin => ContentHint::SensitiveData,
            _ => ContentHint::None,
        };

        let mut new_hint = base_hint;
        if hint.contains(ImeHint::COMPLETION) {
            new_hint |= ContentHint::Completion;
        }
        if hint.contains(ImeHint::SPELLCHECK) {
            new_hint |= ContentHint::Spellcheck;
        }
        if hint.contains(ImeHint::AUTO_CAPITALIZATION) {
            new_hint |= ContentHint::AutoCapitalization;
        }
        if hint.contains(ImeHint::LOWERCASE) {
            new_hint |= ContentHint::Lowercase;
        }
        if hint.contains(ImeHint::UPPERCASE) {
            new_hint |= ContentHint::Uppercase;
        }
        if hint.contains(ImeHint::TITLECASE) {
            new_hint |= ContentHint::Titlecase;
        }
        if hint.contains(ImeHint::HIDDEN_TEXT) {
            new_hint |= ContentHint::HiddenText;
        }
        if hint.contains(ImeHint::SENSITIVE_DATA) {
            new_hint |= ContentHint::SensitiveData;
        }
        if hint.contains(ImeHint::LATIN) {
            new_hint |= ContentHint::Latin;
        }
        if hint.contains(ImeHint::MULTILINE) {
            new_hint |= ContentHint::Multiline;
        }

        Self { hint: new_hint, purpose }
    }
}

impl Default for ContentType {
    fn default() -> Self {
        ContentType { purpose: ContentPurpose::Normal, hint: ContentHint::None }
    }
}

delegate_dispatch!(WinitState: [ZwpTextInputManagerV3: GlobalData] => TextInputState);
delegate_dispatch!(WinitState: [ZwpTextInputV3: TextInputData] => TextInputState);

#[cfg(test)]
mod tests {
    use winit_core::window::ImeSurroundingText;

    use super::Sync;

    fn surrounding(text: &str, cursor: usize, anchor: usize) -> ImeSurroundingText {
        ImeSurroundingText::new(text.into(), cursor, anchor).unwrap()
    }

    #[test]
    fn state_waits_for_the_input_method_to_catch_up() {
        let mut sync = Sync::default();
        assert!(sync.may_send());
        sync.committed(Some(surrounding("abcdef", 6, 6)));

        // Commit 1 is not acknowledged yet: the caret move waits.
        assert!(!sync.may_send());
        assert!(!sync.flush_due());

        // A `done` for an older commit changes nothing.
        sync.done(0);
        assert!(!sync.flush_due());

        sync.done(1);
        assert!(sync.flush_due());
        sync.committed(Some(surrounding("abcdef", 4, 4)));
        assert!(!sync.flush_due());
        assert_eq!(sync.surrounding_text(), Some(&surrounding("abcdef", 4, 4)));
    }

    #[test]
    fn the_serial_wraps_with_the_commit_count() {
        let mut sync = Sync { commits: u32::MAX, done_serial: u32::MAX, ..Sync::default() };
        sync.committed(None);
        assert_eq!(sync.commits, 0);
        assert!(!sync.may_send());
        sync.done(0);
        assert!(sync.may_send());
    }

    #[test]
    fn deletions_count_from_the_text_the_input_method_knows() {
        let mut sync = Sync::default();
        sync.committed(Some(surrounding("abcdef", 6, 6)));

        // Delete "ef", then commit "X": a second deletion counts from after the "X".
        sync.edited(Some((2, 0)), Some("X"), false);
        assert_eq!(sync.surrounding_text(), Some(&surrounding("abcdX", 5, 5)));

        // A composition replaces the selection and is not surrounding text.
        sync.committed(Some(surrounding("abcdef", 2, 4)));
        sync.edited(None, None, true);
        assert_eq!(sync.surrounding_text(), Some(&surrounding("abef", 2, 2)));

        // A preedit update without edits leaves the text alone.
        sync.edited(None, None, false);
        assert_eq!(sync.surrounding_text(), Some(&surrounding("abef", 2, 2)));
    }

    #[test]
    fn edits_that_do_not_fit_forget_the_text() {
        let mut sync = Sync::default();
        sync.committed(Some(surrounding("ab", 1, 1)));
        sync.edited(Some((2, 0)), None, false);
        assert_eq!(sync.surrounding_text(), None);

        sync.committed(Some(surrounding("\u{e9}", 2, 2)));
        sync.edited(Some((1, 0)), None, false);
        assert_eq!(sync.surrounding_text(), None, "a deletion inside a character");
    }
}
