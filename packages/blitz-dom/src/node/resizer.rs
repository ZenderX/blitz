//! The `resize` grip (css-ui-4 §5.1): its geometry, shared by blitz-paint's drawing and
//! the hit test so the two cannot drift, and the drag that resizes the box.

use blitz_traits::node_id::NodeId;
use kurbo::Rect as KurboRect;
use style::computed_values::box_sizing::T as BoxSizing;
use style::values::computed::Resize;

use super::Node;
use crate::BaseDocument;

/// The side of the grip's square, in CSS px.
const GRIP_SIZE: f64 = 15.0;

/// The axes `resize` lets the user drag a box in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResizeAxes {
    pub horizontal: bool,
    pub vertical: bool,
}

/// A drag of a `resize` grip in progress.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ResizeDrag {
    node_id: NodeId,
    axes: ResizeAxes,
    /// The press, in the coordinates [`BaseDocument::hit`] takes.
    start: (f32, f32),
    /// The border box's size at the press.
    start_size: (f32, f32),
}

/// The `width` and `height` a grip drag gives its box, in CSS px and for the box's own
/// `box-sizing`; `None` in an axis the drag does not resize.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResizedSize {
    pub node_id: NodeId,
    pub width: Option<f32>,
    pub height: Option<f32>,
}

impl Node {
    /// The axes the computed `resize` lets the user drag. It applies to scroll containers
    /// only; computed `resize` already maps `block`/`inline` to physical axes.
    pub fn resize_axes(&self) -> ResizeAxes {
        let Some(style) = self.primary_styles() else {
            return ResizeAxes::default();
        };
        if !style.clone_overflow_x().is_scrollable() && !style.clone_overflow_y().is_scrollable() {
            return ResizeAxes::default();
        }
        let (horizontal, vertical) = match style.clone_resize() {
            Resize::None => (false, false),
            Resize::Both => (true, true),
            Resize::Horizontal => (true, false),
            Resize::Vertical => (false, true),
        };
        ResizeAxes {
            horizontal,
            vertical,
        }
    }

    /// The grip, a square in the bottom-right corner of the padding box, in (unscaled) CSS
    /// px relative to the node's border-box origin; `None` when `resize` allows no axis.
    pub fn resizer_rect(&self) -> Option<KurboRect> {
        if self.resize_axes() == ResizeAxes::default() {
            return None;
        }
        let port = self.scrollport();
        let side = GRIP_SIZE.min(port.width()).min(port.height()).max(0.0);
        Some(KurboRect::new(
            port.x1 - side,
            port.y1 - side,
            port.x1,
            port.y1,
        ))
    }
}

impl BaseDocument {
    /// Start dragging the innermost grip under the point (x, y), in the coordinates
    /// [`hit`](Self::hit) takes. Returns the grip's node, or `None` when no grip is there.
    pub fn start_resize_drag(&mut self, x: f32, y: f32) -> Option<NodeId> {
        let hit = self.hit(x, y)?;
        let mut current = Some(hit.node_id);
        while let Some(node) = current.map(|id| &self.nodes[id]) {
            current = node.layout_parent.get();
            let Some(grip) = node.resizer_rect() else {
                continue;
            };
            let scroll = node.scroll_offset();
            let origin = node.absolute_position(scroll.x as f32, scroll.y as f32);
            let local = kurbo::Point::new((x - origin.x) as f64, (y - origin.y) as f64);
            if !grip.contains(local) {
                continue;
            }
            let size = node.final_layout().size;
            self.resize_drag = Some(ResizeDrag {
                node_id: node.id,
                axes: node.resize_axes(),
                start: (x, y),
                start_size: (size.width, size.height),
            });
            return Some(node.id);
        }
        None
    }

    /// The size the grip drag in progress gives its box with the pointer at (x, y): the
    /// border box grows by the pointer's travel since the press, and never below zero
    /// content. `None` when no drag is in progress or its node has gone.
    pub fn resize_drag_to(&self, x: f32, y: f32) -> Option<ResizedSize> {
        let drag = self.resize_drag?;
        let node = self.get_node(drag.node_id)?;
        let style = node.primary_styles()?;
        let layout = node.final_layout();
        let is_border_box = style.clone_box_sizing() == BoxSizing::BorderBox;
        let axis = |enabled: bool, start: f32, travel: f32, padding_border: f32| {
            enabled.then(|| {
                let content = (start + travel - padding_border).max(0.0);
                if is_border_box {
                    content + padding_border
                } else {
                    content
                }
            })
        };
        Some(ResizedSize {
            node_id: drag.node_id,
            width: axis(
                drag.axes.horizontal,
                drag.start_size.0,
                x - drag.start.0,
                layout.padding.left
                    + layout.padding.right
                    + layout.border.left
                    + layout.border.right,
            ),
            height: axis(
                drag.axes.vertical,
                drag.start_size.1,
                y - drag.start.1,
                layout.padding.top
                    + layout.padding.bottom
                    + layout.border.top
                    + layout.border.bottom,
            ),
        })
    }

    /// End the grip drag in progress, if any.
    pub fn end_resize_drag(&mut self) {
        self.resize_drag = None;
    }
}
