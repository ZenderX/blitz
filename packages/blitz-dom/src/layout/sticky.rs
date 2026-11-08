//! `position: sticky` boxes whose scroll container is the viewport.
//!
//! `stylo_taffy` lays a sticky box out in flow, as a `position: relative` one with no
//! inset offset. After the main layout and rounding, this pass shifts each sticky box
//! toward its insets, measured from the viewport's current scroll, and stops it at the
//! edge of its containing block. The shift is added to the box's `final_layout` location,
//! the one field that painting, hit-testing and `absolute_position` all read, so none of
//! them know about sticky.
//!
//! The in-flow position is never stored: `round_layout` rewrites every `final_layout` from
//! `unrounded_layout` on each `resolve_layout`, so the pass always starts from it and a
//! scroll, which sets no damage, is picked up by the next `resolve`.
//!
//! A sticky box inside a scroll container other than the viewport keeps its in-flow
//! position: the scroll offset of such a container is not tracked here. Boxes placed by an
//! ancestor's sticky shift move with it, because ancestors are shifted first, except a
//! fixed box whose containing block is the viewport, which is shifted back.
//!
//! The root element's `overflow`, and the `<body>`'s when the root's is `visible`, belongs
//! to the viewport: neither makes its descendants' scroll container something else.

use crate::node::Node;
use crate::{BaseDocument, NodeId};
use markup5ever::local_name;
use style::properties::generated::longhands::position::computed_value::T as Position;
use style::values::computed::{Length, LengthPercentage, Overflow, Percentage};
use style::values::generics::position::Inset;

/// A box's extent along one axis in document space, with what bounds its shift.
struct Axis {
    /// The border box.
    start: f32,
    size: f32,
    margin_start: f32,
    margin_end: f32,
    /// The containing block's content box.
    container_start: f32,
    container_end: f32,
    /// The viewport.
    port_start: f32,
    port_end: f32,
    inset_start: Option<f32>,
    inset_end: Option<f32>,
}

impl Axis {
    /// The distance the box moves along this axis. The end inset applies first, so the
    /// start inset wins when both cannot hold.
    fn offset(&self) -> f32 {
        let mut offset: f32 = 0.0;
        if let Some(inset) = self.inset_end {
            offset = offset.min(self.port_end - inset - (self.start + self.size));
        }
        if let Some(inset) = self.inset_start {
            offset = offset.max(self.port_start + inset - self.start);
        }

        // The margin box stays inside the containing block, but a box already outside it
        // is not pulled back.
        let room_after = self.container_end - (self.start + self.size + self.margin_end);
        let room_before = self.container_start - (self.start - self.margin_start);
        offset
            .min(room_after.max(0.0))
            .max(room_before.min(0.0))
            .round()
    }
}

fn is_scroll_container(node: &Node) -> bool {
    node.primary_styles().is_some_and(|style| {
        clips_overflow(style.clone_overflow_x()) || clips_overflow(style.clone_overflow_y())
    })
}

fn clips_overflow(overflow: Overflow) -> bool {
    matches!(
        overflow,
        Overflow::Hidden | Overflow::Scroll | Overflow::Auto
    )
}

/// An inset in px, or `None` for `auto`. Anchor positioning is not enabled.
fn resolve_inset(inset: &Inset<Percentage, LengthPercentage>, basis: f32) -> Option<f32> {
    match inset {
        Inset::LengthPercentage(value) => Some(value.resolve(Length::new(basis)).px()),
        Inset::Auto
        | Inset::AnchorFunction(_)
        | Inset::AnchorSizeFunction(_)
        | Inset::AnchorContainingCalcFunction(_) => None,
    }
}

impl BaseDocument {
    /// Document-space origin of a node's border box, from the final layout.
    fn final_origin(&self, id: NodeId) -> taffy::Point<f32> {
        let mut origin = taffy::Point::ZERO;
        let mut current = Some(id);
        while let Some(id) = current {
            let node = &self.nodes[id];
            origin.x += node.final_layout().location.x;
            origin.y += node.final_layout().location.y;
            current = node.layout_parent.get();
        }
        origin
    }

    /// Shift every sticky box toward its insets. Runs after rounding, so each box starts
    /// from its in-flow position. A fixed box inside a shifted one gets the shifts carried
    /// since its nearest fixed ancestor back, if its containing block is the viewport.
    pub(crate) fn layout_sticky_boxes(&mut self) {
        let root = self.root_element();
        let root_id = root.id;
        // With a visible root overflow, the `<body>` child's overflow belongs to the
        // viewport too (CSS Overflow 3), so body is not a scroll container.
        let body_overflow_is_propagated = !is_scroll_container(root);

        // Pre-order, so a box is placed after its ancestors and before its descendants,
        // which measure from where it ends up.
        let mut stack = vec![(root_id, false, taffy::Point::ZERO)];
        while let Some((id, is_in_scroll_container, carried_shift)) = stack.pop() {
            let Some(node) = self.nodes.get(id) else {
                continue;
            };
            let position = node.primary_styles().map(|style| style.clone_position());
            let child_shift = match position {
                Some(Position::Sticky) if !is_in_scroll_container => {
                    let shift = self.layout_sticky_box(id);
                    taffy::Point {
                        x: carried_shift.x + shift.x,
                        y: carried_shift.y + shift.y,
                    }
                }
                Some(Position::Fixed) => {
                    if carried_shift != taffy::Point::ZERO {
                        self.cancel_sticky_shift(id, carried_shift);
                    }
                    taffy::Point::ZERO
                }
                _ => carried_shift,
            };

            let node = &self.nodes[id];
            if let Some(children) = node.layout_children.borrow().as_ref() {
                // The root element's overflow belongs to the viewport.
                let is_viewport_overflow = id == root_id
                    || (body_overflow_is_propagated
                        && node.parent == Some(root_id)
                        && node.data.is_element_with_tag_name(&local_name!("body")));
                let is_child_in_scroll_container =
                    is_in_scroll_container || (!is_viewport_overflow && is_scroll_container(node));
                stack.extend(
                    children
                        .iter()
                        .rev()
                        .map(|&child| (child, is_child_in_scroll_container, child_shift)),
                );
            }
        }
    }

    /// Shift a sticky box toward its insets; returns the shift.
    fn layout_sticky_box(&mut self, id: NodeId) -> taffy::Point<f32> {
        let Some(parent_id) = self.nodes[id].layout_parent.get() else {
            return taffy::Point::ZERO;
        };

        let viewport = self.stylist.device().au_viewport_size();
        let (port_width, port_height) = (viewport.width.to_f32_px(), viewport.height.to_f32_px());
        let scroll = self.viewport_scroll();
        let (port_left, port_top) = (scroll.x as f32, scroll.y as f32);

        let node = &self.nodes[id];
        let Some(style) = node.primary_styles() else {
            return taffy::Point::ZERO;
        };
        let position = style.get_position();
        let top = resolve_inset(&position.top, port_height);
        let bottom = resolve_inset(&position.bottom, port_height);
        let left = resolve_inset(&position.left, port_width);
        let right = resolve_inset(&position.right, port_width);
        drop(style);
        if top.is_none() && bottom.is_none() && left.is_none() && right.is_none() {
            return taffy::Point::ZERO;
        }

        let layout = node.final_layout();
        let origin = self.final_origin(id);

        let parent = self.nodes[parent_id].final_layout();
        let parent_origin = self.final_origin(parent_id);
        let container_left = parent_origin.x + parent.border.left + parent.padding.left;
        let container_top = parent_origin.y + parent.border.top + parent.padding.top;
        let container_width = parent.size.width
            - parent.border.left
            - parent.border.right
            - parent.padding.left
            - parent.padding.right
            - parent.scrollbar_size.width;
        let container_height = parent.size.height
            - parent.border.top
            - parent.border.bottom
            - parent.padding.top
            - parent.padding.bottom
            - parent.scrollbar_size.height;

        let horizontal = Axis {
            start: origin.x,
            size: layout.size.width,
            margin_start: layout.margin.left,
            margin_end: layout.margin.right,
            container_start: container_left,
            container_end: container_left + container_width,
            port_start: port_left,
            port_end: port_left + port_width,
            inset_start: left,
            inset_end: right,
        };
        let vertical = Axis {
            start: origin.y,
            size: layout.size.height,
            margin_start: layout.margin.top,
            margin_end: layout.margin.bottom,
            container_start: container_top,
            container_end: container_top + container_height,
            port_start: port_top,
            port_end: port_top + port_height,
            inset_start: top,
            inset_end: bottom,
        };

        let shift = taffy::Point {
            x: horizontal.offset(),
            y: vertical.offset(),
        };
        let location = &mut self.nodes[id].final_layout_mut().location;
        location.x += shift.x;
        location.y += shift.y;
        shift
    }
}

#[cfg(test)]
mod tests {
    use crate::{Attribute, BaseDocument, DocumentConfig, NodeId, Point, qual_name};
    use blitz_traits::shell::{ColorScheme, Viewport};

    fn layout(lead: u32, section: &str, sticky: &str) -> (BaseDocument, NodeId) {
        layout_with("", "", lead, section, sticky)
    }

    /// Lay out, in a 400x300 viewport with the document scrolled to (0, 0),
    /// `<html style="{html}"><body style="margin:0;{body}"><div 100px>
    /// <div style="height:600px;width:600px;{section}">
    /// <div style="height:{lead}px"><div style="height:40px;width:40px;{sticky}">`.
    /// The sticky box starts at y = 100 + `lead`, and its containing block spans
    /// y 100..700. Returns the document and the sticky box.
    fn layout_with(
        html_style: &str,
        body_style: &str,
        lead: u32,
        section: &str,
        sticky: &str,
    ) -> (BaseDocument, NodeId) {
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
        let html = mutator.create_element(
            qual_name!("html"),
            style(&format!("display:block;{html_style}")),
        );
        let body = mutator.create_element(
            qual_name!("body"),
            style(&format!("display:block;margin:0;{body_style}")),
        );
        let spacer = mutator.create_element(qual_name!("div"), style("display:block;height:100px"));
        let section_id = mutator.create_element(
            qual_name!("div"),
            style(&format!("display:block;height:600px;width:600px;{section}")),
        );
        let lead_id = mutator.create_element(
            qual_name!("div"),
            style(&format!("display:block;height:{lead}px")),
        );
        let sticky_id = mutator.create_element(
            qual_name!("div"),
            style(&format!(
                "display:block;height:40px;width:40px;position:sticky;{sticky}"
            )),
        );
        mutator.append_children(section_id, &[lead_id, sticky_id]);
        mutator.append_children(body, &[spacer, section_id]);
        mutator.append_children(html, &[body]);
        mutator.append_children(root_id, &[html]);
        drop(mutator);

        doc.resolve(0.0);
        (doc, sticky_id)
    }

    fn scroll_to(doc: &mut BaseDocument, x: f64, y: f64) {
        doc.set_viewport_scroll(Point { x, y });
        doc.resolve(0.0);
    }

    fn position(doc: &BaseDocument, id: NodeId) -> (f32, f32) {
        let point = doc.nodes[id].absolute_position(0.0, 0.0);
        (point.x, point.y)
    }

    #[test]
    fn sticks_to_the_top_inset_once_scrolled_past_it() {
        let (mut doc, sticky) = layout(0, "", "top:10px");
        assert_eq!(position(&doc, sticky), (0.0, 100.0), "in flow at scroll 0");

        scroll_to(&mut doc, 0.0, 50.0);
        assert_eq!(
            position(&doc, sticky),
            (0.0, 100.0),
            "still below the inset"
        );

        scroll_to(&mut doc, 0.0, 250.0);
        assert_eq!(position(&doc, sticky), (0.0, 260.0), "scroll + inset");
    }

    #[test]
    fn is_released_at_the_end_of_its_containing_block() {
        let (mut doc, sticky) = layout(0, "", "top:0");
        scroll_to(&mut doc, 0.0, 650.0);
        assert_eq!(
            position(&doc, sticky),
            (0.0, 650.0),
            "still stuck, 690 < 700"
        );

        scroll_to(&mut doc, 0.0, 690.0);
        assert_eq!(
            position(&doc, sticky),
            (0.0, 660.0),
            "bottom edge on the container's end at 700"
        );
    }

    #[test]
    fn a_stuck_box_returns_to_its_flow_position_when_scrolled_back() {
        let (mut doc, sticky) = layout(0, "", "top:0");
        scroll_to(&mut doc, 0.0, 250.0);
        scroll_to(&mut doc, 0.0, 250.0);
        assert_eq!(position(&doc, sticky), (0.0, 250.0), "repeat resolves hold");

        scroll_to(&mut doc, 0.0, 0.0);
        assert_eq!(position(&doc, sticky), (0.0, 100.0));
    }

    #[test]
    fn bottom_inset_pulls_a_box_below_the_viewport_up_to_its_edge() {
        let (mut doc, sticky) = layout(400, "", "bottom:0");
        assert_eq!(position(&doc, sticky), (0.0, 260.0), "bottom edge at 300");

        scroll_to(&mut doc, 0.0, 400.0);
        assert_eq!(position(&doc, sticky), (0.0, 500.0), "already inside");
    }

    #[test]
    fn left_inset_sticks_horizontally() {
        let (mut doc, sticky) = layout(0, "", "left:5px");
        scroll_to(&mut doc, 50.0, 0.0);
        assert_eq!(position(&doc, sticky), (55.0, 100.0));
    }

    #[test]
    fn insets_are_not_a_relative_offset() {
        // As a relative inset, `top:30px` would move the box to y = 130.
        let (doc, sticky) = layout(0, "", "top:30px");
        assert_eq!(position(&doc, sticky), (0.0, 100.0));
    }

    #[test]
    fn a_sticky_box_in_an_inner_scroll_container_keeps_its_flow_position() {
        let (mut doc, sticky) = layout(0, "overflow:hidden", "top:0");
        scroll_to(&mut doc, 0.0, 250.0);
        assert_eq!(position(&doc, sticky), (0.0, 100.0));
    }

    #[test]
    fn body_overflow_propagates_to_the_viewport_when_the_root_is_visible() {
        let (mut doc, sticky) = layout_with("", "overflow-x:hidden", 0, "", "top:0");
        scroll_to(&mut doc, 0.0, 250.0);
        assert_eq!(position(&doc, sticky), (0.0, 250.0), "scroll + inset");
    }

    #[test]
    fn body_is_a_scroll_container_when_the_root_clips_too() {
        let (mut doc, sticky) = layout_with("overflow:hidden", "overflow:hidden", 0, "", "top:0");
        scroll_to(&mut doc, 0.0, 250.0);
        assert_eq!(position(&doc, sticky), (0.0, 100.0), "stays in flow");
    }

    /// Append `<div style="{style}">` to `parent` and lay out again.
    fn append_child(doc: &mut BaseDocument, parent: NodeId, style: &str) -> NodeId {
        let mut mutator = doc.mutate();
        let child = mutator.create_element(
            qual_name!("div"),
            vec![Attribute {
                name: qual_name!("style"),
                value: style.to_string(),
            }],
        );
        mutator.append_children(parent, &[child]);
        drop(mutator);
        doc.resolve(0.0);
        child
    }

    const FIXED: &str = "position:fixed;left:50px;top:20px;width:40px;height:30px";

    /// The box is placed from the viewport, so the stuck ancestor's shift must not carry
    /// it; the hit test reaches it outside the ancestor's own box.
    #[test]
    fn a_fixed_box_inside_a_stuck_box_keeps_its_viewport_position() {
        let (mut doc, sticky) = layout(0, "", "top:0");
        let fixed = append_child(&mut doc, sticky, FIXED);
        scroll_to(&mut doc, 0.0, 250.0);
        assert_eq!(position(&doc, sticky), (0.0, 250.0), "stuck");
        assert_eq!(position(&doc, fixed), (50.0, 270.0), "scroll + inset only");
        assert_eq!(doc.hit(55.0, 275.0).map(|hit| hit.node_id), Some(fixed));

        scroll_to(&mut doc, 0.0, 0.0);
        assert_eq!(position(&doc, fixed), (50.0, 20.0), "scrolled back");
    }

    /// A transformed sticky box is the fixed box's containing block, so the fixed box
    /// moves with it: 10px below the stuck box, not 20px below the viewport's top.
    #[test]
    fn a_fixed_box_contained_by_a_stuck_box_moves_with_it() {
        let (mut doc, sticky) = layout(0, "", "top:10px;transform:translate(0)");
        let fixed = append_child(&mut doc, sticky, FIXED);
        scroll_to(&mut doc, 0.0, 250.0);
        assert_eq!(position(&doc, sticky), (0.0, 260.0), "stuck");
        assert_eq!(position(&doc, fixed), (50.0, 280.0));
    }

    #[test]
    fn hit_testing_finds_the_box_where_it_is_stuck() {
        let (mut doc, sticky) = layout(0, "", "top:0");
        scroll_to(&mut doc, 0.0, 250.0);
        assert_eq!(doc.hit(5.0, 255.0).map(|hit| hit.node_id), Some(sticky));
        assert_ne!(
            doc.hit(5.0, 105.0).map(|hit| hit.node_id),
            Some(sticky),
            "its flow position no longer hits it"
        );
    }
}
