//! Glyphs an inline layout draws beside its text: the hyphen at the end of a line broken
//! at a soft hyphen (css-text-3 §5.4) and `text-emphasis` marks (css-text-decor-3 §3).

use anyrender::PaintScene;
use blitz_dom::node::{TextBrush, TextLayout};
use blitz_dom::util::ToColorColor as _;
use blitz_dom::{BaseDocument, NodeId};
use kurbo::Affine;
use parley::{Layout, PositionedLayoutItem};
use peniko::Fill;
use style::values::computed::TextEmphasisPosition;

use crate::color::{Color, ToColorColor as _};
use crate::text::{LineTransform, platform_embolden};

/// Draw the hyphen of each line of `text_layout` that breaks at a soft hyphen, after the
/// line's last glyph, in that run's font and its node's colour.
pub(crate) fn draw_line_hyphens(
    scene: &mut impl PaintScene,
    text_layout: &TextLayout,
    doc: &BaseDocument,
    line_transform: LineTransform<'_>,
    scale: f64,
) {
    for line in text_layout.layout.lines() {
        let transform = line_transform(&line);
        let Some(hyphen) = text_layout.line_hyphen(&line) else {
            continue;
        };
        let Some(glyph_run) = line.items().filter_map(glyph_run).last() else {
            continue;
        };
        let run = glyph_run.run();
        let color = node_color(doc, glyph_run.style().brush.id);
        scene.draw_glyphs(
            run.font(),
            run.font_size(),
            !crate::FONT_EMBOLDEN_ENABLED,
            run.normalized_coords(),
            platform_embolden(run.font_size(), scale),
            Fill::NonZero,
            &anyrender::Paint::from(color),
            1.0,
            transform,
            None,
            std::iter::once(anyrender::Glyph {
                id: hyphen.glyph_id,
                x: glyph_run.offset() + glyph_run.advance(),
                y: glyph_run.baseline(),
            }),
        );
    }
}

/// Draw each emphasis mark of `text_layout`, centred over (`over`) or under (`under`) its
/// character just outside the run's ascent or descent, as Blink places it. The mark is
/// coloured by `text-emphasis-color`.
pub(crate) fn draw_emphasis_marks(
    scene: &mut impl PaintScene,
    text_layout: &TextLayout,
    doc: &BaseDocument,
    line_transform: LineTransform<'_>,
    scale: f64,
) {
    if text_layout.emphasis_marks.is_empty() {
        return;
    }
    // The emphasis colour and side of the node the previous mark belonged to.
    let mut node_paint: Option<(NodeId, Color, bool)> = None;
    for line in text_layout.layout.lines() {
        let transform = line_transform(&line);
        let mut drawn_run = None;
        for glyph_run in line.items().filter_map(glyph_run) {
            let run = glyph_run.run();
            // Every glyph run of one run is walked at once, from the first's offset.
            if drawn_run == Some(run.index()) {
                continue;
            }
            drawn_run = Some(run.index());
            let metrics = run.metrics();
            let mut x = glyph_run.offset();
            for cluster in run.visual_clusters() {
                let advance = cluster.advance();
                if let Some(mark) = text_layout.emphasis_mark(&cluster) {
                    let node_id = cluster.first_style().brush.id;
                    let (color, is_under) = match node_paint {
                        Some((id, color, is_under)) if id == node_id => (color, is_under),
                        _ => {
                            let (color, is_under) = emphasis_paint(doc, node_id);
                            node_paint = Some((node_id, color, is_under));
                            (color, is_under)
                        }
                    };
                    let (mark_ascent, mark_descent) = first_line_extent(mark);
                    let baseline = if is_under {
                        glyph_run.baseline() + metrics.descent + mark_ascent
                    } else {
                        glyph_run.baseline() - metrics.ascent - mark_descent
                    };
                    let origin = (x + (advance - mark.full_width()) / 2.0, baseline);
                    draw_mark(scene, mark, color, transform, scale, origin);
                }
                x += advance;
            }
        }
    }
}

fn glyph_run<'a>(
    item: PositionedLayoutItem<'a, TextBrush>,
) -> Option<parley::GlyphRun<'a, TextBrush>> {
    match item {
        PositionedLayoutItem::GlyphRun(glyph_run) => Some(glyph_run),
        PositionedLayoutItem::InlineBox(_) => None,
    }
}

/// The ascent and descent of a mark layout's only line.
fn first_line_extent(mark: &Layout<TextBrush>) -> (f32, f32) {
    mark.lines().next().map_or((0.0, 0.0), |line| {
        (line.metrics().ascent, line.metrics().descent)
    })
}

/// Draw `mark` with its first baseline at `origin`.
fn draw_mark(
    scene: &mut impl PaintScene,
    mark: &Layout<TextBrush>,
    color: Color,
    transform: Affine,
    scale: f64,
    origin: (f32, f32),
) {
    for line in mark.lines() {
        let shift = Affine::translate((
            f64::from(origin.0),
            f64::from(origin.1 - line.metrics().baseline),
        ));
        for glyph_run in line.items().filter_map(glyph_run) {
            let run = glyph_run.run();
            scene.draw_glyphs(
                run.font(),
                run.font_size(),
                !crate::FONT_EMBOLDEN_ENABLED,
                run.normalized_coords(),
                platform_embolden(run.font_size(), scale),
                Fill::NonZero,
                &anyrender::Paint::from(color),
                1.0,
                transform * shift,
                None,
                glyph_run.positioned_glyphs().map(|glyph| anyrender::Glyph {
                    id: glyph.id as _,
                    x: glyph.x,
                    y: glyph.y,
                }),
            );
        }
    }
}

/// `node_id`'s text colour.
fn node_color(doc: &BaseDocument, node_id: NodeId) -> Color {
    doc.get_node(node_id)
        .and_then(|node| node.primary_styles())
        .map_or(Color::BLACK, |styles| {
            styles.get_inherited_text().color.as_color_color()
        })
}

/// `node_id`'s `text-emphasis-color` and whether `text-emphasis-position` puts its marks
/// under the text (`auto` and `over` both draw over horizontal text).
fn emphasis_paint(doc: &BaseDocument, node_id: NodeId) -> (Color, bool) {
    let Some(styles) = doc.get_node(node_id).and_then(|node| node.primary_styles()) else {
        return (Color::BLACK, false);
    };
    let text = styles.get_inherited_text();
    let color = text
        .text_emphasis_color
        .resolve_to_absolute(&styles.clone_color())
        .as_srgb_color();
    let is_under = text
        .text_emphasis_position
        .contains(TextEmphasisPosition::UNDER);
    (color, is_under)
}
