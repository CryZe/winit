//! Geometry for an event-driven borderless resize gesture in AppKit screen points.
//!
//! The view owns mouse capture and native window updates. Keeping the original
//! frame and pointer position here avoids drift when the left or bottom edge
//! moves, including when a constraint stops the frame before the pointer stops.

use objc2_foundation::{NSPoint, NSRect, NSSize};
use winit_core::window::ResizeDirection;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ResizeDrag {
    direction: ResizeDirection,
    frame: NSRect,
    pointer: NSPoint,
    increments: NSSize,
}

impl ResizeDrag {
    pub(crate) fn new(
        direction: ResizeDirection,
        frame: NSRect,
        pointer: NSPoint,
        increments: NSSize,
    ) -> Self {
        Self { direction, frame, pointer, increments }
    }

    pub(crate) fn frame_at(self, pointer: NSPoint, minimum: NSSize, maximum: NSSize) -> NSRect {
        use ResizeDirection::{
            East, North, NorthEast, NorthWest, South, SouthEast, SouthWest, West,
        };

        let mut frame = self.frame;
        let delta = NSPoint::new(pointer.x - self.pointer.x, pointer.y - self.pointer.y);
        let west = matches!(self.direction, West | NorthWest | SouthWest);
        let east = matches!(self.direction, East | NorthEast | SouthEast);
        let south = matches!(self.direction, South | SouthWest | SouthEast);
        let north = matches!(self.direction, North | NorthWest | NorthEast);

        if west || east {
            frame.size.width = constrained_size(
                self.frame.size.width + if west { -delta.x } else { delta.x },
                minimum.width,
                maximum.width,
                self.increments.width,
            );
            if west {
                frame.origin.x += self.frame.size.width - frame.size.width;
            }
        }

        if south || north {
            // AppKit screen coordinates grow upwards, unlike winit surface coordinates.
            frame.size.height = constrained_size(
                self.frame.size.height + if south { -delta.y } else { delta.y },
                minimum.height,
                maximum.height,
                self.increments.height,
            );
            if south {
                frame.origin.y += self.frame.size.height - frame.size.height;
            }
        }

        frame
    }
}

fn constrained_size(requested: f64, minimum: f64, maximum: f64, increment: f64) -> f64 {
    // AppKit permits a zero minimum; retain at least one point so the window
    // remains drawable. Conflicting size hints must not make clamp panic.
    let minimum = minimum.max(1.0);
    let maximum = maximum.max(minimum);
    let requested = requested.clamp(minimum, maximum);
    if increment.is_finite() && increment >= 1.0 {
        ((requested / increment).round() * increment).clamp(minimum, maximum)
    } else {
        requested
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit_core::window::ResizeDirection::*;

    fn frame() -> NSRect {
        NSRect::new(NSPoint::new(-300.0, 100.0), NSSize::new(200.0, 150.0))
    }

    fn drag(direction: ResizeDirection) -> ResizeDrag {
        ResizeDrag::new(direction, frame(), NSPoint::new(20.0, 30.0), NSSize::new(1.0, 1.0))
    }

    #[test]
    fn all_edges_and_corners_anchor_the_opposite_edges() {
        let cases = [
            (North, (-300.0, 100.0, 200.0, 170.0)),
            (NorthEast, (-300.0, 100.0, 210.0, 170.0)),
            (East, (-300.0, 100.0, 210.0, 150.0)),
            (SouthEast, (-300.0, 120.0, 210.0, 130.0)),
            (South, (-300.0, 120.0, 200.0, 130.0)),
            (SouthWest, (-290.0, 120.0, 190.0, 130.0)),
            (West, (-290.0, 100.0, 190.0, 150.0)),
            (NorthWest, (-290.0, 100.0, 190.0, 170.0)),
        ];
        for (direction, (x, y, width, height)) in cases {
            assert_eq!(
                drag(direction).frame_at(
                    NSPoint::new(30.0, 50.0),
                    NSSize::new(0.0, 0.0),
                    NSSize::new(1000.0, 1000.0),
                ),
                NSRect::new(NSPoint::new(x, y), NSSize::new(width, height)),
                "{direction:?}",
            );
        }
    }

    #[test]
    fn constraints_keep_the_fixed_edges_and_do_not_accumulate_drag_error() {
        let drag = drag(SouthWest);
        let minimum = NSSize::new(80.0, 60.0);
        let maximum = NSSize::new(250.0, 200.0);
        assert_eq!(
            drag.frame_at(NSPoint::new(1000.0, 1000.0), minimum, maximum),
            NSRect::new(NSPoint::new(-180.0, 190.0), minimum),
        );
        assert_eq!(
            drag.frame_at(NSPoint::new(-1000.0, -1000.0), minimum, maximum),
            NSRect::new(NSPoint::new(-350.0, 50.0), maximum),
        );
        assert_eq!(drag.frame_at(NSPoint::new(20.0, 30.0), minimum, maximum), frame());
    }

    #[test]
    fn increments_preserve_opposite_edges_and_obey_non_aligned_limits() {
        let drag =
            ResizeDrag::new(SouthWest, frame(), NSPoint::new(0.0, 0.0), NSSize::new(20.0, 10.0));
        assert_eq!(
            drag.frame_at(
                NSPoint::new(13.0, 13.0),
                NSSize::new(81.0, 61.0),
                NSSize::new(251.0, 201.0),
            ),
            NSRect::new(NSPoint::new(-280.0, 110.0), NSSize::new(180.0, 140.0)),
        );
    }

    #[test]
    fn logical_screen_points_preserve_fractional_positions_on_retina() {
        let drag = ResizeDrag::new(East, frame(), NSPoint::new(20.0, 30.0), NSSize::new(0.0, 0.0));
        let actual = drag.frame_at(
            NSPoint::new(20.5, 30.0),
            NSSize::new(0.0, 0.0),
            NSSize::new(1000.0, 1000.0),
        );
        assert_eq!(actual.size.width, 200.5);
        assert_eq!(actual.origin, frame().origin);
    }

    #[test]
    fn disabled_increments_and_conflicting_limits_remain_safe() {
        assert_eq!(constrained_size(10.5, 0.0, 100.0, 0.0), 10.5);
        assert_eq!(constrained_size(-100.0, 0.0, 0.0, f64::NAN), 1.0);
        assert_eq!(constrained_size(500.0, 80.0, 60.0, 10.0), 80.0);
        assert_eq!(constrained_size(500.0, 81.0, 251.0, 20.0), 251.0);
    }
}
