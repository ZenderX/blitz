//! CSS scroll snap (css-scroll-snap-1): the offset a scroll container settles at after a
//! scroll, chosen from its `scroll-snap-type` and `scroll-padding` and from the
//! `scroll-snap-align`, `scroll-snap-stop` and `scroll-margin` of its snap areas.
//!
//! `block` is y and `inline` is x for a horizontal-tb container, the reverse for a
//! vertical one; `start` is the physical top or left either way.

use blitz_traits::node_id::NodeId;
use style::properties::ComputedValues;
use style::values::computed::{
    Length, NonNegativeLengthPercentageOrAuto, Overflow, ScrollSnapAxis, ScrollSnapStop,
    ScrollSnapStrictness,
};
use style::values::generics::length::GenericLengthPercentageOrAuto;
use style::values::specified::box_::ScrollSnapAlignKeyword;

use super::ScrollTarget;
use crate::util::Point;
use crate::{BaseDocument, Node};

/// Frames with no user scroll after which a gesture has ended: about 100 ms at 60 Hz.
/// Wheel input carries no end phase, so idle frames stand in for it.
const SCROLL_GESTURE_END_FRAMES: u32 = 6;

/// The four sides of a `scroll-margin` or `scroll-padding`, in CSS pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScrollSides {
    pub top: f64,
    pub right: f64,
    pub bottom: f64,
    pub left: f64,
}

/// How a scroll chooses among snap positions (css-scroll-snap-1 §6.2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum SnapStrategy {
    /// An absolute scroll (`scrollTo`, a `scrollTop` write): the snap position closest
    /// to the intended offset.
    Absolute,
    /// A relative scroll from `from` (`scrollBy`): the closest snap position past `from`
    /// in the direction of the scroll, never passing a `scroll-snap-stop: always` area.
    Directional { from: Point<f64> },
    /// The end of a user scroll gesture which began at `from`: the snap position closest
    /// to where it ended, never passing a `scroll-snap-stop: always` area.
    GestureEnd { from: Point<f64> },
}

/// The scrollers a user scroll gesture moved, each with its offset when the gesture
/// began, and the frames since the gesture last scrolled.
#[derive(Debug, Default)]
pub(crate) struct ScrollGesture {
    origins: Vec<(ScrollTarget, Point<f64>)>,
    idle_frames: u32,
}

/// A snap area: a box with a `scroll-snap-align`, outset by its `scroll-margin`, in its
/// snap container's scroll coordinates (the offset at which its start is at the start of
/// the scrollport).
struct SnapArea {
    start: Point<f64>,
    size: Point<f64>,
    align_x: ScrollSnapAlignKeyword,
    align_y: ScrollSnapAlignKeyword,
    is_stop_always: bool,
}

/// One axis of a snap container.
struct SnapAxis {
    /// The scrollport's size.
    port: f64,
    /// The `scroll-padding` at the start and end of the axis.
    padding_start: f64,
    padding_end: f64,
    /// The largest scroll offset.
    max: f64,
}

/// A snap position, and whether its area has `scroll-snap-stop: always`.
type Candidate = (f64, bool);

impl Node {
    /// This box's `scroll-margin`: how far its snap area extends past its border box.
    pub fn scroll_margin(&self) -> ScrollSides {
        let Some(style) = self.primary_styles() else {
            return ScrollSides::default();
        };
        let px = |length: Length| f64::from(length.px());
        ScrollSides {
            top: px(style.clone_scroll_margin_top()),
            right: px(style.clone_scroll_margin_right()),
            bottom: px(style.clone_scroll_margin_bottom()),
            left: px(style.clone_scroll_margin_left()),
        }
    }

    /// This scroll container's `scroll-padding`: the insets of its snapport within a
    /// scrollport of `port_width` by `port_height`. `auto` is 0, as in Chrome.
    pub fn scroll_padding(&self, port_width: f64, port_height: f64) -> ScrollSides {
        let Some(style) = self.primary_styles() else {
            return ScrollSides::default();
        };
        let resolve = |value: NonNegativeLengthPercentageOrAuto, basis: f64| match value {
            GenericLengthPercentageOrAuto::Auto => 0.0,
            GenericLengthPercentageOrAuto::LengthPercentage(lp) => {
                f64::from(lp.0.resolve(Length::new(basis as f32)).px())
            }
        };
        ScrollSides {
            top: resolve(style.clone_scroll_padding_top(), port_height),
            right: resolve(style.clone_scroll_padding_right(), port_width),
            bottom: resolve(style.clone_scroll_padding_bottom(), port_height),
            left: resolve(style.clone_scroll_padding_left(), port_width),
        }
    }
}

fn is_scroll_container(style: &ComputedValues) -> bool {
    let scrolls = |overflow| {
        matches!(
            overflow,
            Overflow::Hidden | Overflow::Scroll | Overflow::Auto
        )
    };
    scrolls(style.clone_overflow_x()) || scrolls(style.clone_overflow_y())
}

impl BaseDocument {
    /// Record a user scroll of `target` from `current`: it joins the gesture in progress.
    pub(crate) fn note_user_scroll(&mut self, target: ScrollTarget, current: Point<f64>) {
        let gesture = &mut self.scroll_gesture;
        gesture.idle_frames = 0;
        if !gesture.origins.iter().any(|(moved, _)| *moved == target) {
            gesture.origins.push((target, current));
        }
    }

    /// Count a frame in which no user scroll happened, returning whether the gesture in
    /// progress has now ended. The embedder then brings layout up to date and calls
    /// [`BaseDocument::end_scroll_gesture`].
    pub fn idle_scroll_gesture_frame(&mut self) -> bool {
        let gesture = &mut self.scroll_gesture;
        if gesture.origins.is_empty() {
            return false;
        }
        gesture.idle_frames += 1;
        gesture.idle_frames >= SCROLL_GESTURE_END_FRAMES
    }

    /// End the user scroll gesture in progress: every scroller it moved snaps.
    pub fn end_scroll_gesture(&mut self) {
        let gesture = std::mem::take(&mut self.scroll_gesture);
        for (target, from) in gesture.origins {
            let (current, max) = self.scroll_state(target, false);
            let end =
                self.snap_scroll_offset(target, current, max, SnapStrategy::GestureEnd { from });
            self.write_scroll_offset(target, end, &mut |_| {});
        }
    }

    /// The offset a scroll of `target` towards `intended` (already clamped to `0..=max`)
    /// settles at: `intended` itself unless `target` is a snap container.
    pub(crate) fn snap_scroll_offset(
        &self,
        target: ScrollTarget,
        intended: Point<f64>,
        max: Point<f64>,
        strategy: SnapStrategy,
    ) -> Point<f64> {
        // The viewport takes its snap type and padding from the root element.
        let (style_node, port, origin) = match target {
            ScrollTarget::Viewport => {
                let Some(root) = self.try_root_element() else {
                    return intended;
                };
                let scale = self.viewport.scale() as f64;
                let port = Point {
                    x: self.viewport.window_size.0 as f64 / scale,
                    y: self.viewport.window_size.1 as f64 / scale,
                };
                let location = root.final_layout().location;
                let origin = Point {
                    x: location.x as f64,
                    y: location.y as f64,
                };
                (root, port, origin)
            }
            ScrollTarget::Node(node_id) => {
                let Some(node) = self.nodes.get(node_id) else {
                    return intended;
                };
                let border = node.final_layout().border;
                let port = Point {
                    x: node.client_width() as f64,
                    y: node.client_height() as f64,
                };
                // Children are placed from the border box; scroll offsets from the padding box.
                let origin = Point {
                    x: -border.left as f64,
                    y: -border.top as f64,
                };
                (node, port, origin)
            }
        };
        let Some(snap_type) = style_node
            .primary_styles()
            .map(|style| style.clone_scroll_snap_type())
        else {
            return intended;
        };
        let is_vertical = style_node
            .element_data()
            .is_some_and(|element| element.layout_flow.is_vertical());
        let (snaps_x, snaps_y) = match (snap_type.axis, is_vertical) {
            (ScrollSnapAxis::X, _)
            | (ScrollSnapAxis::Inline, false)
            | (ScrollSnapAxis::Block, true) => (true, false),
            (ScrollSnapAxis::Y, _)
            | (ScrollSnapAxis::Block, false)
            | (ScrollSnapAxis::Inline, true) => (false, true),
            (ScrollSnapAxis::Both, _) => (true, true),
        };
        let is_proximity = match snap_type.strictness {
            ScrollSnapStrictness::None => return intended,
            ScrollSnapStrictness::Mandatory => false,
            ScrollSnapStrictness::Proximity => true,
        };

        let padding = style_node.scroll_padding(port.x, port.y);
        let mut areas = Vec::new();
        self.collect_snap_areas(style_node.id, origin, is_vertical, &mut areas);

        let (from, is_directional) = match strategy {
            SnapStrategy::Absolute => (None, false),
            SnapStrategy::Directional { from } => (Some(from), true),
            SnapStrategy::GestureEnd { from } => (Some(from), false),
        };
        let x_axis = SnapAxis {
            port: port.x,
            padding_start: padding.left,
            padding_end: padding.right,
            max: max.x,
        };
        let y_axis = SnapAxis {
            port: port.y,
            padding_start: padding.top,
            padding_end: padding.bottom,
            max: max.y,
        };
        let x = match snaps_x {
            true => {
                let candidates = x_axis.candidates(
                    areas
                        .iter()
                        .map(|a| (a.start.x, a.size.x, a.align_x, a.is_stop_always)),
                    intended.x,
                );
                x_axis.choose(
                    &candidates,
                    intended.x,
                    from.map(|f| f.x),
                    is_directional,
                    is_proximity,
                )
            }
            false => intended.x,
        };
        let y = match snaps_y {
            true => {
                let candidates = y_axis.candidates(
                    areas
                        .iter()
                        .map(|a| (a.start.y, a.size.y, a.align_y, a.is_stop_always)),
                    intended.y,
                );
                y_axis.choose(
                    &candidates,
                    intended.y,
                    from.map(|f| f.y),
                    is_directional,
                    is_proximity,
                )
            }
            false => intended.y,
        };
        Point { x, y }
    }

    /// Collect the snap areas below `node_id` whose nearest scroll container is the snap
    /// container, `origin` being `node_id`'s border-box position in its scroll coordinates.
    /// In a vertical snap container the block axis is x and the inline axis y.
    fn collect_snap_areas(
        &self,
        node_id: NodeId,
        origin: Point<f64>,
        is_vertical: bool,
        areas: &mut Vec<SnapArea>,
    ) {
        let Some(node) = self.nodes.get(node_id) else {
            return;
        };
        let children = node.layout_children.borrow();
        for &child_id in children.iter().flatten() {
            let Some(child) = self.nodes.get(child_id) else {
                continue;
            };
            let layout = child.final_layout();
            let position = Point {
                x: origin.x + layout.location.x as f64,
                y: origin.y + layout.location.y as f64,
            };
            if let Some(style) = child.primary_styles() {
                let align = style.clone_scroll_snap_align();
                if align.block != ScrollSnapAlignKeyword::None
                    || align.inline != ScrollSnapAlignKeyword::None
                {
                    let margin = child.scroll_margin();
                    areas.push(SnapArea {
                        start: Point {
                            x: position.x - margin.left,
                            y: position.y - margin.top,
                        },
                        size: Point {
                            x: layout.size.width as f64 + margin.left + margin.right,
                            y: layout.size.height as f64 + margin.top + margin.bottom,
                        },
                        align_x: if is_vertical {
                            align.block
                        } else {
                            align.inline
                        },
                        align_y: if is_vertical {
                            align.inline
                        } else {
                            align.block
                        },
                        is_stop_always: style.clone_scroll_snap_stop() == ScrollSnapStop::Always,
                    });
                }
                // A nested scroll container snaps its own descendants.
                if is_scroll_container(&style) {
                    continue;
                }
            }
            self.collect_snap_areas(child_id, position, is_vertical, areas);
        }
    }
}

impl SnapAxis {
    /// The size of the snapport: the scrollport less the `scroll-padding`.
    fn snapport(&self) -> f64 {
        (self.port - self.padding_start - self.padding_end).max(0.0)
    }

    /// The snap position of each area aligned on this axis, clamped to `0..=max`.
    fn candidates(
        &self,
        areas: impl Iterator<Item = (f64, f64, ScrollSnapAlignKeyword, bool)>,
        intended: f64,
    ) -> Vec<Candidate> {
        let snapport = self.snapport();
        areas
            .filter_map(|(start, size, align, is_stop_always)| {
                let aligned = match align {
                    ScrollSnapAlignKeyword::None => return None,
                    ScrollSnapAlignKeyword::Start => start - self.padding_start,
                    ScrollSnapAlignKeyword::End => start + size - self.port + self.padding_end,
                    ScrollSnapAlignKeyword::Center => {
                        start + size / 2.0 - self.padding_start - snapport / 2.0
                    }
                };
                // §6.3: an area larger than the snapport is valid wherever it covers it.
                let position = if size > snapport {
                    intended.clamp(
                        start - self.padding_start,
                        start + size - self.port + self.padding_end,
                    )
                } else {
                    aligned
                };
                Some((position.clamp(0.0, self.max), is_stop_always))
            })
            .collect()
    }

    /// Choose among `candidates` for a scroll towards `intended` which began at `from`.
    /// Ties go to the area first in tree order.
    fn choose(
        &self,
        candidates: &[Candidate],
        intended: f64,
        from: Option<f64>,
        is_directional: bool,
        is_proximity: bool,
    ) -> f64 {
        let distance = |position: f64| (position - intended).abs();
        let ahead: Vec<Candidate> = match (from, is_directional) {
            (Some(from), true) => candidates
                .iter()
                .copied()
                .filter(|(position, _)| (position - from) * (intended - from) > 0.0)
                .collect(),
            _ => Vec::new(),
        };
        let pool = if ahead.is_empty() { candidates } else { &ahead };
        let closest = pool
            .iter()
            .map(|&(position, _)| position)
            .min_by(|a, b| distance(*a).total_cmp(&distance(*b)));
        // Chrome's proximity range is a third of the snapport (Edge 156: 100px of 300px).
        let chosen = closest
            .filter(|&position| !is_proximity || distance(position) <= self.snapport() / 3.0)
            .unwrap_or(intended);

        let Some(from) = from else {
            return chosen;
        };
        // §6.2 `scroll-snap-stop: always`: never pass over such an area's snap position.
        candidates
            .iter()
            .filter(|&&(position, is_stop_always)| {
                is_stop_always
                    && (position - from) * (chosen - from) > 0.0
                    && (position - from).abs() < (chosen - from).abs()
            })
            .map(|&(position, _)| position)
            .min_by(|a, b| (a - from).abs().total_cmp(&(b - from).abs()))
            .unwrap_or(chosen)
    }
}
