//! Windows IMM context access and per-window composition key ownership.
//! Callers capture queue state at native composition boundaries and classify
//! keys before reentrant event assembly. This module does not own application
//! text, focus policy, or deferred application actions.

use std::ffi::{OsString, c_void};
use std::io;
use std::os::windows::prelude::OsStringExt;
use std::ptr::null_mut;

use dpi::{Position, Size};
use windows_sys::Win32::Foundation::{POINT, RECT};
use windows_sys::Win32::UI::Input::Ime::{
    ATTR_TARGET_CONVERTED, ATTR_TARGET_NOTCONVERTED, CANDIDATEFORM, CFS_EXCLUDE, CFS_POINT,
    COMPOSITIONFORM, GCS_COMPATTR, GCS_COMPSTR, GCS_CURSORPOS, GCS_RESULTSTR, HIMC, IACE_CHILDREN,
    IACE_DEFAULT, ImmAssociateContextEx, ImmGetCompositionStringW, ImmGetContext,
    ImmReleaseContext, ImmSetCandidateWindow, ImmSetCompositionWindow,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetKeyboardState, VK_ESCAPE, VK_PROCESSKEY};
use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_IMMENABLED};
use winit_core::event::ElementState;

use crate::HWND;

pub struct ImeContext {
    hwnd: HWND,
    himc: HIMC,
}

impl ImeContext {
    pub unsafe fn current(hwnd: HWND) -> Self {
        let himc = unsafe { ImmGetContext(hwnd) };
        ImeContext { hwnd, himc }
    }

    pub unsafe fn get_composing_text_and_cursor(
        &self,
    ) -> Option<(String, Option<usize>, Option<usize>)> {
        let text = unsafe { self.get_composition_string(GCS_COMPSTR) }?;
        let attrs = unsafe { self.get_composition_data(GCS_COMPATTR) }.unwrap_or_default();

        let mut first = None;
        let mut last = None;
        let mut boundary_before_char = 0;
        let mut attr_idx = 0;

        for chr in text.chars() {
            let Some(attr) = attrs.get(attr_idx).copied() else {
                break;
            };

            let char_is_targeted =
                attr as u32 == ATTR_TARGET_CONVERTED || attr as u32 == ATTR_TARGET_NOTCONVERTED;

            if first.is_none() && char_is_targeted {
                first = Some(boundary_before_char);
            } else if first.is_some() && last.is_none() && !char_is_targeted {
                last = Some(boundary_before_char);
            }

            boundary_before_char += chr.len_utf8();
            attr_idx += chr.len_utf16();
        }

        if first.is_some() && last.is_none() {
            last = Some(text.len());
        } else if first.is_none() {
            // IME haven't split words and select any clause yet, so trying to retrieve normal
            // cursor.
            let cursor = unsafe { self.get_composition_cursor(&text) };
            first = cursor;
            last = cursor;
        }

        Some((text, first, last))
    }

    pub unsafe fn get_composed_text(&self) -> Option<String> {
        unsafe { self.get_composition_string(GCS_RESULTSTR) }
    }

    unsafe fn get_composition_cursor(&self, text: &str) -> Option<usize> {
        let cursor = unsafe { ImmGetCompositionStringW(self.himc, GCS_CURSORPOS, null_mut(), 0) };
        (cursor >= 0).then(|| text.chars().take(cursor as _).map(|c| c.len_utf8()).sum())
    }

    unsafe fn get_composition_string(&self, gcs_mode: u32) -> Option<String> {
        let data = unsafe { self.get_composition_data(gcs_mode) }?;
        let (prefix, shorts, suffix) = unsafe { data.align_to::<u16>() };
        if prefix.is_empty() && suffix.is_empty() {
            OsString::from_wide(shorts).into_string().ok()
        } else {
            None
        }
    }

    unsafe fn get_composition_data(&self, gcs_mode: u32) -> Option<Vec<u8>> {
        let size = match unsafe { ImmGetCompositionStringW(self.himc, gcs_mode, null_mut(), 0) } {
            0 => return Some(Vec::new()),
            size if size < 0 => return None,
            size => size,
        };

        let mut buf = Vec::<u8>::with_capacity(size as _);
        let size = unsafe {
            ImmGetCompositionStringW(
                self.himc,
                gcs_mode,
                buf.as_mut_ptr() as *mut c_void,
                size as _,
            )
        };

        if size < 0 {
            None
        } else {
            unsafe { buf.set_len(size as _) };
            Some(buf)
        }
    }

    pub unsafe fn set_ime_cursor_area(&self, spot: Position, size: Size, scale_factor: f64) {
        if !unsafe { ImeContext::system_has_ime() } {
            return;
        }

        let (x, y) = spot.to_physical::<i32>(scale_factor).into();
        let (width, height): (i32, i32) = size.to_physical::<i32>(scale_factor).into();
        let rc_area = RECT { left: x, top: y, right: x + width, bottom: y + height };
        let candidate_form = CANDIDATEFORM {
            dwIndex: 0,
            dwStyle: CFS_EXCLUDE,
            ptCurrentPos: POINT { x, y },
            rcArea: rc_area,
        };
        let composition_form = COMPOSITIONFORM {
            dwStyle: CFS_POINT,
            ptCurrentPos: POINT { x, y: y + height },
            rcArea: rc_area,
        };

        unsafe {
            ImmSetCompositionWindow(self.himc, &composition_form);
            ImmSetCandidateWindow(self.himc, &candidate_form);
        }
    }

    pub unsafe fn set_ime_allowed(hwnd: HWND, allowed: bool) {
        if !unsafe { ImeContext::system_has_ime() } {
            return;
        }

        if allowed {
            unsafe { ImmAssociateContextEx(hwnd, null_mut(), IACE_DEFAULT) };
        } else {
            unsafe { ImmAssociateContextEx(hwnd, null_mut(), IACE_CHILDREN) };
        }
    }

    pub unsafe fn system_has_ime() -> bool {
        unsafe { GetSystemMetrics(SM_IMMENABLED) != 0 }
    }
}

impl Drop for ImeContext {
    fn drop(&mut self) {
        unsafe { ImmReleaseContext(self.hwnd, self.himc) };
    }
}

// Scancodes occupy one byte plus an extended-key bit. Reserve another byte-sized
// range for virtual keys without a scancode, as used by some media/input devices.
const IME_KEY_RANGE: usize = u8::MAX as usize + 1;
const IME_KEY_WORDS: usize = 3 * IME_KEY_RANGE / u64::BITS as usize;

/// Down virtual keys in the current thread's input queue (not asynchronous
/// hardware state). In the Microsoft Korean IME traces, this already includes
/// the composition-ending press before its ordinary key message is forwarded.
/// That ordering is an observation, not a Windows guarantee of IME ownership.
/// A down bit identifies queue state, not whether IMM consumed that press.
/// https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-getkeyboardstate
#[derive(Clone, Copy, Default)]
pub(crate) struct PressedKeys([u64; IME_KEY_RANGE / u64::BITS as usize]);

impl PressedKeys {
    pub(crate) fn capture() -> io::Result<Self> {
        let mut state = [0; IME_KEY_RANGE];
        // SAFETY: GetKeyboardState writes exactly 256 bytes to this live buffer
        // and retains no pointer. The caller runs on the event-loop thread.
        if unsafe { GetKeyboardState(state.as_mut_ptr()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self::from_native_state(state))
    }

    /// Ignore toggle bits: only keys currently held belong to a physical press.
    pub(crate) fn from_native_state(state: [u8; IME_KEY_RANGE]) -> Self {
        const DOWN_BIT: u8 = 1 << 7;
        let mut keys = Self::default();
        for (vkey, value) in state.into_iter().enumerate() {
            if value & DOWN_BIT != 0 {
                keys.insert(vkey as u16);
            }
        }
        keys
    }

    fn insert(&mut self, vkey: u16) {
        let vkey = usize::from(vkey as u8);
        self.0[vkey / u64::BITS as usize] |= 1 << (vkey % u64::BITS as usize);
    }

    fn contains(&self, vkey: u16) -> bool {
        let vkey = usize::from(vkey as u8);
        self.0[vkey / u64::BITS as usize] & (1 << (vkey % u64::BITS as usize)) != 0
    }

    fn remove(&mut self, vkey: u16) -> bool {
        let vkey = usize::from(vkey as u8);
        let word = &mut self.0[vkey / u64::BITS as usize];
        let mask = 1 << (vkey % u64::BITS as usize);
        let contained = *word & mask != 0;
        *word &= !mask;
        contained
    }
}

/// Tracks withheld composition keys until release or a native editing handoff.
/// This belongs to one window's keyboard builder and performs no native queries.
#[derive(Default)]
pub(crate) struct ImeKeyState {
    withheld: [u64; IME_KEY_WORDS],
    // None disables boundary inference outside composition or after a failed
    // starting snapshot. The held-key baseline
    // includes keys held at its start and real presses already delivered to
    // the application. Their releases must not be inferred as composition-owned.
    before: Option<PressedKeys>,
    captured: PressedKeys,
}

impl ImeKeyState {
    pub(crate) fn clear(&mut self) {
        self.withheld.fill(0);
        self.before = None;
        self.captured = PressedKeys::default();
    }

    pub(crate) fn begin_composition(&mut self, held: Option<PressedKeys>) {
        self.before = held;
    }

    pub(crate) fn end_composition(&mut self, held: Option<PressedKeys>) {
        match (self.before.take(), held) {
            (Some(before), Some(held))
                if held.contains(VK_ESCAPE) && !before.contains(VK_ESCAPE) =>
            {
                // Korean IMM forwards the Escape that cancels composition only
                // after ending preedit, so retain that cancellation press. A held
                // key snapshot alone does not prove that the IME consumed a key:
                // Space, Enter and navigation can intentionally continue editing
                // after a commit. Their ordinary forwarded messages must survive.
                // Keys observed during preedit or as VK_PROCESSKEY remain owned by
                // the composition through the normal scancode tracking below.
                self.captured.insert(VK_ESCAPE);
            },
            _ => {},
        }
    }

    pub(crate) fn filter_key(
        &mut self,
        vkey: u16,
        scancode: u8,
        extended: bool,
        state: ElementState,
        composing: bool,
        is_repeat: bool,
    ) -> bool {
        let captured = self.captured.remove(vkey);
        let key = if scancode == 0 {
            // Windows virtual-key codes have an eight-bit namespace.
            2 * IME_KEY_RANGE + usize::from(vkey as u8)
        } else {
            IME_KEY_RANGE * usize::from(extended) + usize::from(scancode)
        };
        let word = &mut self.withheld[key / u64::BITS as usize];
        let mask = 1 << (key % u64::BITS as usize);
        let was_withheld = *word & mask != 0;

        match state {
            ElementState::Pressed => {
                // VK_PROCESSKEY can precede WM_IME_STARTCOMPOSITION. Retain a
                // canceling Escape across repeats so holding it cannot also dismiss
                // application UI after canceling composition. Other ordinary repeats
                // after preedit are a native handoff back to editing: Korean IMM,
                // for example, forwards Backspace after deleting the final jamo.
                // Clear ownership on that handoff so repeats and release both reach
                // the application, while PROCESSKEY and active preedit stay withheld.
                //
                // An IME may also consume a release, as observed for the Japanese
                // input-mode toggle. The previous-state bit keeps stale ownership
                // from extending to a fresh press without any focus transition.
                // https://learn.microsoft.com/en-us/windows/win32/inputdev/wm-keydown
                let retain_cancellation = was_withheld && is_repeat && vkey == VK_ESCAPE;
                let withhold =
                    retain_cancellation || captured || composing || vkey == VK_PROCESSKEY;
                if withhold {
                    *word |= mask;
                } else {
                    *word &= !mask;
                    if let Some(before) = &mut self.before {
                        // A real press already delivered during this session belongs
                        // to the application. Exclude it from later completion-boundary
                        // inference so its matching release cannot be swallowed.
                        before.insert(vkey);
                    }
                }
                withhold
            },
            ElementState::Released => {
                // VK_PROCESSKEY on keydown becomes the actual virtual key on
                // keyup, so match by scancode rather than by virtual-key code.
                *word &= !mask;
                if let Some(before) = &mut self.before {
                    before.remove(vkey);
                }
                was_withheld || captured
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        VK_A, VK_BACK, VK_ESCAPE, VK_LEFT, VK_PROCESSKEY, VK_RETURN, VK_SPACE, VK_TAB,
    };
    use winit_core::event::ElementState::{Pressed, Released};

    use crate::ime::{IME_KEY_RANGE, ImeKeyState, PressedKeys};

    // Set-1 hardware scancodes from the Windows keyboard scan-code specification.
    const ESCAPE: u8 = 0x01;
    const ENTER: u8 = 0x1c;

    #[test]
    fn native_toggle_bits_do_not_capture_a_key_as_pressed() {
        // GetKeyboardState uses bit 7 for held state and bit 0 for toggled state.
        const KEY_DOWN: u8 = 1 << 7;
        const TOGGLED: u8 = 1;
        let mut native = [0; IME_KEY_RANGE];
        native[VK_ESCAPE as usize] = TOGGLED;
        native[VK_RETURN as usize] = KEY_DOWN | TOGGLED;
        let mut held = PressedKeys::from_native_state(native);

        assert!(!held.remove(VK_ESCAPE));
        assert!(held.remove(VK_RETURN));
    }

    #[test]
    fn composition_ending_escape_and_its_repeats_release_before_a_fresh_escape() {
        let mut keys = ImeKeyState::default();
        assert!(keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, true, false));

        // WM_IME_ENDCOMPOSITION can run inside key assembly. Repeats and release
        // still belong to the same press even though the IME is now disabled.
        assert!(keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false, true));
        assert!(keys.filter_key(VK_ESCAPE, ESCAPE, false, Released, false, false));
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false, false));
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Released, false, false));
    }

    #[test]
    fn fresh_press_recovers_ownership_when_an_ime_consumed_the_previous_release() {
        // The Japanese input-mode toggle can consume a release without a focus
        // transition. A fresh native press is a new lifetime, including fallback
        // virtual-key slots and extended keys.
        for (scancode, extended) in [(ENTER, false), (ENTER, true), (0, false)] {
            let mut keys = ImeKeyState::default();
            assert!(keys.filter_key(VK_RETURN, scancode, extended, Pressed, true, false));

            assert!(!keys.filter_key(VK_RETURN, scancode, extended, Pressed, false, false));
            assert!(!keys.filter_key(VK_RETURN, scancode, extended, Released, false, false));
        }
    }

    #[test]
    fn ordinary_repeats_resume_editing_after_composition_ends() {
        // Korean IMM returns ordinary Backspace repeats after deleting the last
        // jamo. Keeping the entire hold owned would stop deletion at that boundary.
        // The same handoff applies to other ordinary forwarded editing keys.
        for (vkey, scancode, extended) in [
            (VK_BACK, 0x0e, false),
            (VK_A, 0x1e, false),
            (VK_RETURN, ENTER, false),
            (VK_LEFT, 0x4b, true),
        ] {
            let mut keys = ImeKeyState::default();
            assert!(keys.filter_key(VK_PROCESSKEY, scancode, extended, Pressed, true, false));
            assert!(keys.filter_key(vkey, scancode, extended, Pressed, true, true));

            assert!(!keys.filter_key(vkey, scancode, extended, Pressed, false, true));
            assert!(!keys.filter_key(vkey, scancode, extended, Pressed, false, true));
            assert!(!keys.filter_key(vkey, scancode, extended, Released, false, false));
        }
    }

    #[test]
    fn process_key_is_withheld_before_preedit_and_matches_its_real_release() {
        let mut keys = ImeKeyState::default();
        assert!(keys.filter_key(VK_PROCESSKEY, ENTER, false, Pressed, false, false));
        assert!(keys.filter_key(VK_RETURN, ENTER, false, Released, false, false));
        assert!(!keys.filter_key(VK_RETURN, ENTER, false, Pressed, false, false));
    }

    #[test]
    fn normal_and_extended_keys_keep_independent_press_ownership() {
        let mut keys = ImeKeyState::default();
        assert!(keys.filter_key(VK_RETURN, ENTER, false, Pressed, true, false));
        assert!(!keys.filter_key(VK_RETURN, ENTER, true, Pressed, false, false));
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false, false));
        assert!(!keys.filter_key(VK_RETURN, ENTER, true, Released, false, false));
        assert!(keys.filter_key(VK_RETURN, ENTER, false, Released, false, false));
    }

    #[test]
    fn keys_without_scancodes_do_not_collide_with_scanned_keys_or_each_other() {
        let mut keys = ImeKeyState::default();
        assert!(keys.filter_key(VK_RETURN, 0, false, Pressed, true, false));
        assert!(!keys.filter_key(VK_ESCAPE, 0, false, Pressed, false, false));
        assert!(!keys.filter_key(VK_RETURN, ENTER, false, Pressed, false, false));
        assert!(!keys.filter_key(VK_ESCAPE, 0, false, Released, false, false));
        assert!(keys.filter_key(VK_RETURN, 0, false, Released, false, false));
    }

    #[test]
    fn a_release_for_an_application_press_is_not_swallowed_when_preedit_begins() {
        let mut keys = ImeKeyState::default();
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false, false));
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Released, true, false));
    }

    #[test]
    fn focus_change_clears_key_ownership_when_the_original_release_is_lost() {
        let mut keys = ImeKeyState::default();
        assert!(keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, true, false));
        keys.clear();
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false, false));
    }

    fn held(keys: &[u16]) -> PressedKeys {
        let mut pressed = PressedKeys::default();
        for &key in keys {
            pressed.insert(key);
        }
        pressed
    }

    #[test]
    fn forwarded_escape_is_owned_by_the_native_press_at_composition_end() {
        let mut keys = ImeKeyState::default();
        keys.begin_composition(Some(held(&[])));
        keys.end_composition(Some(held(&[VK_ESCAPE])));
        assert!(keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false, false));
        assert!(keys.filter_key(VK_ESCAPE, ESCAPE, false, Released, false, false));
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false, false));
    }

    #[test]
    fn composition_completion_preserves_forwarded_text_and_navigation() {
        // Set-1 scancodes identify the original press and its matching release.
        // Korean commits before forwarding Space; other normal keys can also
        // intentionally continue editing or navigation after the same boundary.
        for (vkey, scancode, extended) in [
            (VK_SPACE, 0x39, false),
            (VK_RETURN, ENTER, false),
            (VK_TAB, 0x0f, false),
            (VK_LEFT, 0x4b, true),
            (VK_A, 0x1e, false),
        ] {
            let mut keys = ImeKeyState::default();
            keys.begin_composition(Some(held(&[])));
            keys.end_composition(Some(held(&[vkey])));

            assert!(!keys.filter_key(vkey, scancode, extended, Pressed, false, false));
            assert!(!keys.filter_key(vkey, scancode, extended, Pressed, false, true));
            assert!(!keys.filter_key(vkey, scancode, extended, Released, false, false));
        }
    }

    #[test]
    fn a_press_already_delivered_during_composition_keeps_its_release() {
        let mut keys = ImeKeyState::default();
        keys.begin_composition(Some(held(&[])));

        // A session may be Enabled before its first nonempty preedit or after
        // result text. This ordinary press has already reached the application.
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false, false));
        keys.end_composition(Some(held(&[VK_ESCAPE])));

        // The completion snapshot must not retrospectively take that press.
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Released, false, false));
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false, false));
    }

    #[test]
    fn candidate_space_and_process_key_enter_stay_owned_by_composition() {
        // Japanese candidate navigation still belongs to an active preedit.
        // A PROCESSKEY press keeps its release when composition ends without
        // an ordinary forwarded editing repeat handing input back first.
        const SPACE: u8 = 0x39;
        let mut keys = ImeKeyState::default();
        keys.begin_composition(Some(held(&[])));
        assert!(keys.filter_key(VK_SPACE, SPACE, false, Pressed, true, false));
        assert!(keys.filter_key(VK_SPACE, SPACE, false, Released, true, false));
        assert!(keys.filter_key(VK_PROCESSKEY, ENTER, false, Pressed, true, false));
        keys.end_composition(Some(held(&[VK_RETURN])));

        assert!(keys.filter_key(VK_RETURN, ENTER, false, Released, false, false));
        assert!(!keys.filter_key(VK_RETURN, ENTER, false, Pressed, false, false));
    }

    #[test]
    fn a_mouse_commit_and_previously_held_keys_cannot_consume_a_fresh_escape() {
        let mut keys = ImeKeyState::default();
        keys.begin_composition(Some(held(&[VK_RETURN])));
        keys.end_composition(Some(held(&[VK_RETURN])));
        assert!(!keys.filter_key(VK_RETURN, ENTER, false, Released, false, false));
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false, false));
    }

    #[test]
    fn a_key_released_and_repressed_during_preedit_is_a_new_composition_press() {
        let mut keys = ImeKeyState::default();
        keys.begin_composition(Some(held(&[VK_ESCAPE])));
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Released, true, false));
        keys.end_composition(Some(held(&[VK_ESCAPE])));
        assert!(keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false, false));
    }

    #[test]
    fn an_ime_consumed_press_with_only_a_release_does_not_eat_the_next_press() {
        let mut keys = ImeKeyState::default();
        keys.begin_composition(Some(held(&[])));
        keys.end_composition(Some(held(&[VK_ESCAPE])));
        assert!(keys.filter_key(VK_ESCAPE, ESCAPE, false, Released, false, false));
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false, false));
    }

    #[test]
    fn failed_native_snapshots_do_not_reuse_a_previous_composition_baseline() {
        let mut keys = ImeKeyState::default();
        keys.begin_composition(Some(held(&[])));
        keys.end_composition(None);
        keys.end_composition(Some(held(&[VK_ESCAPE])));
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false, false));

        keys.begin_composition(None);
        keys.end_composition(Some(held(&[VK_ESCAPE])));
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false, false));
    }
}
