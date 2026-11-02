//! Table captions (css-tables-3 §3.1, css-logical-1 `caption-side`). blitz has no table
//! wrapper box, so a table root with captions stands for one: its grid is laid out in the
//! room the captions leave and each caption goes on its `caption-side`. `top` and
//! `bottom` captions take the grid's width and stack above or below it. `inline-start` and
//! `inline-end` captions are sized shrink-to-fit and sit beside the grid, level with its
//! top, on the side where the table's inline direction starts or ends. A known size is the
//! grid's: the `width`/`height` of a table size its table box, not the wrapper.
//!
//! Layout runs in the table's flow frame (see `writing_mode`), where top/bottom are the
//! block-start/end sides, and in a vertical flow left is the inline-start side.

use std::sync::Arc;

use style::values::computed::table::CaptionSide;
use taffy::{
    AvailableSpace, Baselines, BoxSizing, CollapsibleMarginSet, Layout, LayoutInput, LayoutOutput,
    LayoutPartialTree as _, Line, MaybeMath as _, MaybeResolve as _, NodeId, Point, Rect,
    RequestedAxis, ResolveOrZero as _, RunMode, Size, SizingMode, compute_grid_layout,
};

use super::resolve_calc_value;
use super::table::{TableContext, TableTreeWrapper};
use crate::{BaseDocument, NodeId as DomNodeId, dom_node_id, taffy_node_id};

/// A caption's side of the grid, in the table's flow frame.
#[derive(Clone, Copy, PartialEq)]
enum Side {
    Top,
    Bottom,
    Left,
    Right,
}

struct Caption {
    id: DomNodeId,
    side: Side,
    size: Size<f32>,
    margin: Rect<f32>,
}

impl Caption {
    fn outer(&self) -> Size<f32> {
        self.size + self.margin.sum_axes()
    }
}

impl BaseDocument {
    /// Lays out a table root: its grid, then the captions around it.
    pub(crate) fn compute_table_layout(
        &mut self,
        node_id: NodeId,
        inputs: LayoutInput,
        ctx: Arc<TableContext>,
    ) -> LayoutOutput {
        if ctx.captions.is_empty() {
            return compute_grid_layout(&mut TableTreeWrapper { doc: self, ctx }, node_id, inputs);
        }
        let Some(writing_mode) = self.nodes[dom_node_id(node_id)]
            .primary_styles()
            .map(|style| style.writing_mode)
        else {
            return compute_grid_layout(&mut TableTreeWrapper { doc: self, ctx }, node_id, inputs);
        };
        let inline_start_is_left = writing_mode.is_vertical() || writing_mode.is_bidi_ltr();
        let basis = inputs.parent_size.width;

        let mut captions: Vec<Caption> = Vec::with_capacity(ctx.captions.len());
        for &id in &ctx.captions {
            let Some(caption_side) = self.nodes[id]
                .primary_styles()
                .map(|style| style.get_inherited_table().clone_caption_side())
            else {
                continue;
            };
            let side = match (caption_side, inline_start_is_left) {
                (CaptionSide::Top, _) => Side::Top,
                (CaptionSide::Bottom, _) => Side::Bottom,
                (CaptionSide::InlineStart, true) | (CaptionSide::InlineEnd, false) => Side::Left,
                (CaptionSide::InlineStart, false) | (CaptionSide::InlineEnd, true) => Side::Right,
            };
            let margin = self.nodes[id]
                .style_in_parent_frame()
                .margin
                .resolve_or_zero(basis, resolve_calc_value);
            captions.push(Caption {
                id,
                side,
                size: Size::ZERO,
                margin,
            });
        }

        // Side captions are shrink-to-fit in the table's available width.
        let mut side_width = 0.0;
        for caption in captions
            .iter_mut()
            .filter(|c| matches!(c.side, Side::Left | Side::Right))
        {
            let margins = caption.margin.horizontal_axis_sum();
            let min = self.caption_width(caption.id, inputs, AvailableSpace::MinContent);
            let max = self.caption_width(caption.id, inputs, AvailableSpace::MaxContent);
            let width = match inputs.available_space.width {
                AvailableSpace::Definite(available) => (available - margins).min(max).max(min),
                AvailableSpace::MinContent => min,
                AvailableSpace::MaxContent => max,
            };
            caption.size = self.caption_size(caption.id, inputs, width);
            side_width += caption.outer().width;
        }

        let grid_inputs = LayoutInput {
            available_space: Size {
                width: inputs.available_space.width.maybe_sub(side_width),
                height: inputs.available_space.height,
            },
            ..inputs
        };
        let grid = compute_grid_layout(
            &mut TableTreeWrapper {
                doc: self,
                ctx: Arc::clone(&ctx),
            },
            node_id,
            grid_inputs,
        );

        for caption in captions
            .iter_mut()
            .filter(|c| matches!(c.side, Side::Top | Side::Bottom))
        {
            let stretched = (grid.size.width - caption.margin.horizontal_axis_sum()).max(0.0);
            let width = self
                .caption_style_width(caption.id, grid.size.width)
                .unwrap_or(stretched);
            caption.size = self.caption_size(caption.id, inputs, width);
        }

        let extent = |side: Side, axis: fn(Size<f32>) -> f32| {
            captions
                .iter()
                .filter(|c| c.side == side)
                .map(|c| axis(c.outer()))
                .sum::<f32>()
        };
        let left = extent(Side::Left, |s| s.width);
        let top = extent(Side::Top, |s| s.height);
        let middle = captions
            .iter()
            .filter(|c| matches!(c.side, Side::Left | Side::Right))
            .map(|c| c.outer().height)
            .fold(grid.size.height, f32::max);
        let size = Size {
            width: side_width + grid.size.width,
            height: top + middle + extent(Side::Bottom, |s| s.height),
        };

        if inputs.run_mode == RunMode::ComputeSize {
            return LayoutOutput::from_outer_size(size);
        }

        let grid_origin = Point { x: left, y: top };
        for cell in &ctx.cells {
            let location = &mut self.nodes[cell.node_id].unrounded_layout_mut().location;
            location.x += grid_origin.x;
            location.y += grid_origin.y;
        }
        *ctx.grid_box.borrow_mut() = Some((grid_origin, grid.size));

        let mut cursor = Point { x: 0.0, y: 0.0 };
        let mut right = left + grid.size.width;
        let mut bottom = top + middle;
        for (order, caption) in captions.iter().enumerate() {
            let outer = caption.outer();
            let origin = match caption.side {
                Side::Top => {
                    cursor.y += outer.height;
                    Point {
                        x: left,
                        y: cursor.y - outer.height,
                    }
                }
                Side::Bottom => {
                    bottom += outer.height;
                    Point {
                        x: left,
                        y: bottom - outer.height,
                    }
                }
                Side::Left => {
                    cursor.x += outer.width;
                    Point {
                        x: cursor.x - outer.width,
                        y: top,
                    }
                }
                Side::Right => {
                    right += outer.width;
                    Point {
                        x: right - outer.width,
                        y: top,
                    }
                }
            };
            let style = self.nodes[caption.id].style_in_parent_frame();
            let padding = style.padding.resolve_or_zero(basis, resolve_calc_value);
            let border = style.border.resolve_or_zero(basis, resolve_calc_value);
            self.set_unrounded_layout(
                taffy_node_id(caption.id),
                &Layout {
                    order: (ctx.cells.len() + order) as u32,
                    location: Point {
                        x: origin.x + caption.margin.left,
                        y: origin.y + caption.margin.top,
                    },
                    size: caption.size,
                    padding,
                    border,
                    margin: caption.margin,
                    ..Layout::new()
                },
            );
        }

        LayoutOutput {
            size,
            scrollable_overflow_rect: Rect {
                left: 0.0,
                right: size.width,
                top: 0.0,
                bottom: size.height,
            },
            baselines: Baselines {
                first: grid.baselines.first.map(|y| y + grid_origin.y),
                last: grid.baselines.last.map(|y| y + grid_origin.y),
            },
            top_margin: CollapsibleMarginSet::ZERO,
            bottom_margin: CollapsibleMarginSet::ZERO,
            margins_can_collapse_through: false,
        }
    }

    /// A caption's border-box `width`, when it has a definite one in a wrapper `basis` wide.
    fn caption_style_width(&self, id: DomNodeId, basis: f32) -> Option<f32> {
        let style = self.nodes[id].style_in_parent_frame();
        let width = style
            .size
            .width
            .maybe_resolve(Some(basis), resolve_calc_value)?;
        Some(match style.box_sizing {
            BoxSizing::BorderBox => width,
            BoxSizing::ContentBox => {
                width
                    + (style
                        .padding
                        .resolve_or_zero(Some(basis), resolve_calc_value)
                        + style
                            .border
                            .resolve_or_zero(Some(basis), resolve_calc_value))
                    .horizontal_axis_sum()
            }
        })
    }

    /// A caption's border-box width under `space`.
    fn caption_width(&mut self, id: DomNodeId, inputs: LayoutInput, space: AvailableSpace) -> f32 {
        let measure = LayoutInput {
            run_mode: RunMode::ComputeSize,
            sizing_mode: SizingMode::InherentSize,
            axis: RequestedAxis::Horizontal,
            known_dimensions: Size::NONE,
            known_dimensions_are_definite: Size {
                width: false,
                height: false,
            },
            available_space: Size {
                width: space,
                height: AvailableSpace::MaxContent,
            },
            vertical_margins_are_collapsible: Line::FALSE,
            ..inputs
        };
        self.compute_child_layout(taffy_node_id(id), measure)
            .size
            .width
    }

    /// A caption's border-box size at `width`, laid out when the table is.
    fn caption_size(&mut self, id: DomNodeId, inputs: LayoutInput, width: f32) -> Size<f32> {
        let at_width = LayoutInput {
            run_mode: inputs.run_mode,
            sizing_mode: SizingMode::InherentSize,
            axis: RequestedAxis::Both,
            known_dimensions: Size {
                width: Some(width),
                height: None,
            },
            known_dimensions_are_definite: Size {
                width: true,
                height: false,
            },
            available_space: Size {
                width: AvailableSpace::Definite(width),
                height: AvailableSpace::MaxContent,
            },
            vertical_margins_are_collapsible: Line::FALSE,
            ..inputs
        };
        self.compute_child_layout(taffy_node_id(id), at_width).size
    }
}
