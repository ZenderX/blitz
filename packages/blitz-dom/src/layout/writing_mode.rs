//! Vertical writing modes in layout (css-writing-modes-4) as a transposition: a box whose
//! flow is vertical keeps its taffy style in its logical frame, with taffy's x axis the
//! inline axis and y the block axis, so block, flex, grid and inline layout run unchanged
//! inside it. Where a box's flow differs from its parent's, the parent reads the box's
//! style in the parent's own frame (`parent_frame_style`), and a box whose axes are
//! orthogonal to its parent's swaps the parent's layout inputs and its own outputs. After
//! rounding, every box laid out in a vertical flow has its final layout mapped from that
//! flow's logical frame to physical coordinates, which paint, hit testing and the CSSOM read.

use style::Atom;
use style::properties::ComputedValues;
use taffy::{
    AvailableSpace, Baselines, BlockContext, CollapsibleMarginSet, Dimension, Layout, LayoutInput,
    LayoutOutput, Line, Point, Rect, RequestedAxis, RunMode, Size, SizingMode, Style,
    prelude::TaffyMaxContent,
};

use crate::node::{LayoutFlow, combines_text};
use crate::{BaseDocument, Node, NodeId as DomNodeId, dom_node_id};

fn swap<T>(size: Size<T>) -> Size<T> {
    Size {
        width: size.height,
        height: size.width,
    }
}

/// A rect measured inside a box with its axes swapped: logical `left`/`right` (inline)
/// become physical `top`/`bottom` and the reverse. Scrollable overflow is measured this
/// way from the scroll origin in both frames.
fn transpose(rect: Rect<f32>) -> Rect<f32> {
    Rect {
        left: rect.top,
        right: rect.bottom,
        top: rect.left,
        bottom: rect.right,
    }
}

/// Physical sides as `flow`'s logical ones: `left`/`right` the inline start/end,
/// `top`/`bottom` the block start/end.
fn sides_to_flow<T>(rect: Rect<T>, flow: LayoutFlow) -> Rect<T> {
    if !flow.is_vertical() {
        return rect;
    }
    let Rect {
        left,
        right,
        top,
        bottom,
    } = rect;
    let (inline_start, inline_end) = if flow.is_inline_bottom_to_top() {
        (bottom, top)
    } else {
        (top, bottom)
    };
    let (block_start, block_end) = if flow.is_block_left_to_right() {
        (left, right)
    } else {
        (right, left)
    };
    Rect {
        left: inline_start,
        right: inline_end,
        top: block_start,
        bottom: block_end,
    }
}

/// The inverse of [`sides_to_flow`].
fn sides_from_flow<T>(rect: Rect<T>, flow: LayoutFlow) -> Rect<T> {
    if !flow.is_vertical() {
        return rect;
    }
    let Rect {
        left: inline_start,
        right: inline_end,
        top: block_start,
        bottom: block_end,
    } = rect;
    let (top, bottom) = if flow.is_inline_bottom_to_top() {
        (inline_end, inline_start)
    } else {
        (inline_start, inline_end)
    };
    let (left, right) = if flow.is_block_left_to_right() {
        (block_start, block_end)
    } else {
        (block_end, block_start)
    };
    Rect {
        left,
        right,
        top,
        bottom,
    }
}

/// A physical taffy `style` as a box laying out in `flow` reads it.
fn style_in_flow(style: &Style<Atom>, flow: LayoutFlow) -> Style<Atom> {
    let mut style = style.clone();
    if flow.is_vertical() {
        style.size = swap(style.size);
        style.min_size = swap(style.min_size);
        style.max_size = swap(style.max_size);
        style.aspect_ratio = style.aspect_ratio.map(|ratio| 1.0 / ratio);
        style.margin = sides_to_flow(style.margin, flow);
        style.padding = sides_to_flow(style.padding, flow);
        style.border = sides_to_flow(style.border, flow);
        style.inset = sides_to_flow(style.inset, flow);
        style.overflow = style.overflow.transpose();
    }
    style
}

/// The physical taffy `style` of a box with `flow` inside a parent with `parent_flow`:
/// its own frame's style, and the parent's view of it where the two flows differ.
pub(crate) fn styles_for_flow(
    style: Style<Atom>,
    flow: LayoutFlow,
    parent_flow: LayoutFlow,
) -> (Style<Atom>, Option<Box<Style<Atom>>>) {
    let parent_frame_style = (flow != parent_flow).then(|| {
        let mut parent_style = style_in_flow(&style, parent_flow);
        // An orthogonal box's auto block size is its content's (css-writing-modes-4 §7.3),
        // and its block axis is the parent's inline axis.
        if flow.is_vertical() != parent_flow.is_vertical() && parent_style.size.width.is_auto() {
            parent_style.size.width = Dimension::fit_content();
        }
        Box::new(parent_style)
    });
    let style = if flow.is_vertical() {
        style_in_flow(&style, flow)
    } else {
        style
    };
    (style, parent_frame_style)
}

/// The flow of a box with `style` whose DOM parent has `parent_style`.
pub(crate) fn flow_of(style: &ComputedValues, parent_style: Option<&ComputedValues>) -> LayoutFlow {
    LayoutFlow::of(style, combines_text(style, parent_style))
}

/// A child's final `layout`, laid out in a parent with `flow` and physical border-box
/// `parent_size`, in physical coordinates.
fn layout_from_flow(layout: Layout, flow: LayoutFlow, parent_size: Size<f32>) -> Layout {
    let Point {
        x: inline,
        y: block,
    } = layout.location;
    let size = swap(layout.size);
    let x = if flow.is_block_left_to_right() {
        block
    } else {
        parent_size.width - block - size.width
    };
    let y = if flow.is_inline_bottom_to_top() {
        parent_size.height - inline - size.height
    } else {
        inline
    };
    Layout {
        order: layout.order,
        location: Point { x, y },
        size,
        scrollable_overflow_rect: transpose(layout.scrollable_overflow_rect),
        scrollbar_size: swap(layout.scrollbar_size),
        border: sides_from_flow(layout.border, flow),
        padding: sides_from_flow(layout.padding, flow),
        margin: sides_from_flow(layout.margin, flow),
    }
}

/// How a box's layout relates to its parent's frame.
enum FlowBoundary {
    /// Laid out in the parent's frame.
    Same,
    /// Its own flow, with the same axes as the parent's.
    Parallel,
    /// Its axes swapped against the parent's.
    Orthogonal,
    /// A `text-combine-upright` cell.
    CombinedText,
}

impl Node {
    /// This box's taffy style as its layout parent's algorithm reads it.
    pub(crate) fn style_in_parent_frame(&self) -> &Style<Atom> {
        self.element_data()
            .and_then(|element| element.parent_frame_style.as_deref())
            .unwrap_or_else(|| self.style())
    }

    fn flow_boundary(&self) -> FlowBoundary {
        let Some(element) = self.element_data() else {
            return FlowBoundary::Same;
        };
        let (flow, parent_flow) = (element.layout_flow, element.parent_layout_flow);
        if flow == parent_flow {
            FlowBoundary::Same
        } else if flow == LayoutFlow::CombinedText {
            FlowBoundary::CombinedText
        } else if flow.is_vertical() != parent_flow.is_vertical() {
            FlowBoundary::Orthogonal
        } else {
            FlowBoundary::Parallel
        }
    }
}

impl BaseDocument {
    /// Lays a box out, crossing into its own flow first where it differs from its
    /// parent's. `inputs` are in the parent's frame, as is the output.
    pub(crate) fn compute_child_layout_internal(
        &mut self,
        node_id: taffy::NodeId,
        inputs: LayoutInput,
        block_ctx: Option<&mut BlockContext<'_>>,
    ) -> LayoutOutput {
        match self.nodes[dom_node_id(node_id)].flow_boundary() {
            FlowBoundary::Same => self.compute_child_layout_in_frame(node_id, inputs, block_ctx),
            // A box in another writing mode establishes an independent formatting context.
            FlowBoundary::Parallel => self.compute_child_layout_in_frame(
                node_id,
                LayoutInput {
                    vertical_margins_are_collapsible: Line::FALSE,
                    ..inputs
                },
                None,
            ),
            FlowBoundary::Orthogonal => self.compute_orthogonal_layout(node_id, inputs),
            FlowBoundary::CombinedText => self.compute_combined_text_layout(node_id),
        }
    }

    fn compute_orthogonal_layout(
        &mut self,
        node_id: taffy::NodeId,
        inputs: LayoutInput,
    ) -> LayoutOutput {
        let mut own = LayoutInput {
            axis: match inputs.axis {
                RequestedAxis::Horizontal => RequestedAxis::Vertical,
                RequestedAxis::Vertical => RequestedAxis::Horizontal,
                RequestedAxis::Both => RequestedAxis::Both,
            },
            known_dimensions: swap(inputs.known_dimensions),
            known_dimensions_are_definite: swap(inputs.known_dimensions_are_definite),
            parent_size: swap(inputs.parent_size),
            available_space: swap(inputs.available_space),
            vertical_margins_are_collapsible: Line::FALSE,
            ..inputs
        };
        // An auto inline size in an orthogonal flow is fit-content against the space the
        // parent offers in that axis (css-writing-modes-4 §7.3).
        if own.known_dimensions.width.is_none()
            && own.sizing_mode == SizingMode::InherentSize
            && self.nodes[dom_node_id(node_id)]
                .style()
                .size
                .width
                .is_auto()
        {
            let measure = |doc: &mut Self, width: AvailableSpace| {
                let measure_inputs = LayoutInput {
                    run_mode: RunMode::ComputeSize,
                    axis: RequestedAxis::Horizontal,
                    available_space: Size {
                        width,
                        height: own.available_space.height,
                    },
                    ..own
                };
                doc.compute_child_layout_in_frame(node_id, measure_inputs, None)
                    .size
                    .width
            };
            own.known_dimensions.width = Some(match own.available_space.width {
                AvailableSpace::Definite(space) => {
                    let max_content = measure(self, AvailableSpace::MaxContent);
                    let min_content = measure(self, AvailableSpace::MinContent);
                    max_content.min(space).max(min_content)
                }
                AvailableSpace::MinContent => measure(self, AvailableSpace::MinContent),
                AvailableSpace::MaxContent => measure(self, AvailableSpace::MaxContent),
            });
        }
        let output = self.compute_child_layout_in_frame(node_id, own, None);
        LayoutOutput {
            size: swap(output.size),
            scrollable_overflow_rect: transpose(output.scrollable_overflow_rect),
            // The box's baselines run across the parent's lines, so it aligns by its edge.
            baselines: Baselines {
                first: None,
                last: None,
            },
            top_margin: CollapsibleMarginSet::ZERO,
            bottom_margin: CollapsibleMarginSet::ZERO,
            margins_can_collapse_through: false,
        }
    }

    /// A `text-combine-upright` cell: its text laid out horizontally on one line, in a
    /// box one em square in the vertical line (css-writing-modes-4 §9.1.1); paint
    /// compresses the text into the em when it is wider.
    fn compute_combined_text_layout(&mut self, node_id: taffy::NodeId) -> LayoutOutput {
        let em = self.nodes[dom_node_id(node_id)]
            .primary_styles()
            .map_or(0.0, |style| style.clone_font_size().used_size().px());
        let one_line = LayoutInput {
            run_mode: RunMode::PerformLayout,
            sizing_mode: SizingMode::ContentSize,
            axis: RequestedAxis::Both,
            known_dimensions: Size::NONE,
            known_dimensions_are_definite: Size {
                width: false,
                height: false,
            },
            parent_size: Size::NONE,
            available_space: Size::MAX_CONTENT,
            vertical_margins_are_collapsible: Line::FALSE,
        };
        self.compute_child_layout_in_frame(node_id, one_line, None);
        LayoutOutput::from_outer_size(Size {
            width: em,
            height: em,
        })
    }

    /// Maps the final layouts of the boxes laid out in a vertical flow, in `node_id`'s
    /// subtree, from that flow's logical frame to physical coordinates. Runs after
    /// rounding, top-down, so a parent's physical size is known before its children map.
    pub(crate) fn map_vertical_layouts(&mut self, node_id: DomNodeId) {
        let flow = self.nodes[node_id]
            .element_data()
            .map_or(LayoutFlow::HorizontalTb, |element| element.layout_flow);
        let size = self.nodes[node_id].final_layout().size;
        let children = std::mem::take(self.nodes[node_id].layout_children.get_mut());
        for &child in children.iter().flatten() {
            if flow.is_vertical() {
                let layout = self.nodes[child].final_layout_mut();
                *layout = layout_from_flow(*layout, flow, size);
            }
            self.map_vertical_layouts(child);
        }
        *self.nodes[node_id].layout_children.get_mut() = children;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: f32, right: f32, top: f32, bottom: f32) -> Rect<f32> {
        Rect {
            left,
            right,
            top,
            bottom,
        }
    }

    #[test]
    fn sides_map_to_the_flow_and_back() {
        let physical = rect(1.0, 2.0, 3.0, 4.0);
        // vertical-rl: inline start is the top, block start the right.
        assert_eq!(
            sides_to_flow(physical, LayoutFlow::VerticalRl),
            rect(3.0, 4.0, 2.0, 1.0)
        );
        // vertical-lr: block start is the left.
        assert_eq!(
            sides_to_flow(physical, LayoutFlow::VerticalLr),
            rect(3.0, 4.0, 1.0, 2.0)
        );
        // sideways-lr: inline start is the bottom.
        assert_eq!(
            sides_to_flow(physical, LayoutFlow::SidewaysLr),
            rect(4.0, 3.0, 1.0, 2.0)
        );
        for flow in [
            LayoutFlow::HorizontalTb,
            LayoutFlow::VerticalRl,
            LayoutFlow::VerticalLr,
            LayoutFlow::SidewaysRl,
            LayoutFlow::SidewaysLr,
        ] {
            assert_eq!(
                sides_from_flow(sides_to_flow(physical, flow), flow),
                physical
            );
        }
    }

    #[test]
    fn a_vertical_rl_child_maps_from_the_right_edge() {
        let logical = Layout {
            location: Point { x: 5.0, y: 20.0 },
            size: Size {
                width: 200.0,
                height: 30.0,
            },
            ..Layout::new()
        };
        let parent = Size {
            width: 120.0,
            height: 200.0,
        };
        let rl = layout_from_flow(logical, LayoutFlow::VerticalRl, parent);
        assert_eq!(rl.location, Point { x: 70.0, y: 5.0 });
        assert_eq!(
            rl.size,
            Size {
                width: 30.0,
                height: 200.0
            }
        );
        let lr = layout_from_flow(logical, LayoutFlow::VerticalLr, parent);
        assert_eq!(lr.location, Point { x: 20.0, y: 5.0 });
        let slr = layout_from_flow(logical, LayoutFlow::SidewaysLr, parent);
        assert_eq!(slr.location, Point { x: 20.0, y: -5.0 });
    }
}
