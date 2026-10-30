The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/).

The sections should follow the order `Added`, `Changed`, `Deprecated`,
`Removed`, and `Fixed`.

Platform specific changed should be added to the end of the section and grouped
by platform name. Common API additions should have `, implemented` at the end
for platforms where the API was initially implemented. See the following example
on how to add them:

```md
### Added

- Add `Window::turbo()`, implemented on X11, Wayland, and Web.
- On X11, add `Window::some_rare_api`.
- On X11, add `Window::even_more_rare_api`.
- On Wayland, add `Window::common_api`.
- On Windows, add `Window::some_rare_api`.
```

When the change requires non-trivial amount of work for users to comply
with it, the migration guide should be added below the entry, like:

```md
- Deprecate `Window` creation outside of `EventLoop::run`

  This was done to simply migration in the future. Consider the
  following code:

  // Code snippet.

  To migrate it we should do X, Y, and then Z, for example:

  // Code snippet.

```

The migration guide could reference other migration examples in the current
changelog entry.

## Unreleased

### Added

- On macOS, add `WindowExtMacOS::set_needs_panel_to_become_key` and its getter
  to control the view's native click-focus policy, including conditional activation
  of nonactivating panels, while preserving explicit window focus capability.

- On macOS, implement `Window::drag_resize_window` for resizable borderless
  windows outside fullscreen, preserving size constraints and resize increments.

### Fixed

- On Windows, reapply the requested IME cursor area when native composition starts.
  This restores placement for input methods that discard it, as observed with
  Microsoft Japanese IME after a layout switch.

- On macOS, validate `Window::drag_window` against the originating mouse press
  and live button state before starting a native move. Multi-click presses do
  not start a move, matching AppKit background dragging without its warning.

- On macOS, keep `Window::set_visible(true)` from stealing keyboard focus. Use
  `Window::focus_window` when showing a window should also focus it.

- On Windows, withhold real key presses observed during IME preedit, including
  modifiers, and `VK_PROCESSKEY` presses from `KeyboardInput` through their matching
  releases, allowing ordinary editing repeats after composition hands input back.
  Keep Microsoft Korean IME's delayed composition-ending Escape and its repeats
  from dismissing application UI.
  `ModifiersChanged` and synthetic focus synchronization retain their behavior.

- On Windows, fix a crash occurring when trying to create a DXGI surface on a window that is both fullscreen and always on top.
- On Wayland, use the touch serial for `Window::drag_window`, `Window::drag_resize_window` and `Window::show_window_menu` when the interaction was started by touch.
- On Windows, fix a crash when moving a window that owns windows not created by winit, such as native dialogs or application helper windows.
- On Windows, avoid overflow when negating minimum mouse-wheel deltas.
- On X11, reset the drag and drop state when a drag operation ends. A later drag no longer reuses the state of a previous drag.
- On X11, avoid spurious focus events when mapping windows, which could dismiss
  menus before they receive focus.
- On Wayland, fix the text input being destroyed whenever a seat capability changed. This made IME input stop working in already-running applications after the seat's capabilities were updated.
- On Wayland, fix a panic when a popup is dropped before its queued compositor
  updates are processed.
- On macOS, forward mouse click events to AppKit. This makes IME semantics more similar to native macOS applications, particularly when an IME is clicked out of.

### Changed

- Removed the `url` dependency from `winit-core`, along with `SendData::from_file_paths`,
  `TypedData::try_as_file_paths` and `impl From<Vec<url::Url>> for SendData`. Use `SendData::Uris`
  / `TypedData::try_as_uris` with `file:` URIs instead.
