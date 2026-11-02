//! Ruby layout (css-ruby-1), in a minimal model: a ruby container's children pair into
//! columns, each a base (an `rb`, or an anonymous block around the base text and inlines)
//! and at most one annotation (`rt`/`rtc`) after it. A column is as wide as the wider of
//! the two and both boxes take that width, `ruby-align` placing their content within it.
//! Annotations sit on the base row's top edge, or on its bottom edge under
//! `ruby-position: under`.
//!
//! An inline ruby container's box is its base row, set on the line's baseline as the
//! base's, and its annotations overflow it: the inline layout leaves room for them above
//! and below the line box they reach past. A `block ruby` box holds both rows.
//! Not modelled: inter-character annotations, annotation overhang, justification inside a
//! column, and line breaks inside a ruby container.

use parley::layout::{Alignment, LineMetrics, PositionedLayoutItem};
use style::computed_values::ruby_align::T as RubyAlign;
use style::values::computed::{Display, RubyPosition};
use style::values::specified::box_::{DisplayInside, DisplayOutside};
use taffy::{
    AvailableSpace, Baselines, CollapsibleMarginSet, Layout, LayoutInput, LayoutOutput,
    LayoutPartialTree as _, Line, Point, Rect, RequestedAxis, ResolveOrZero as _, RunMode, Size,
    SizingMode,
};

use super::construct::LayoutChildren;
use super::resolve_calc_value;
use crate::node::{NodeKind, TextBrush};
use crate::{BaseDocument, Node, NodeId as DomNodeId, dom_node_id, taffy_node_id};

/// Below this many layout units, two measures of line room are the same.
const ROOM_EPSILON: f32 = 0.01;

/// Times an inline layout is broken again for the room its ruby annotations ask for. A
/// second pass settles unless the room moved a line past a float, changing its contents.
pub(crate) const ROOM_PASSES: usize = 2;

fn is_annotation(display: Display) -> bool {
    matches!(
        display.inside(),
        DisplayInside::RubyText | DisplayInside::RubyTextContainer
    )
}

impl Node {
    /// Whether this box lays its children out as ruby columns (`display: ruby` or `block ruby`).
    pub(crate) fn is_ruby_container(&self) -> bool {
        self.display_style()
            .is_some_and(|display| display.inside() == DisplayInside::Ruby)
    }

    fn is_annotation_under(&self) -> bool {
        self.primary_styles().is_some_and(|style| {
            matches!(
                style.get_inherited_text().clone_ruby_position(),
                RubyPosition::Under | RubyPosition::AlternateUnder
            )
        })
    }
}

/// A ruby container's layout children: each `rb`, `rt` and `rtc` as itself, and each run of
/// other content (the base text and its inlines) wrapped in an anonymous block. Hidden
/// children (`rp`) and whitespace between the boxes generate nothing.
pub(crate) fn collect_ruby_children(
    doc: &mut BaseDocument,
    container_id: DomNodeId,
    out: &mut LayoutChildren,
) {
    let node = &doc.nodes[container_id];
    let children: Vec<DomNodeId> = node
        .before()
        .into_iter()
        .chain(node.flat_children().iter().copied())
        .chain(node.after())
        .collect();
    for child_id in children {
        let child = &doc.nodes[child_id];
        if child.data.kind() == NodeKind::Comment || child.is_whitespace_node() {
            continue;
        }
        match child.display_style() {
            Some(display) if display.is_none() => {}
            Some(display)
                if is_annotation(display) || display.inside() == DisplayInside::RubyBase =>
            {
                out.push(child_id, doc)
            }
            _ => out.push_wrapped(container_id, child_id, doc),
        }
    }
    out.maybe_push_anon_block(doc);
}

/// A base and the annotation paired with it.
struct Column {
    base: Option<DomNodeId>,
    annotation: Option<DomNodeId>,
}

/// Pairs each annotation with the base before it; an annotation with no base of its own
/// gets an empty one.
fn pair_columns(doc: &BaseDocument, children: &[DomNodeId]) -> Vec<Column> {
    let mut columns: Vec<Column> = Vec::new();
    for &child in children {
        let is_annotation = doc.nodes[child].display_style().is_some_and(is_annotation);
        match columns.last_mut() {
            Some(column) if is_annotation && column.annotation.is_none() => {
                column.annotation = Some(child);
            }
            _ => columns.push(Column {
                base: (!is_annotation).then_some(child),
                annotation: is_annotation.then_some(child),
            }),
        }
    }
    columns
}

#[derive(Clone, Copy)]
enum Row {
    Over,
    Base,
    Under,
}

/// A laid-out base or annotation box, before it is placed.
struct RubyBox {
    id: DomNodeId,
    row: Row,
    x: f32,
    output: LayoutOutput,
}

/// How an inline ruby container sits in its line, from its layout output: its baseline
/// (the first base's), and how far its annotations reach above and below that baseline.
pub(crate) struct LineExtent {
    /// From the top of the border box to the baseline.
    pub(crate) baseline: f32,
    pub(crate) above: f32,
    pub(crate) below: f32,
}

impl LineExtent {
    /// Reads the annotations' reach from the scrollable overflow, which
    /// `compute_ruby_layout` sets to the union of the box and its annotations.
    pub(crate) fn of(output: &LayoutOutput) -> Self {
        let baseline = output.baselines.first.unwrap_or(output.size.height);
        let overflow = output.scrollable_overflow_rect;
        Self {
            baseline,
            above: baseline - overflow.top.min(0.0),
            below: overflow.bottom.max(output.size.height) - baseline,
        }
    }
}

/// How far a ruby inline box reaches past the space it reserves in its line (from its
/// base's top to its baseline), in the inline layout's units.
pub(crate) struct LineReach {
    pub(crate) id: u64,
    pub(crate) above: f32,
    pub(crate) below: f32,
}

/// The top and bottom of the space a line is stacked in. Its `block_min_coord` and
/// `block_max_coord` reach past that space when the leading is negative (fonts mixed on
/// one line), so this mirrors parley 0.11's quantized placement
/// (`BreakLines::finish_line`), blitz building every inline layout quantized.
fn line_box(metrics: &LineMetrics) -> (f32, f32) {
    let (ascent, descent) = (metrics.ascent.round(), metrics.descent.round());
    let leading_above = ((metrics.line_height - (ascent + descent)) * 0.5).floor();
    let top = metrics.baseline - ascent - leading_above;
    (top, top + metrics.line_height)
}

/// The room each line of an inline layout needs above and below its line box for the ruby
/// annotations in it, in the layout's units: the lines are broken again with that much
/// space left around each.
#[derive(Default)]
pub(crate) struct LineRoom {
    above: Vec<f32>,
    below: Vec<f32>,
}

impl LineRoom {
    pub(crate) fn measure(layout: &parley::Layout<TextBrush>, reaches: &[LineReach]) -> Self {
        let mut room = Self::default();
        for line in layout.lines() {
            let (top, bottom) = line_box(line.metrics());
            let (mut above, mut below) = (0.0f32, 0.0f32);
            for item in line.items() {
                let PositionedLayoutItem::InlineBox(ibox) = item else {
                    continue;
                };
                if let Some(reach) = reaches.iter().find(|reach| reach.id == ibox.id) {
                    above = above.max(top - (ibox.y - reach.above));
                    below = below.max(ibox.y + ibox.height + reach.below - bottom);
                }
            }
            room.above.push(above);
            room.below.push(below);
        }
        room
    }

    /// The space to leave before `line`: the room below the line before it and above it.
    pub(crate) fn before(&self, line: usize) -> f64 {
        let below_previous = line
            .checked_sub(1)
            .and_then(|previous| self.below.get(previous));
        f64::from(
            below_previous.copied().unwrap_or(0.0) + self.above.get(line).copied().unwrap_or(0.0),
        )
    }

    /// All the space left between and around the lines.
    pub(crate) fn total(&self) -> f32 {
        self.above.iter().chain(&self.below).sum()
    }

    /// Whether lines broken with `applied` room have the room this measure asks for.
    pub(crate) fn is_settled(&self, applied: &Self) -> bool {
        let same = |wanted: &[f32], given: &[f32]| {
            wanted.iter().enumerate().all(|(line, room)| {
                (room - given.get(line).copied().unwrap_or(0.0)).abs() < ROOM_EPSILON
            })
        };
        same(&self.above, &applied.above) && same(&self.below, &applied.below)
    }
}

impl BaseDocument {
    /// The alignment `ruby-align` gives the content of a ruby base or annotation box, or
    /// `None` for a box outside a ruby container.
    pub(crate) fn ruby_content_alignment(&self, node_id: DomNodeId) -> Option<Alignment> {
        let parent = self.nodes[node_id].layout_parent.get()?;
        if !self.nodes[parent].is_ruby_container() {
            return None;
        }
        let style = self.nodes[node_id].primary_styles()?;
        Some(match style.get_inherited_text().clone_ruby_align() {
            RubyAlign::Start => Alignment::Start,
            // Justification opportunities are not modelled, and content without any is
            // centred (css-ruby-1 §4.1).
            RubyAlign::Center | RubyAlign::SpaceBetween | RubyAlign::SpaceAround => {
                Alignment::Center
            }
        })
    }

    pub(crate) fn compute_ruby_layout(
        &mut self,
        node_id: taffy::NodeId,
        inputs: LayoutInput,
    ) -> LayoutOutput {
        let id = dom_node_id(node_id);
        let is_block = self.nodes[id]
            .display_style()
            .is_some_and(|display| display.outside() == DisplayOutside::Block);
        let style = self.nodes[id].style();
        let inset = style
            .padding
            .resolve_or_zero(inputs.parent_size.width, resolve_calc_value)
            + style
                .border
                .resolve_or_zero(inputs.parent_size.width, resolve_calc_value);
        let children: Vec<DomNodeId> = self.nodes[id]
            .layout_children
            .borrow()
            .iter()
            .flatten()
            .copied()
            .collect();
        let columns = pair_columns(self, &children);

        let measure = LayoutInput {
            run_mode: RunMode::ComputeSize,
            sizing_mode: SizingMode::InherentSize,
            axis: RequestedAxis::Horizontal,
            known_dimensions: Size::NONE,
            known_dimensions_are_definite: Size {
                width: false,
                height: false,
            },
            parent_size: Size::NONE,
            available_space: Size {
                width: AvailableSpace::MaxContent,
                height: AvailableSpace::MaxContent,
            },
            vertical_margins_are_collapsible: Line::FALSE,
        };
        let width_of = |doc: &mut Self, child: Option<DomNodeId>| {
            child.map_or(0.0, |child| {
                doc.compute_child_layout(taffy_node_id(child), measure)
                    .size
                    .width
            })
        };

        let mut boxes: Vec<RubyBox> = Vec::new();
        let mut x = 0.0;
        for column in &columns {
            let width = width_of(self, column.base).max(width_of(self, column.annotation));
            let at_width = LayoutInput {
                run_mode: RunMode::PerformLayout,
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
                ..measure
            };
            let base = column.base.map(|base| (base, Row::Base));
            let annotation = column.annotation.map(|annotation| {
                let row = if self.nodes[annotation].is_annotation_under() {
                    Row::Under
                } else {
                    Row::Over
                };
                (annotation, row)
            });
            for (child, row) in base.into_iter().chain(annotation) {
                let output = self.compute_child_layout(taffy_node_id(child), at_width);
                boxes.push(RubyBox {
                    id: child,
                    row,
                    x,
                    output,
                });
            }
            x += width;
        }
        let content_width = x;

        let row_height = |row: fn(Row) -> bool| {
            boxes
                .iter()
                .filter(|ruby_box| row(ruby_box.row))
                .map(|ruby_box| ruby_box.output.size.height)
                .fold(0.0, f32::max)
        };
        let over = row_height(|row| matches!(row, Row::Over));
        let base_height = row_height(|row| matches!(row, Row::Base));
        let under = row_height(|row| matches!(row, Row::Under));
        let base_baseline = boxes
            .iter()
            .filter(|ruby_box| matches!(ruby_box.row, Row::Base))
            .find_map(|ruby_box| ruby_box.output.baselines.first)
            .unwrap_or(base_height);

        // A block ruby box holds its annotations; an inline one's overflow it.
        let base_top = inset.top + if is_block { over } else { 0.0 };
        let content_height = base_height + if is_block { over + under } else { 0.0 };
        let size = Size {
            width: inputs
                .known_dimensions
                .width
                .unwrap_or(content_width + inset.horizontal_axis_sum()),
            height: inputs
                .known_dimensions
                .height
                .unwrap_or(content_height + inset.vertical_axis_sum()),
        };

        if inputs.run_mode == RunMode::PerformLayout {
            for (order, ruby_box) in boxes.iter().enumerate() {
                let height = ruby_box.output.size.height;
                let y = match ruby_box.row {
                    Row::Over => base_top - height,
                    Row::Base => base_top,
                    Row::Under => base_top + base_height,
                };
                let child_style = self.nodes[ruby_box.id].style_in_parent_frame();
                let padding = child_style
                    .padding
                    .resolve_or_zero(Some(content_width), resolve_calc_value);
                let border = child_style
                    .border
                    .resolve_or_zero(Some(content_width), resolve_calc_value);
                self.set_unrounded_layout(
                    taffy_node_id(ruby_box.id),
                    &Layout {
                        order: order as u32,
                        location: Point {
                            x: inset.left + ruby_box.x,
                            y,
                        },
                        size: ruby_box.output.size,
                        padding,
                        border,
                        ..Layout::new()
                    },
                );
            }
        }

        LayoutOutput {
            size,
            scrollable_overflow_rect: Rect {
                left: 0.0,
                right: size.width.max(inset.left + content_width),
                top: (base_top - over).min(0.0),
                bottom: size.height.max(base_top + base_height + under),
            },
            baselines: Baselines::from_first(Some(base_top + base_baseline)),
            top_margin: CollapsibleMarginSet::ZERO,
            bottom_margin: CollapsibleMarginSet::ZERO,
            margins_can_collapse_through: false,
        }
    }
}
