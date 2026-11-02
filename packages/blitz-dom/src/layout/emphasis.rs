//! The `text-emphasis-style` marks of an inline layout (css-text-decor-3 §3.1), each laid
//! out once in its node's font at half the node's font size. blitz-paint draws one over
//! (or under) every character that takes a mark ([`TextLayout::emphasis_mark`]).
//!
//! [`TextLayout::emphasis_mark`]: crate::node::TextLayout::emphasis_mark

use std::borrow::Cow;

use parley::{FontContext, LayoutContext};
use style::values::computed::text::TextEmphasisStyle;
use style::values::specified::text::{TextEmphasisFillMode, TextEmphasisShapeKeyword};

use crate::node::{TextBrush, TextLayout};
use crate::stylo_to_parley;

/// Lay out the mark of every node whose text `text_layout` holds and that sets a
/// `text-emphasis-style`, replacing the previous build's marks.
pub(crate) fn build_emphasis_marks(
    nodes: &crate::NodeTree,
    layout_ctx: &mut LayoutContext<TextBrush>,
    font_ctx: &mut FontContext,
    text_layout: &mut TextLayout,
    scale: f32,
) {
    text_layout.emphasis_marks.clear();
    for style in text_layout.layout.styles() {
        let node_id = style.brush.id;
        if text_layout.emphasis_marks.contains_key(&node_id) {
            continue;
        }
        let Some(styles) = nodes.get(node_id).and_then(|node| node.primary_styles()) else {
            continue;
        };
        let Some(mark) = mark_text(&styles.get_inherited_text().text_emphasis_style) else {
            continue;
        };
        let mut mark_style = stylo_to_parley::style(node_id, &styles);
        mark_style.font_size *= 0.5;
        let mut builder = layout_ctx.tree_builder(font_ctx, scale, true, &mark_style);
        builder.push_text(&mark);
        let mut layout = parley::Layout::new();
        builder.build_into(&mut layout);
        layout.break_all_lines(None);
        text_layout.emphasis_marks.insert(node_id, layout);
    }
}

/// The text a `text-emphasis-style` draws: the spec's character for each shape and fill,
/// or the first grapheme cluster of a string (computed value already trims it).
fn mark_text(style: &TextEmphasisStyle) -> Option<Cow<'_, str>> {
    use TextEmphasisFillMode::{Filled, Open};
    use TextEmphasisShapeKeyword::{Circle, Dot, DoubleCircle, Sesame, Triangle};
    let ch = match style {
        TextEmphasisStyle::None => return None,
        TextEmphasisStyle::String(text) => return Some(Cow::Borrowed(&**text)),
        TextEmphasisStyle::Keyword { fill, shape } => match (shape, fill) {
            (Dot, Filled) => '\u{2022}',
            (Dot, Open) => '\u{25E6}',
            (Circle, Filled) => '\u{25CF}',
            (Circle, Open) => '\u{25CB}',
            (DoubleCircle, Filled) => '\u{25C9}',
            (DoubleCircle, Open) => '\u{25CE}',
            (Triangle, Filled) => '\u{25B2}',
            (Triangle, Open) => '\u{25B3}',
            (Sesame, Filled) => '\u{FE45}',
            (Sesame, Open) => '\u{FE46}',
        },
    };
    Some(Cow::Owned(ch.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_marks_are_the_spec_characters() {
        // css-text-decor-3 §3.1, the text-emphasis-style shape table.
        let mark = |fill, shape| {
            mark_text(&TextEmphasisStyle::Keyword { fill, shape }).map(Cow::into_owned)
        };
        use TextEmphasisFillMode::{Filled, Open};
        use TextEmphasisShapeKeyword::{Circle, Dot, DoubleCircle, Sesame, Triangle};
        assert_eq!(mark(Filled, Dot).as_deref(), Some("•"));
        assert_eq!(mark(Open, Dot).as_deref(), Some("◦"));
        assert_eq!(mark(Filled, Circle).as_deref(), Some("●"));
        assert_eq!(mark(Open, Circle).as_deref(), Some("○"));
        assert_eq!(mark(Filled, DoubleCircle).as_deref(), Some("◉"));
        assert_eq!(mark(Open, DoubleCircle).as_deref(), Some("◎"));
        assert_eq!(mark(Filled, Triangle).as_deref(), Some("▲"));
        assert_eq!(mark(Open, Triangle).as_deref(), Some("△"));
        assert_eq!(mark(Filled, Sesame).as_deref(), Some("﹅"));
        assert_eq!(mark(Open, Sesame).as_deref(), Some("﹆"));
        assert_eq!(mark_text(&TextEmphasisStyle::None), None);
    }
}
