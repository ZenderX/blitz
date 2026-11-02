//! Multi-column layout (css-multicol-1): a block container with a non-auto
//! `column-count` or `column-width` lays its content out once at the column width, then
//! moves each in-flow child into the column its place in that single-column flow falls in.
//!
//! The in-flow children are the fragmentation unit: a column break falls only between
//! two of them, and each is monolithic, so a child taller than the column stays whole
//! in one column. The margin at a break is truncated. The column height is the least
//! that fits the content in `column-count` columns (balancing, by bisection) unless the
//! height is definite and `column-fill` is `auto`: then columns fill in turn to that
//! height, and content past the last column overflows into further columns inline.

use style::computed_values::column_fill::T as ColumnFill;
use style::properties::ComputedValues;
use style::values::computed::length::NonNegativeLengthPercentageOrNormal;
use style::values::computed::{
    CSSPixelLength, ColumnCount, ColumnWidth, NonNegativeLengthPercentage,
};
use style::values::generics::length::LengthPercentageOrNormal;
use taffy::{
    AvailableSpace, BoxSizing, CacheTree, Display, LayoutInput, LayoutOutput, Line, MaybeMath,
    MaybeResolve, NodeId, Overflow, Position, Rect, RequestedAxis, ResolveOrZero, RunMode, Size,
    SizingMode, compute_block_layout,
};

use super::resolve_calc_value;
use crate::node::MulticolInfo;
use crate::{BaseDocument, NodeId as DomNodeId, dom_node_id};

/// Bisection steps when balancing: enough to bring any realistic content height
/// within a thousandth of a pixel of the least column height that fits.
const BALANCE_STEPS: usize = 32;

/// Slack when testing whether a child fits a column, for float error in the flow offsets.
const FIT_EPSILON: f32 = 0.01;

/// A `column-width` sized by the content (css-sizing-3 §4.1, column sizing).
#[derive(Clone)]
enum IntrinsicWidth {
    MinContent,
    MaxContent,
    FitContent(NonNegativeLengthPercentage),
}

/// The used column properties of a multi-column container.
pub(crate) struct ColumnSpec {
    count: Option<u32>,
    /// The column width, once any intrinsic `column-width` is resolved.
    width: Option<f32>,
    intrinsic_width: Option<IntrinsicWidth>,
    gap: NonNegativeLengthPercentageOrNormal,
    font_size: f32,
    is_balanced: bool,
}

impl ColumnSpec {
    /// The column properties of `style`, or `None` when it is not a multi-column container.
    pub(crate) fn of(style: &ComputedValues) -> Option<Self> {
        let column = style.get_column();
        if !column.is_multicol() {
            return None;
        }
        let (width, intrinsic_width) = match &column.column_width {
            ColumnWidth::Length(width) => (Some(width.0.px()), None),
            ColumnWidth::Auto => (None, None),
            ColumnWidth::MinContent => (None, Some(IntrinsicWidth::MinContent)),
            ColumnWidth::MaxContent => (None, Some(IntrinsicWidth::MaxContent)),
            ColumnWidth::FitContent(limit) => {
                (None, Some(IntrinsicWidth::FitContent(limit.clone())))
            }
        };
        Some(Self {
            count: match column.column_count {
                ColumnCount::Integer(count) => Some(count.0.max(1) as u32),
                ColumnCount::Auto => None,
            },
            width,
            intrinsic_width,
            gap: style.get_position().column_gap.clone(),
            font_size: style.get_font().font_size.computed_size().px(),
            is_balanced: column.column_fill == ColumnFill::Balance,
        })
    }

    /// The used `column-gap`: `normal` is 1em in multi-column layout.
    fn gap(&self, basis: f32) -> f32 {
        match &self.gap {
            LengthPercentageOrNormal::Normal => self.font_size,
            LengthPercentageOrNormal::LengthPercentage(gap) => {
                gap.0.resolve(CSSPixelLength::new(basis)).px()
            }
        }
    }

    /// The column count and width for a content box `available` wide
    /// (css-multicol-1 §3.4, the pseudo-algorithm).
    fn columns(&self, available: f32, gap: f32) -> (usize, f32) {
        let fitting = |width: f32| {
            ((available + gap) / (width.max(1.0) + gap))
                .floor()
                .max(1.0)
        };
        let count = match (self.count, self.width) {
            (Some(count), None) => count as f32,
            (None, Some(width)) => fitting(width),
            (Some(count), Some(width)) => fitting(width).min(count as f32),
            (None, None) => 1.0,
        };
        (count as usize, ((available + gap) / count - gap).max(0.0))
    }
}

/// An in-flow child's border box in the single-column flow, as offsets from the
/// container's border-box top.
#[derive(Clone, Copy)]
struct FlowItem {
    top: f32,
    bottom: f32,
}

/// The children one column holds, and the stretch of the flow they span.
struct FilledColumn {
    /// The flow offset the column starts at.
    start: f32,
    /// The bottom of the lowest child in it.
    end: f32,
    items: std::ops::Range<usize>,
}

/// Fills columns `height` tall in turn. A child that does not fit opens the next column
/// at its own border-box top, and a column always takes its first child.
fn fill_columns(items: &[FlowItem], flow_top: f32, height: f32) -> Vec<FilledColumn> {
    let mut columns: Vec<FilledColumn> = Vec::new();
    for (index, item) in items.iter().enumerate() {
        match columns.last_mut() {
            Some(column) if item.bottom - column.start <= height + FIT_EPSILON => {
                column.end = column.end.max(item.bottom);
                column.items.end = index + 1;
            }
            last => {
                let start = if last.is_some() { item.top } else { flow_top };
                columns.push(FilledColumn {
                    start,
                    end: item.bottom,
                    items: index..index + 1,
                });
            }
        }
    }
    columns
}

/// The tallest column's used height.
fn used_height(columns: &[FilledColumn]) -> f32 {
    columns
        .iter()
        .map(|column| column.end - column.start)
        .fold(0.0, f32::max)
}

/// The least column height at which the content fits in `count` columns.
fn balanced_height(items: &[FlowItem], flow_top: f32, count: usize) -> f32 {
    let Some(flow_bottom) = items.iter().map(|item| item.bottom).reduce(f32::max) else {
        return 0.0;
    };
    let mut fits = (flow_bottom - flow_top).max(0.0);
    let mut short = 0.0;
    for _ in 0..BALANCE_STEPS {
        let height = (short + fits) / 2.0;
        if fill_columns(items, flow_top, height).len() <= count {
            fits = height;
        } else {
            short = height;
        }
    }
    used_height(&fill_columns(items, flow_top, fits))
}

impl BaseDocument {
    /// The used width of an intrinsic `column-width`: the min- or max-content inline size
    /// of the container's content, or `fit-content()`'s length clamped between the two,
    /// a percentage resolving against `basis`, the content-box width.
    fn intrinsic_column_width(
        &mut self,
        node_id: NodeId,
        inputs: LayoutInput,
        intrinsic: &IntrinsicWidth,
        inset_width: f32,
        basis: f32,
    ) -> f32 {
        let mut content_size = |space: AvailableSpace| {
            let measure = LayoutInput {
                run_mode: RunMode::ComputeSize,
                sizing_mode: SizingMode::ContentSize,
                axis: RequestedAxis::Horizontal,
                known_dimensions: Size::NONE,
                available_space: Size {
                    width: space,
                    height: AvailableSpace::MaxContent,
                },
                ..inputs
            };
            let width = compute_block_layout(self, node_id, measure, None)
                .size
                .width;
            (width - inset_width).max(0.0)
        };
        match intrinsic {
            IntrinsicWidth::MinContent => content_size(AvailableSpace::MinContent),
            IntrinsicWidth::MaxContent => content_size(AvailableSpace::MaxContent),
            IntrinsicWidth::FitContent(limit) => {
                let min = content_size(AvailableSpace::MinContent);
                let max = content_size(AvailableSpace::MaxContent);
                limit
                    .0
                    .resolve(CSSPixelLength::new(basis))
                    .px()
                    .max(min)
                    .min(max)
            }
        }
    }

    pub(crate) fn compute_multicol_layout(
        &mut self,
        node_id: NodeId,
        inputs: LayoutInput,
        mut spec: ColumnSpec,
    ) -> LayoutOutput {
        // A multi-column container is an independent formatting context: its children's
        // margins stay inside it.
        let inputs = LayoutInput {
            vertical_margins_are_collapsible: Line::FALSE,
            ..inputs
        };
        let style = self.nodes[dom_node_id(node_id)].style();
        let border = style
            .border
            .resolve_or_zero(inputs.parent_size.width, resolve_calc_value);
        let inset = style
            .padding
            .resolve_or_zero(inputs.parent_size.width, resolve_calc_value)
            + border;
        let inset_size = inset.sum_axes();
        let box_sizing_adjustment = if style.box_sizing == BoxSizing::ContentBox {
            inset_size
        } else {
            Size::ZERO
        };
        let style_height = match inputs.sizing_mode {
            SizingMode::InherentSize => style
                .size
                .height
                .maybe_resolve(inputs.parent_size.height, resolve_calc_value),
            SizingMode::ContentSize => None,
        };
        let is_height_definite = inputs.known_dimensions.height.is_some() || style_height.is_some();
        let min_height = style
            .min_size
            .height
            .maybe_resolve(inputs.parent_size.height, resolve_calc_value)
            .maybe_add(box_sizing_adjustment.height);
        let max_height = style
            .max_size
            .height
            .maybe_resolve(inputs.parent_size.height, resolve_calc_value)
            .maybe_add(box_sizing_adjustment.height);
        let is_width_auto = style.size.width.is_auto();
        let intrinsic_width = spec.intrinsic_width.take();
        if let Some(intrinsic) = &intrinsic_width {
            // Sizing the container by its content leaves no basis for a percentage.
            spec.width = Some(self.intrinsic_column_width(
                node_id,
                inputs,
                intrinsic,
                inset_size.width,
                0.0,
            ));
        }

        let outer_width = match inputs.known_dimensions.width {
            Some(width) => width,
            None => {
                let measure = LayoutInput {
                    run_mode: RunMode::ComputeSize,
                    axis: RequestedAxis::Horizontal,
                    ..inputs
                };
                let width = compute_block_layout(self, node_id, measure, None)
                    .size
                    .width;
                if is_width_auto && !inputs.available_space.width.is_definite() {
                    // Sized by its content: each column holds the content's width.
                    let column = (width - inset_size.width).max(spec.width.unwrap_or(0.0));
                    let count = spec.count.unwrap_or(1) as f32;
                    count * column + (count - 1.0) * spec.gap(0.0) + inset_size.width
                } else {
                    width
                }
            }
        };
        if inputs.run_mode == RunMode::ComputeSize {
            if inputs.axis == RequestedAxis::Horizontal {
                return LayoutOutput::from_outer_size(Size {
                    width: outer_width,
                    height: 0.0,
                });
            }
            if let Some(height) = inputs.known_dimensions.height {
                return LayoutOutput::from_outer_size(Size {
                    width: outer_width,
                    height,
                });
            }
        }

        let content_width = (outer_width - inset_size.width).max(0.0);
        if let Some(fit @ IntrinsicWidth::FitContent(_)) = &intrinsic_width {
            spec.width = Some(self.intrinsic_column_width(
                node_id,
                inputs,
                fit,
                inset_size.width,
                content_width,
            ));
        }
        let column_gap = spec.gap(content_width);
        let (count, column_width) = spec.columns(content_width, column_gap);
        let flow_width = column_width + inset_size.width;
        let flow = compute_block_layout(
            self,
            node_id,
            LayoutInput {
                run_mode: RunMode::PerformLayout,
                axis: RequestedAxis::Both,
                known_dimensions: Size {
                    width: Some(flow_width),
                    height: inputs.known_dimensions.height,
                },
                available_space: Size {
                    width: AvailableSpace::Definite(flow_width),
                    height: inputs.available_space.height,
                },
                ..inputs
            },
            None,
        );

        let children: Vec<DomNodeId> = self.nodes[dom_node_id(node_id)]
            .layout_children
            .borrow()
            .as_deref()
            .map(<[DomNodeId]>::to_vec)
            .unwrap_or_default();
        let in_flow: Vec<DomNodeId> = children
            .iter()
            .copied()
            .filter(|&child| {
                let style = self.nodes[child].style();
                style.display != Display::None && style.position != Position::Absolute
            })
            .collect();
        let items: Vec<FlowItem> = in_flow
            .iter()
            .map(|&child| {
                let layout = self.nodes[child].unrounded_layout();
                FlowItem {
                    top: layout.location.y,
                    bottom: layout.location.y + layout.size.height,
                }
            })
            .collect();

        let flow_top = inset.top;
        let column_height = if is_height_definite {
            let available = (flow.size.height - inset_size.height).max(0.0);
            if spec.is_balanced {
                balanced_height(&items, flow_top, count).min(available)
            } else {
                available
            }
        } else {
            let balanced = balanced_height(&items, flow_top, count);
            max_height.map_or(balanced, |max| {
                balanced.min((max - inset_size.height).max(0.0))
            })
        };
        let outer_height = if is_height_definite {
            flow.size.height
        } else {
            (column_height + inset_size.height)
                .maybe_clamp(min_height, max_height)
                .max(inset_size.height)
        };

        if inputs.run_mode == RunMode::ComputeSize {
            // Measuring laid the children out in one column: drop this node's cached
            // layout so the next perform pass moves them into their columns again.
            self.cache_clear(node_id);
            return LayoutOutput::from_outer_size(Size {
                width: outer_width,
                height: outer_height,
            });
        }

        let columns = fill_columns(&items, flow_top, column_height);
        for (index, column) in columns.iter().enumerate() {
            let shift = taffy::Point {
                x: index as f32 * (column_width + column_gap),
                y: if index == 0 {
                    0.0
                } else {
                    column.start - flow_top
                },
            };
            for &child in &in_flow[column.items.clone()] {
                let location = &mut self.nodes[child].unrounded_layout_mut().location;
                location.x += shift.x;
                location.y -= shift.y;
            }
        }

        let mut overflow = Rect::ZERO;
        for &child in &children {
            let node = &self.nodes[child];
            let style = node.style();
            if style.display == Display::None {
                continue;
            }
            let layout = node.unrounded_layout();
            let extent = Size {
                width: if style.overflow.x == Overflow::Visible {
                    layout.size.width.max(layout.scrollable_overflow_rect.right)
                } else {
                    layout.size.width
                },
                height: if style.overflow.y == Overflow::Visible {
                    layout
                        .size
                        .height
                        .max(layout.scrollable_overflow_rect.bottom)
                } else {
                    layout.size.height
                },
            };
            // The overflow rect is measured from the padding box.
            let left = layout.location.x - border.left;
            let top = layout.location.y - border.top;
            overflow = overflow.union(Rect {
                left,
                right: left + extent.width,
                top,
                bottom: top + extent.height,
            });
        }

        if let Some(element) = self.nodes[dom_node_id(node_id)].element_data_mut() {
            element.multicol_info = Some(Box::new(MulticolInfo {
                origin: taffy::Point {
                    x: inset.left,
                    y: inset.top,
                },
                column_width,
                column_gap,
                column_height,
                used_columns: columns.len(),
            }));
        }

        LayoutOutput {
            size: Size {
                width: outer_width,
                height: outer_height,
            },
            scrollable_overflow_rect: overflow,
            baselines: flow.baselines,
            top_margin: flow.top_margin,
            bottom_margin: flow.bottom_margin,
            margins_can_collapse_through: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blocks(heights: &[f32]) -> Vec<FlowItem> {
        let mut top = 0.0;
        heights
            .iter()
            .map(|height| {
                let item = FlowItem {
                    top,
                    bottom: top + height,
                };
                top += height;
                item
            })
            .collect()
    }

    fn counts(columns: &[FilledColumn]) -> Vec<usize> {
        columns.iter().map(|column| column.items.len()).collect()
    }

    #[test]
    fn nine_equal_blocks_balance_three_per_column() {
        let items = blocks(&[20.0; 9]);
        let height = balanced_height(&items, 0.0, 3);
        assert_eq!(height, 60.0);
        assert_eq!(counts(&fill_columns(&items, 0.0, height)), [3, 3, 3]);
    }

    #[test]
    fn sequential_fill_overflows_into_further_columns() {
        let items = blocks(&[20.0; 9]);
        assert_eq!(counts(&fill_columns(&items, 0.0, 100.0)), [5, 4]);
        assert_eq!(counts(&fill_columns(&items, 0.0, 30.0)), [1; 9]);
    }

    #[test]
    fn a_child_taller_than_the_column_keeps_a_column_to_itself() {
        let items = blocks(&[50.0, 20.0, 20.0, 20.0]);
        let height = balanced_height(&items, 0.0, 2);
        assert_eq!(
            height, 60.0,
            "the 50px child leaves 60px for the three others"
        );
        assert_eq!(counts(&fill_columns(&items, 0.0, height)), [1, 3]);
    }

    #[test]
    fn a_break_truncates_the_margin_before_it() {
        // 10px margins between 20px blocks, with a 10px margin above the first.
        let items: Vec<FlowItem> = (0..4)
            .map(|index| FlowItem {
                top: 10.0 + index as f32 * 30.0,
                bottom: 30.0 + index as f32 * 30.0,
            })
            .collect();
        let columns = fill_columns(&items, 0.0, 60.0);
        assert_eq!(counts(&columns), [2, 2]);
        assert_eq!(
            columns[1].start, 70.0,
            "column 2 starts at the third block's border box"
        );
        assert_eq!(used_height(&columns), 60.0);
    }

    #[test]
    fn the_pseudo_algorithm_picks_count_and_width() {
        let spec = |count, width| ColumnSpec {
            count,
            width,
            intrinsic_width: None,
            gap: LengthPercentageOrNormal::Normal,
            font_size: 16.0,
            is_balanced: true,
        };
        assert_eq!(spec(Some(3), None).columns(600.0, 30.0), (3, 180.0));
        assert_eq!(spec(None, Some(100.0)).columns(350.0, 10.0), (3, 110.0));
        assert_eq!(spec(Some(2), Some(100.0)).columns(350.0, 10.0), (2, 170.0));
        assert_eq!(spec(None, Some(400.0)).columns(350.0, 10.0), (1, 350.0));
    }
}
