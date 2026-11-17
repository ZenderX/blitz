//! Rounding of the finished layout onto the device-pixel grid.
//!
//! Layout runs in CSS px, and painting multiplies by the device scale afterwards. Rounding
//! to whole CSS px, as `taffy::round_layout` does, leaves a box edge on a whole device
//! pixel only at a scale of 1. This pass rounds the same way (cumulative edges, sizes as
//! differences of rounded edges, reading `unrounded_layout` and writing `final_layout`)
//! but on a grid of `1 / scale` CSS px, so every edge lands on a whole device pixel. At a
//! scale of 1 the result is that of `taffy::round_layout`.

use taffy::{Layout, NodeId, RoundTree};

/// `value` rounded to the nearest whole device pixel, in CSS px. A scale that is not a
/// positive finite number rounds to whole CSS px.
#[inline]
pub(crate) fn round_to_device(value: f32, scale: f32) -> f32 {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    (value * scale + 0.5).floor() / scale
}

/// Rounds the layout of `node_id` and its descendants onto the device-pixel grid.
pub(crate) fn round_layout_to_device(tree: &mut impl RoundTree, node_id: NodeId, scale: f32) {
    round_node(tree, node_id, 0.0, 0.0, scale);
}

fn round_node(
    tree: &mut impl RoundTree,
    node_id: NodeId,
    cumulative_x: f32,
    cumulative_y: f32,
    scale: f32,
) {
    let unrounded = tree.get_unrounded_layout(node_id);
    let mut layout: Layout = unrounded;
    let round = |value: f32| round_to_device(value, scale);

    let cumulative_x = cumulative_x + unrounded.location.x;
    let cumulative_y = cumulative_y + unrounded.location.y;
    let right = cumulative_x + unrounded.size.width;
    let bottom = cumulative_y + unrounded.size.height;

    layout.location.x = round(unrounded.location.x);
    layout.location.y = round(unrounded.location.y);
    layout.size.width = round(right) - round(cumulative_x);
    layout.size.height = round(bottom) - round(cumulative_y);
    layout.scrollbar_size.width = round(unrounded.scrollbar_size.width);
    layout.scrollbar_size.height = round(unrounded.scrollbar_size.height);
    layout.border.left = round(cumulative_x + unrounded.border.left) - round(cumulative_x);
    layout.border.right = round(right) - round(right - unrounded.border.right);
    layout.border.top = round(cumulative_y + unrounded.border.top) - round(cumulative_y);
    layout.border.bottom = round(bottom) - round(bottom - unrounded.border.bottom);
    layout.padding.left = round(cumulative_x + unrounded.padding.left) - round(cumulative_x);
    layout.padding.right = round(right) - round(right - unrounded.padding.right);
    layout.padding.top = round(cumulative_y + unrounded.padding.top) - round(cumulative_y);
    layout.padding.bottom = round(bottom) - round(bottom - unrounded.padding.bottom);

    let overflow = unrounded.scrollable_overflow_rect;
    layout.scrollable_overflow_rect.left =
        round(cumulative_x + overflow.left) - round(cumulative_x);
    layout.scrollable_overflow_rect.right =
        round(cumulative_x + overflow.right) - round(cumulative_x);
    layout.scrollable_overflow_rect.top = round(cumulative_y + overflow.top) - round(cumulative_y);
    layout.scrollable_overflow_rect.bottom =
        round(cumulative_y + overflow.bottom) - round(cumulative_y);

    tree.set_final_layout(node_id, &layout);

    let child_count = tree.child_count(node_id);
    for index in 0..child_count {
        let child = tree.get_child_id(node_id, index);
        round_node(tree, child, cumulative_x, cumulative_y, scale);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_to_whole_css_px_at_scale_one() {
        for value in [0.0, 0.49, 0.5, 1.5, 2.4999, 7.5, -0.5, -1.5] {
            assert_eq!(round_to_device(value, 1.0), (value + 0.5_f32).floor());
        }
    }

    #[test]
    fn lands_on_whole_device_pixels_at_a_fractional_scale() {
        let scale = 2.0 / 3.0;
        for value in [0.0, 1.0, 13.3, 26.67, 40.0, 100.5] {
            let device = round_to_device(value, scale) * scale;
            assert!(
                (device - device.round()).abs() < 1e-3,
                "{value} -> {device}"
            );
        }
    }

    #[test]
    fn an_unusable_scale_rounds_to_css_px() {
        for scale in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            assert_eq!(round_to_device(1.6, scale), 2.0);
        }
    }
}
