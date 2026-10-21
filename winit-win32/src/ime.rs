//! Windows IMM context access and per-window composition key ownership.
//! Callers capture queue state at native composition boundaries and classify
//! keys before reentrant event assembly. This module does not own application
//! text, focus policy, or deferred application actions.

use std::{
    ffi::{OsString, c_void},
    io,
    os::windows::prelude::OsStringExt,
    ptr::null_mut,
};

use dpi::{Position, Size};
use windows_sys::Win32::{
    Foundation::{POINT, RECT},
    UI::{
        Input::{
            Ime::{
                ATTR_TARGET_CONVERTED, ATTR_TARGET_NOTCONVERTED, CANDIDATEFORM, CFS_EXCLUDE,
                CFS_POINT, COMPOSITIONFORM, GCS_COMPATTR, GCS_COMPSTR, GCS_CURSORPOS,
                GCS_RESULTSTR, HIMC, IACE_CHILDREN, IACE_DEFAULT, ImmAssociateContextEx,
                ImmGetCompositionStringW, ImmGetContext, ImmReleaseContext, ImmSetCandidateWindow,
                ImmSetCompositionWindow,
            },
            KeyboardAndMouse::{GetKeyboardState, VK_PROCESSKEY},
        },
        WindowsAndMessaging::{GetSystemMetrics, SM_IMMENABLED},
    },
};
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
/// hardware state). IMM can update this before forwarding a normal key message.
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

    fn remove(&mut self, vkey: u16) -> bool {
        let vkey = usize::from(vkey as u8);
        let word = &mut self.0[vkey / u64::BITS as usize];
        let mask = 1 << (vkey % u64::BITS as usize);
        let contained = *word & mask != 0;
        *word &= !mask;
        contained
    }
}

/// Tracks keys withheld for composition until their matching physical release.
/// This belongs to one window's keyboard builder and performs no native queries.
#[derive(Default)]
pub(crate) struct ImeKeyState {
    withheld: [u64; IME_KEY_WORDS],
    // None means there is no active native composition. The held-key baseline
    // is captured once per session and releases remove keys from that baseline.
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
        if let Some(before) = self.before.take()
            && let Some(held) = held
        {
            // Keep only keys that became pressed during preedit. Existing held
            // modifiers/application keys must retain their ordinary releases.
            for ((captured, held), before) in self.captured.0.iter_mut().zip(held.0).zip(before.0) {
                *captured |= held & !before;
            }
        }
    }

    pub(crate) fn filter_key(
        &mut self,
        vkey: u16,
        scancode: u8,
        extended: bool,
        state: ElementState,
        composing: bool,
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
                // VK_PROCESSKEY starts composition before WM_IME_STARTCOMPOSITION
                // has arrived. Later repeat events may arrive after composition
                // ended; they still belong to the same withheld physical press.
                let withhold = was_withheld || captured || composing || vkey == VK_PROCESSKEY;
                if withhold {
                    *word |= mask;
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
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_PROCESSKEY, VK_RETURN};
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
        assert!(keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, true));

        // WM_IME_ENDCOMPOSITION can run inside key assembly. Repeats and release
        // still belong to the same press even though the IME is now disabled.
        assert!(keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false));
        assert!(keys.filter_key(VK_ESCAPE, ESCAPE, false, Released, false));
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false));
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Released, false));
    }

    #[test]
    fn process_key_is_withheld_before_preedit_and_matches_its_real_release() {
        let mut keys = ImeKeyState::default();
        assert!(keys.filter_key(VK_PROCESSKEY, ENTER, false, Pressed, false));
        assert!(keys.filter_key(VK_RETURN, ENTER, false, Released, false));
        assert!(!keys.filter_key(VK_RETURN, ENTER, false, Pressed, false));
    }

    #[test]
    fn normal_and_extended_keys_keep_independent_press_ownership() {
        let mut keys = ImeKeyState::default();
        assert!(keys.filter_key(VK_RETURN, ENTER, false, Pressed, true));
        assert!(!keys.filter_key(VK_RETURN, ENTER, true, Pressed, false));
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false));
        assert!(!keys.filter_key(VK_RETURN, ENTER, true, Released, false));
        assert!(keys.filter_key(VK_RETURN, ENTER, false, Released, false));
    }

    #[test]
    fn keys_without_scancodes_do_not_collide_with_scanned_keys_or_each_other() {
        let mut keys = ImeKeyState::default();
        assert!(keys.filter_key(VK_RETURN, 0, false, Pressed, true));
        assert!(!keys.filter_key(VK_ESCAPE, 0, false, Pressed, false));
        assert!(!keys.filter_key(VK_RETURN, ENTER, false, Pressed, false));
        assert!(!keys.filter_key(VK_ESCAPE, 0, false, Released, false));
        assert!(keys.filter_key(VK_RETURN, 0, false, Released, false));
    }

    #[test]
    fn a_release_for_an_application_press_is_not_swallowed_when_preedit_begins() {
        let mut keys = ImeKeyState::default();
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false));
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Released, true));
    }

    #[test]
    fn focus_change_clears_key_ownership_when_the_original_release_is_lost() {
        let mut keys = ImeKeyState::default();
        assert!(keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, true));
        keys.clear();
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false));
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
        assert!(keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false));
        assert!(keys.filter_key(VK_ESCAPE, ESCAPE, false, Released, false));
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false));
    }

    #[test]
    fn a_mouse_commit_and_previously_held_keys_cannot_consume_a_fresh_escape() {
        let mut keys = ImeKeyState::default();
        keys.begin_composition(Some(held(&[VK_RETURN])));
        keys.end_composition(Some(held(&[VK_RETURN])));
        assert!(!keys.filter_key(VK_RETURN, ENTER, false, Released, false));
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false));
    }

    #[test]
    fn a_key_released_and_repressed_during_preedit_is_a_new_composition_press() {
        let mut keys = ImeKeyState::default();
        keys.begin_composition(Some(held(&[VK_ESCAPE])));
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Released, true));
        keys.end_composition(Some(held(&[VK_ESCAPE])));
        assert!(keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false));
    }

    #[test]
    fn an_ime_consumed_press_with_only_a_release_does_not_eat_the_next_press() {
        let mut keys = ImeKeyState::default();
        keys.begin_composition(Some(held(&[])));
        keys.end_composition(Some(held(&[VK_ESCAPE])));
        assert!(keys.filter_key(VK_ESCAPE, ESCAPE, false, Released, false));
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false));
    }

    #[test]
    fn failed_native_snapshots_do_not_reuse_a_previous_composition_baseline() {
        let mut keys = ImeKeyState::default();
        keys.begin_composition(Some(held(&[])));
        keys.end_composition(None);
        keys.end_composition(Some(held(&[VK_ESCAPE])));
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false));

        keys.begin_composition(None);
        keys.end_composition(Some(held(&[VK_ESCAPE])));
        assert!(!keys.filter_key(VK_ESCAPE, ESCAPE, false, Pressed, false));
    }
}
