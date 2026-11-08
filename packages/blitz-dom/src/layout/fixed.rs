//! Containing blocks for `position: fixed` boxes.
//!
//! `stylo_taffy` maps `position: fixed` to `taffy::Position::Absolute`, which taffy
//! resolves against the box's layout parent. Per CSS Position 3 the containing block of a
//! fixed box is the viewport, unless an ancestor establishes one for fixed descendants
//! (see [`establishes_fixed_containing_block`]). After the main layout, this pass re-lays
//! out every fixed box against its real containing block. The box stays in its layout
//! parent's frame, so painting and hit-testing read the corrected rects unchanged.
//!
//! A fixed box does not move with the scroll of an ancestor strictly between it and its
//! containing block, nor, when that is the viewport, with viewport scroll. As for sticky
//! boxes (see `sticky.rs`), a pass after rounding adds those scroll offsets to its
//! `final_layout` location, so painting and hit-testing, which subtract them, leave it in
//! place; a scroll, which sets no damage, is picked up by the next `resolve`. The box then
//! lies outside what its ancestors stored for hit-testing to prune on, so the pass widens
//! that too.
//!
//! Nor does it move with an ancestor's sticky shift: the sticky pass takes that back from
//! it. Overflow clips it only from its containing block upward (CSS 2.1 §11.1.1).

use crate::layout::inline::layout_abspos_child;
use crate::node::Node;
use crate::{BaseDocument, NodeId};
use style::properties::ComputedValues;
use style::properties::generated::longhands::position::computed_value::T as Position;
use style::values::computed::{Perspective, Rotate};
use style::values::generics::transform::{Scale, Translate};
use style::values::specified::box_::{Contain, DisplayOutside, WillChangeBits};

/// Whether a box of this style is the containing block for its `position: fixed`
/// descendants: `transform` (or `translate`/`rotate`/`scale`), `perspective`, `filter` or
/// `backdrop-filter` other than none, `contain: paint | layout | strict | content`, or a
/// `will-change` naming one.
pub(super) fn establishes_fixed_containing_block(style: &ComputedValues) -> bool {
    let box_style = style.get_box();
    let effects = style.get_effects();
    !box_style.transform.0.is_empty()
        || !matches!(box_style.translate, Translate::None)
        || !matches!(box_style.rotate, Rotate::None)
        || !matches!(box_style.scale, Scale::None)
        || !matches!(box_style.perspective, Perspective::None)
        || !effects.filter.0.is_empty()
        || !effects.backdrop_filter.0.is_empty()
        || box_style
            .contain
            .intersects(Contain::LAYOUT | Contain::PAINT)
        || box_style.will_change.bits.intersects(
            WillChangeBits::TRANSFORM
                | WillChangeBits::PERSPECTIVE
                | WillChangeBits::CONTAIN
                | WillChangeBits::FIXPOS_CB_NON_SVG,
        )
}

pub(super) fn is_fixed_positioned(node: &Node) -> bool {
    node.primary_styles()
        .is_some_and(|style| matches!(style.clone_position(), Position::Fixed))
}

impl BaseDocument {
    /// Document-space origin of a node's border box, from the unrounded layout.
    fn unrounded_origin(&self, id: NodeId) -> taffy::Point<f32> {
        let mut origin = taffy::Point::ZERO;
        let mut current = Some(id);
        while let Some(id) = current {
            let node = &self.nodes[id];
            origin.x += node.unrounded_layout().location.x;
            origin.y += node.unrounded_layout().location.y;
            current = node.layout_parent.get();
        }
        origin
    }

    /// Re-lay out every `position: fixed` box against its containing block. Runs between
    /// the taffy layout and rounding. Idempotent: the static position of an axis without
    /// insets is recovered from the box's current location, which is also the output.
    /// Returns the boxes, for [`Self::scroll_fixed_boxes`] to shift.
    pub(crate) fn layout_fixed_boxes(&mut self) -> Vec<NodeId> {
        let viewport = self.stylist.device().au_viewport_size();
        let viewport = taffy::Size {
            width: viewport.width.to_f32_px(),
            height: viewport.height.to_f32_px(),
        };

        // Pre-order, so a fixed box nested in another is placed after its ancestor.
        let mut fixed_boxes = Vec::new();
        let mut stack = vec![self.root_element().id];
        while let Some(id) = stack.pop() {
            let Some(node) = self.nodes.get(id) else {
                continue;
            };
            if let Some(children) = node.layout_children.borrow().as_ref() {
                stack.extend(children.iter().rev());
            }
            if is_fixed_positioned(node) {
                fixed_boxes.push(id);
            }
        }

        for &id in &fixed_boxes {
            self.layout_fixed_box(id, viewport);
        }
        fixed_boxes
    }

    /// Cancel, for each of `boxes`, the scroll offsets it would otherwise move with: those
    /// of its ancestors below its containing block, which carries it, up to and including
    /// the nearest fixed one, which any box inside moves with, or else up to the root and
    /// the viewport's. Runs after rounding, so each box starts from its unscrolled position,
    /// and before the sticky pass, so a sticky box inside one measures from where it is.
    pub(crate) fn scroll_fixed_boxes(&mut self, boxes: &[NodeId]) {
        for &id in boxes {
            let scroll = self.carried_scroll(id);
            let location = &mut self.nodes[id].final_layout_mut().location;
            location.x += scroll.x as f32;
            location.y += scroll.y as f32;
            self.widen_ancestors_to(id);
        }
    }

    /// The scroll offsets painting and hit-testing apply to a box from its ancestors, up to
    /// and excluding its containing block, up to the nearest fixed ancestor, or else up to
    /// and including the viewport.
    fn carried_scroll(&self, id: NodeId) -> crate::Point<f64> {
        let mut scroll = crate::Point { x: 0.0, y: 0.0 };
        let mut current = id;
        while let Some(parent_id) = self.nodes[current].layout_parent.get() {
            let parent = &self.nodes[parent_id];
            if parent
                .primary_styles()
                .is_some_and(|style| establishes_fixed_containing_block(&style))
            {
                return scroll;
            }
            scroll.x += parent.scroll_offset().x;
            scroll.y += parent.scroll_offset().y;
            if is_fixed_positioned(parent) {
                return scroll;
            }
            current = parent_id;
        }
        let viewport = self.viewport_scroll();
        crate::Point {
            x: scroll.x + viewport.x,
            y: scroll.y + viewport.y,
        }
    }

    /// Take back from a fixed box the sticky shift of its ancestors up to the nearest fixed
    /// one, when its containing block is the viewport; otherwise it moves with them.
    pub(super) fn cancel_sticky_shift(&mut self, id: NodeId, shift: taffy::Point<f32>) {
        let mut ancestor = self.nodes[id].layout_parent.get();
        while let Some(ancestor_id) = ancestor {
            let node = &self.nodes[ancestor_id];
            if node
                .primary_styles()
                .is_some_and(|style| establishes_fixed_containing_block(&style))
            {
                return;
            }
            ancestor = node.layout_parent.get();
        }

        let location = &mut self.nodes[id].final_layout_mut().location;
        location.x -= shift.x;
        location.y -= shift.y;
        self.widen_ancestors_to(id);
    }

    /// Hit-testing prunes a subtree on what its root stored, and no damage recomputes that
    /// for a scroll. So each ancestor's stored overflow grows to cover the box wherever a
    /// scroll puts it, and never shrinks for one: a wider rect only prunes less. Each step
    /// goes through that ancestor's transform, so a transformed containing block is covered
    /// too; a transform not resolved yet leaves its box damaged, which recomputes the rect.
    fn widen_ancestors_to(&mut self, id: NodeId) {
        let mut current = id;
        while let Some(parent_id) = self.nodes[current].layout_parent.get() {
            let overflow = self.overflow_in_parent_frame(current);
            let stored = self.nodes[parent_id].scrollable_overflow_mut();
            *stored = stored.union(overflow);
            current = parent_id;
        }
    }

    fn layout_fixed_box(&mut self, id: NodeId, viewport: taffy::Size<f32>) {
        let Some(parent_id) = self.nodes[id].layout_parent.get() else {
            return;
        };

        // The containing block's padding box, in document space.
        let mut containing_block = Some(parent_id);
        let (area_origin, area_size) = loop {
            let Some(candidate_id) = containing_block else {
                break (taffy::Point::ZERO, viewport);
            };
            let candidate = &self.nodes[candidate_id];
            if candidate
                .primary_styles()
                .is_some_and(|style| establishes_fixed_containing_block(&style))
            {
                let layout = candidate.unrounded_layout();
                let origin = self.unrounded_origin(candidate_id);
                break (
                    taffy::Point {
                        x: origin.x + layout.border.left,
                        y: origin.y + layout.border.top,
                    },
                    taffy::Size {
                        width: layout.size.width
                            - layout.border.left
                            - layout.border.right
                            - layout.scrollbar_size.width,
                        height: layout.size.height
                            - layout.border.top
                            - layout.border.bottom
                            - layout.scrollbar_size.height,
                    },
                );
            }
            containing_block = candidate.layout_parent.get();
        };

        let parent_origin = self.unrounded_origin(parent_id);
        let node = &self.nodes[id];
        let current = node.unrounded_layout();
        let static_position = taffy::Point {
            x: current.location.x - current.margin.left,
            y: current.location.y - current.margin.top,
        };
        let direction = node.style().direction;
        let is_inline_level = node
            .primary_styles()
            .map(|style| style.get_box().original_display.outside() == DisplayOutside::Inline)
            .unwrap_or(true);

        layout_abspos_child(
            self,
            id.as_u64(),
            static_position,
            is_inline_level,
            area_size,
            taffy::Point {
                x: area_origin.x - parent_origin.x,
                y: area_origin.y - parent_origin.y,
            },
            direction,
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::{Attribute, BaseDocument, DocumentConfig, NodeId, Point, qual_name};
    use blitz_traits::shell::{ColorScheme, Viewport};

    /// Lay out `<body style="margin:0"><div style="{wrapper}"><div style="{fixed}"></div></div>`
    /// in a 400x300 viewport; returns the document, the wrapper and the fixed box.
    fn layout(wrapper: &str, fixed: &str) -> (BaseDocument, NodeId, NodeId) {
        let mut doc = BaseDocument::new(DocumentConfig {
            viewport: Some(Viewport::new(400, 300, 1.0, ColorScheme::Light)),
            ..Default::default()
        });
        let root_id = doc.root_node().id;
        let style = |value: &str| {
            vec![Attribute {
                name: qual_name!("style"),
                value: value.to_string(),
            }]
        };

        let mut mutator = doc.mutate();
        // No UA stylesheet is loaded, so every box needs an explicit `display:block`.
        let html = mutator.create_element(qual_name!("html"), style("display:block"));
        let body = mutator.create_element(qual_name!("body"), style("display:block;margin:0"));
        let wrapper_id = mutator.create_element(
            qual_name!("div"),
            style(&format!("display:block;{wrapper}")),
        );
        let fixed_id = mutator.create_element(qual_name!("div"), style(fixed));
        mutator.append_children(wrapper_id, &[fixed_id]);
        mutator.append_children(body, &[wrapper_id]);
        mutator.append_children(html, &[body]);
        mutator.append_children(root_id, &[html]);
        drop(mutator);

        doc.resolve(0.0);
        (doc, wrapper_id, fixed_id)
    }

    fn position(doc: &BaseDocument, id: NodeId) -> (f32, f32) {
        let point = doc.nodes[id].absolute_position(0.0, 0.0);
        (point.x, point.y)
    }

    const FIXED: &str = "position:fixed;left:30px;top:20px;width:40px;height:30px";

    #[test]
    fn fixed_box_ignores_plain_ancestor_margin() {
        let (doc, _, fixed) = layout("margin:50px;width:100px;height:100px", FIXED);
        assert_eq!(position(&doc, fixed), (30.0, 20.0));
    }

    #[test]
    fn fixed_box_is_placed_against_a_transformed_ancestor() {
        let (doc, _, fixed) = layout(
            "margin:50px;width:100px;height:100px;transform:translate(0)",
            FIXED,
        );
        assert_eq!(position(&doc, fixed), (80.0, 70.0));
    }

    /// Filter Effects 2: `backdrop-filter` is a containing block for fixed descendants, while
    /// `clip-path` and `mask-image` are not.
    #[test]
    fn fixed_box_is_placed_against_a_backdrop_filter_ancestor() {
        let placed_at = |effect: &str| {
            let wrapper = format!("margin:50px;width:100px;height:100px;{effect}");
            let (doc, _, fixed) = layout(&wrapper, FIXED);
            position(&doc, fixed)
        };
        assert_eq!(placed_at("backdrop-filter:blur(1px)"), (80.0, 70.0));
        assert_eq!(placed_at("clip-path:inset(0)"), (30.0, 20.0));
        assert_eq!(
            placed_at("mask-image:linear-gradient(black,black)"),
            (30.0, 20.0)
        );
    }

    #[test]
    fn fixed_box_insets_resolve_against_the_viewport() {
        let (doc, _, fixed) = layout(
            "margin:50px;width:100px;height:100px",
            "position:fixed;right:10px;bottom:10px;width:40px;height:30px",
        );
        assert_eq!(position(&doc, fixed), (350.0, 260.0));
        assert_eq!(doc.nodes[fixed].final_layout().size.width, 40.0);
    }

    #[test]
    fn fixed_box_without_insets_keeps_its_static_position() {
        let (doc, _, fixed) = layout(
            "margin:50px;width:100px;height:100px",
            "position:fixed;width:40px;height:30px",
        );
        assert_eq!(position(&doc, fixed), (50.0, 50.0));
    }

    #[test]
    fn hit_test_finds_fixed_box_at_its_viewport_position() {
        let (doc, wrapper, fixed) = layout("margin:50px;width:100px;height:100px", FIXED);
        assert_eq!(doc.hit(35.0, 25.0).map(|hit| hit.node_id), Some(fixed));
        // Where the box used to be (relative to the wrapper), only the wrapper is hit.
        assert_eq!(doc.hit(85.0, 75.0).map(|hit| hit.node_id), Some(wrapper));
    }

    fn scroll_to(doc: &mut BaseDocument, y: f64) {
        doc.set_viewport_scroll(Point { x: 0.0, y });
        doc.resolve(0.0);
    }

    #[test]
    fn fixed_box_stays_on_screen_under_viewport_scroll() {
        let (mut doc, _, fixed) = layout("margin:50px;width:100px;height:100px", FIXED);
        scroll_to(&mut doc, 250.0);
        assert_eq!(position(&doc, fixed), (30.0, 270.0), "scroll + inset");

        scroll_to(&mut doc, 250.0);
        assert_eq!(position(&doc, fixed), (30.0, 270.0), "repeat resolves hold");

        scroll_to(&mut doc, 0.0);
        assert_eq!(position(&doc, fixed), (30.0, 20.0), "scrolled back");
    }

    /// The transformed ancestor is the containing block, so the box scrolls off with it.
    #[test]
    fn scroll_shift_stops_at_a_transformed_ancestor() {
        let (mut doc, _, fixed) = layout(
            "margin:50px;width:100px;height:100px;transform:translate(0)",
            FIXED,
        );
        scroll_to(&mut doc, 250.0);
        assert_eq!(position(&doc, fixed), (80.0, 70.0));
        assert_eq!(doc.hit(85.0, 75.0).map(|hit| hit.node_id), Some(fixed));
    }

    #[test]
    fn fixed_box_inside_a_fixed_box_takes_the_shift_once() {
        let (mut doc, _, inner) = layout(
            "position:fixed;left:10px;top:10px;width:100px;height:100px",
            FIXED,
        );
        scroll_to(&mut doc, 250.0);
        assert_eq!(position(&doc, inner), (30.0, 270.0));
    }

    /// Measured before the shift, the box would count the scroll as distance to stick over
    /// and take it twice.
    #[test]
    fn sticky_box_inside_a_fixed_box_stays_put_under_viewport_scroll() {
        let (mut doc, _, sticky) = layout(
            "position:fixed;left:0;top:0;width:100px;height:100px",
            "display:block;position:sticky;top:0;width:40px;height:30px",
        );
        scroll_to(&mut doc, 250.0);
        assert_eq!(position(&doc, sticky), (0.0, 250.0));
    }

    /// No damage recomputes the stored overflow for a scroll, so without the widening the
    /// short wrapper and its ancestors would prune the box from the hit test.
    #[test]
    fn hit_test_reaches_a_fixed_box_scrolled_past_its_ancestors_overflow() {
        let (mut doc, wrapper, fixed) = layout("width:100px;height:100px", FIXED);
        scroll_to(&mut doc, 250.0);
        assert_eq!(doc.hit(35.0, 275.0).map(|hit| hit.node_id), Some(fixed));
        assert_eq!(
            doc.hit(35.0, 25.0).map(|hit| hit.node_id),
            Some(wrapper),
            "its unscrolled position no longer hits it"
        );
    }

    /// A non-zero `z-index` is reached through the stacking context's content area, which
    /// `place_hoisted_children` computes after the shift.
    #[test]
    fn hit_test_reaches_a_hoisted_fixed_box_on_the_first_resolve_after_a_scroll() {
        let (mut doc, _, fixed) = layout("width:100px;height:100px", &format!("{FIXED};z-index:1"));
        scroll_to(&mut doc, 250.0);
        assert_eq!(doc.hit(35.0, 275.0).map(|hit| hit.node_id), Some(fixed));
    }

    fn scroll_container_to(doc: &mut BaseDocument, container: NodeId, y: f64) {
        *doc.nodes[container].scroll_offset_mut() = Point { x: 0.0, y };
        doc.resolve(0.0);
    }

    const SCROLLER: &str = "margin:50px;width:100px;height:100px;overflow:auto";
    const FIXED_IN_SCROLLER: &str = "position:fixed;left:60px;top:70px;width:40px;height:30px";

    #[test]
    fn fixed_box_stays_put_when_its_scroll_container_scrolls() {
        let (mut doc, scroller, fixed) = layout(SCROLLER, FIXED_IN_SCROLLER);
        scroll_container_to(&mut doc, scroller, 40.0);
        assert_eq!(position(&doc, fixed), (60.0, 70.0), "scroll cancelled");

        scroll_container_to(&mut doc, scroller, 40.0);
        assert_eq!(position(&doc, fixed), (60.0, 70.0), "repeat resolves hold");

        scroll_to(&mut doc, 250.0);
        assert_eq!(position(&doc, fixed), (60.0, 320.0), "both cancelled");
    }

    #[test]
    fn hit_test_finds_a_fixed_box_after_its_scroll_container_scrolls() {
        let (mut doc, scroller, fixed) = layout(SCROLLER, FIXED_IN_SCROLLER);
        scroll_container_to(&mut doc, scroller, 40.0);
        assert_eq!(doc.hit(65.0, 75.0).map(|hit| hit.node_id), Some(fixed));
        assert_eq!(
            doc.hit(65.0, 55.0).map(|hit| hit.node_id),
            Some(scroller),
            "where the container scroll would have carried it, only the container is hit"
        );
    }

    /// The inner box moves with the outer one, which takes the viewport scroll, but not
    /// with the outer box's own scroll.
    #[test]
    fn fixed_box_inside_a_scrolling_fixed_box_stays_put() {
        let (mut doc, outer, inner) = layout(
            "position:fixed;left:10px;top:10px;width:100px;height:100px;overflow:auto",
            FIXED,
        );
        scroll_container_to(&mut doc, outer, 40.0);
        scroll_to(&mut doc, 250.0);
        assert_eq!(position(&doc, inner), (30.0, 270.0));
    }

    /// Overflow recomputed while scrolled holds only the scrolled position, yet scrolling
    /// back sets no damage to recompute it again. The wrapper sits below both positions,
    /// or its own box would span them.
    #[test]
    fn hit_test_reaches_a_fixed_box_scrolled_back_after_overflow_recomputed() {
        let (mut doc, wrapper, fixed) = layout("margin-top:500px;width:100px;height:10px", FIXED);
        scroll_to(&mut doc, 250.0);
        doc.mutate().set_attribute(
            wrapper,
            qual_name!("style"),
            "display:block;margin-top:500px;width:100px;height:15px",
        );
        doc.resolve(0.0);

        scroll_to(&mut doc, 0.0);
        assert_eq!(doc.hit(35.0, 25.0).map(|hit| hit.node_id), Some(fixed));
    }

    /// As [`layout`], with a box of style `{middle}` between the wrapper and the fixed box;
    /// returns the document, the wrapper, the middle box and the fixed box.
    fn layout_with_middle(
        wrapper: &str,
        middle: &str,
        fixed: &str,
    ) -> (BaseDocument, NodeId, NodeId, NodeId) {
        let (mut doc, wrapper_id, middle_id) = layout(wrapper, &format!("display:block;{middle}"));
        let mut mutator = doc.mutate();
        let fixed_id = mutator.create_element(
            qual_name!("div"),
            vec![Attribute {
                name: qual_name!("style"),
                value: fixed.to_string(),
            }],
        );
        mutator.append_children(middle_id, &[fixed_id]);
        drop(mutator);
        doc.resolve(0.0);
        (doc, wrapper_id, middle_id, fixed_id)
    }

    const FIXED_IN_TRANSFORMED: &str = "position:fixed;left:10px;top:60px;width:40px;height:30px";

    /// Chrome and Firefox keep a fixed box's scroll state at its containing block, so a
    /// scroller inside that block does not reach it.
    #[test]
    fn fixed_box_under_a_transform_stays_put_when_a_scroller_inside_it_scrolls() {
        let (mut doc, _, scroller, fixed) = layout_with_middle(
            "margin:50px;width:150px;height:150px;transform:translate(1px,0)",
            "width:100px;height:100px;overflow:auto",
            FIXED_IN_TRANSFORMED,
        );
        assert_eq!(doc.hit(65.0, 115.0).map(|hit| hit.node_id), Some(fixed));

        scroll_container_to(&mut doc, scroller, 40.0);
        assert_eq!(position(&doc, fixed), (60.0, 110.0), "scroll cancelled");
        assert_eq!(doc.hit(65.0, 115.0).map(|hit| hit.node_id), Some(fixed));
        assert_eq!(
            doc.hit(65.0, 75.0).map(|hit| hit.node_id),
            Some(scroller),
            "where the scroll would have carried it, only the scroller is hit"
        );

        scroll_container_to(&mut doc, scroller, 40.0);
        assert_eq!(position(&doc, fixed), (60.0, 110.0), "repeat resolves hold");
    }

    /// WPT `transform-containing-block-and-scrolling-area-for-fixed`: the containing block
    /// scrolls its fixed descendants like any other content.
    #[test]
    fn fixed_box_moves_with_the_scroll_of_its_transformed_containing_block() {
        let (mut doc, containing_block, _, fixed) = layout_with_middle(
            "margin:50px;width:150px;height:150px;overflow:auto;transform:translate(1px,0)",
            "width:100px;height:100px",
            FIXED_IN_TRANSFORMED,
        );
        scroll_container_to(&mut doc, containing_block, 40.0);
        assert_eq!(position(&doc, fixed), (60.0, 70.0), "nothing cancelled");
        assert_eq!(doc.hit(65.0, 75.0).map(|hit| hit.node_id), Some(fixed));
    }

    fn resize_window(doc: &mut BaseDocument, width: u32, height: u32) {
        doc.set_viewport(Viewport::new(width, height, 1.0, ColorScheme::Light));
        doc.resolve(0.0);
    }

    /// A window resize moves a box with a right or bottom inset, without resizing it or its
    /// parent, so no overflow damage reaches the parent to recompute what it stored.
    #[test]
    fn stored_overflow_follows_a_viewport_fixed_box_moved_by_a_window_resize() {
        let (mut doc, wrapper, fixed) = layout(
            "margin:50px;width:100px;height:100px",
            "position:fixed;right:10px;bottom:10px;width:40px;height:30px",
        );
        resize_window(&mut doc, 800, 600);
        assert_eq!(position(&doc, fixed), (750.0, 560.0));

        let stored = *doc.nodes[wrapper].scrollable_overflow();
        assert_eq!(
            (stored.x1, stored.y1),
            (740.0, 540.0),
            "in the wrapper's frame"
        );
        assert_eq!(doc.hit(755.0, 565.0).map(|hit| hit.node_id), Some(fixed));
    }

    /// The containing block resizes and is damaged, but a box between it and the fixed box
    /// is not, so it needs the widening too.
    #[test]
    fn stored_overflow_follows_a_fixed_box_moved_by_its_containing_block_resizing() {
        let (mut doc, _, middle, fixed) = layout_with_middle(
            "margin:50px;width:50%;height:100px;transform:translate(1px,0)",
            "width:10px;height:10px",
            "position:fixed;right:10px;top:0;width:40px;height:30px",
        );
        assert_eq!(position(&doc, fixed), (200.0, 50.0));
        resize_window(&mut doc, 800, 300);
        assert_eq!(position(&doc, fixed), (400.0, 50.0));

        let stored = *doc.nodes[middle].scrollable_overflow();
        assert_eq!(stored.x1, 390.0, "in the middle box's frame");
    }
}
