//! Text of vertical lines (css-writing-modes-4). An inline root in a vertical flow is
//! laid out as horizontal lines in its logical frame; paint turns each line a quarter
//! turn onto the physical block axis, then turns the glyphs `text-orientation` sets
//! upright back about their em cells. A `text-combine-upright` cell's single horizontal
//! line is compressed into its one-em box.

use std::f64::consts::FRAC_PI_2;

use blitz_dom::node::{LayoutFlow, TextBrush, is_upright};
use kurbo::{Affine, Point, Size};
use parley::{GlyphRun, Layout, Line};
use style::computed_values::text_orientation::T as TextOrientation;

/// The map from an inline root's logical text, in device pixels from its content box, to
/// its physical content box `content` (device pixels), for `line`. `None` for a flow that
/// lays text out horizontally.
pub(crate) fn line_transform(
    flow: LayoutFlow,
    content: Size,
    line: &Line<'_, TextBrush>,
) -> Option<Affine> {
    match flow {
        LayoutFlow::HorizontalTb | LayoutFlow::CombinedText => None,
        LayoutFlow::VerticalRl | LayoutFlow::SidewaysRl => {
            Some(Affine::translate((content.width, 0.0)) * Affine::rotate(FRAC_PI_2))
        }
        // Lines stack left to right but their glyphs still turn clockwise, so each line's
        // over side faces right (css-writing-modes-4 §4.4): it mirrors within its own extent.
        LayoutFlow::VerticalLr => {
            let metrics = line.metrics();
            let mirror = (metrics.block_min_coord + metrics.block_max_coord) as f64;
            Some(Affine::translate((mirror, 0.0)) * Affine::rotate(FRAC_PI_2))
        }
        LayoutFlow::SidewaysLr => {
            Some(Affine::translate((0.0, content.height)) * Affine::rotate(-FRAC_PI_2))
        }
    }
}

/// The map that fits a `text-combine-upright` cell's one horizontal line into its content
/// box `content` (device pixels): compressed to the box's width when wider, centred.
pub(crate) fn combined_text_transform(layout: &Layout<TextBrush>, content: Size) -> Affine {
    let natural = Size::new(layout.width() as f64, layout.height() as f64);
    let scale_x = if natural.width > content.width {
        content.width / natural.width
    } else {
        1.0
    };
    Affine::translate((
        (content.width - natural.width * scale_x) / 2.0,
        (content.height - natural.height) / 2.0,
    )) * Affine::scale_non_uniform(scale_x, 1.0)
}

/// For each glyph of `glyph_run` in a line of `flow`, the centre of its em cell when it
/// stands upright, in the line's logical coordinates; `None` for a glyph set sideways.
/// `glyph_start` is the number of the run's glyphs that earlier glyph runs of the line
/// drew (parley splits a run at each style change).
pub(crate) fn upright_cell_centres(
    glyph_run: &GlyphRun<'_, TextBrush>,
    glyph_start: usize,
    flow: LayoutFlow,
    orientation: TextOrientation,
) -> Vec<Option<Point>> {
    let run = glyph_run.run();
    let metrics = run.metrics();
    let centre_y = glyph_run.baseline() as f64 + (metrics.descent - metrics.ascent) as f64 / 2.0;
    let mut x = glyph_run.offset() as f64;
    run.visual_clusters()
        .flat_map(|cluster| {
            let is_upright = is_upright(flow, orientation, cluster.source_char());
            cluster
                .glyphs()
                .map(move |glyph| (glyph.advance, is_upright))
        })
        .skip(glyph_start)
        .take(glyph_run.glyphs().count())
        .map(|(advance, is_upright)| {
            let centre = Point::new(x + advance as f64 / 2.0, centre_y);
            x += advance as f64;
            is_upright.then_some(centre)
        })
        .collect()
}

/// The map that turns a glyph drawn by `line` back upright about its cell `centre`.
pub(crate) fn upright_glyph_transform(line: Affine, centre: Point) -> Affine {
    line * Affine::rotate_about(-FRAC_PI_2, centre)
}
