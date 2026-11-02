//! `::first-line` (css-pseudo-4 §3.1): the first formatted line of a block container is
//! styled as if a fictional inline element wrapped its content. stylo keeps only the
//! properties that apply to it (`APPLIES_TO_FIRST_LINE`).
//!
//! What fits on the first line depends on the first-line style itself, so an inline
//! context whose element has one is rebuilt at layout time: first with all its text
//! restyled, to find what line 1 holds when restyled, then with only that text
//! restyled, line 1 capped at the advance it had so that text after it, now in its own
//! style, cannot move up onto it. Layout applies the font, spacing and line-height
//! values here; paint applies `color`, the background and `text-decoration` to the runs
//! of line 1.
//!
//! The restyling is one span per level of the inline tree (the root and each inline
//! element), opened as the level opens and closed where line 1's text ends in it. parley
//! trims the whitespace at a span's edges, so the span wraps all of the level's text up
//! to that point, whitespace included, as its element's own span does.

use blitz_traits::node_id::NodeId;
use parley::{StyleProperty, TextStyle, TreeBuilder, YieldData};
use style::properties::ComputedValues;

use super::construct::build_inline_layout_into;
use crate::node::{TextBrush, TextLayout};
use crate::{BaseDocument, stylo_to_parley};

/// Room past line 1's restyled content, in layout units: the content shapes alike in
/// both builds, up to kerning across the restyled edge, and no following glyph fits.
const LINE_CAP_SLACK: f32 = 1.0;

/// How much of an inline context's text the `::first-line` style restyles in a build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FirstLineExtent {
    All,
    /// The text up to its `n`th non-whitespace character. Whitespace collapsing changes
    /// only whitespace, so the count means the same in the source and the built text.
    Chars(usize),
}

/// The `::first-line` restyling of one inline context build.
pub(crate) struct FirstLineBuild {
    root: TextStyle<'static, 'static, TextBrush>,
    first_line: TextStyle<'static, 'static, TextBrush>,
    /// Non-whitespace characters still to restyle; `None` restyles all of them.
    remaining: Option<usize>,
    /// Per open level, innermost last: whether its first-line span is open.
    levels: Vec<bool>,
}

impl FirstLineBuild {
    pub(crate) fn new(
        root_id: NodeId,
        root: &ComputedValues,
        first_line: &ComputedValues,
        extent: FirstLineExtent,
    ) -> Self {
        Self {
            root: stylo_to_parley::style(root_id, root),
            first_line: stylo_to_parley::style(root_id, first_line),
            remaining: match extent {
                FirstLineExtent::All => None,
                FirstLineExtent::Chars(count) => Some(count),
            },
            levels: Vec::new(),
        }
    }

    /// Open a level styled `own`, the root or an inline element whose span was just
    /// pushed: while line 1 is unfinished, in a span of its first-line values.
    pub(crate) fn enter(
        &mut self,
        builder: &mut TreeBuilder<'_, TextBrush>,
        own: &TextStyle<'static, 'static, TextBrush>,
    ) {
        let is_open = self.remaining != Some(0);
        if is_open {
            builder.push_style_modification_span(&self.overrides(own));
        }
        self.levels.push(is_open);
    }

    /// Close the innermost level, before its element's own span is popped.
    pub(crate) fn leave(&mut self, builder: &mut TreeBuilder<'_, TextBrush>) {
        if self.levels.pop() == Some(true) {
            builder.pop_style_span();
        }
    }

    /// Push `text` of the innermost level through `push`, closing the level's span where
    /// line 1's text ends.
    pub(crate) fn push_text(
        &mut self,
        builder: &mut TreeBuilder<'_, TextBrush>,
        text: &str,
        mut push: impl FnMut(&mut TreeBuilder<'_, TextBrush>, &str),
    ) {
        let end = self.take(text);
        if end > 0 {
            push(builder, &text[..end]);
        }
        // The span's pending text ends in line 1's last non-whitespace character, or an
        // inner span's pop committed it, so the pop trims none of it.
        if self.remaining == Some(0)
            && let Some(is_open @ true) = self.levels.last_mut()
        {
            builder.pop_style_span();
            *is_open = false;
        }
        if end < text.len() {
            push(builder, &text[end..]);
        }
    }

    /// Count `text` against line 1 without restyling it, as for a list marker.
    pub(crate) fn skip(&mut self, text: &str) {
        self.take(text);
    }

    /// The byte length of the part of `text` that is restyled, taken from the count.
    fn take(&mut self, text: &str) -> usize {
        let Some(remaining) = &mut self.remaining else {
            return text.len();
        };
        let mut end = 0;
        for (index, ch) in text.char_indices() {
            if *remaining == 0 {
                break;
            }
            end = index + ch.len_utf8();
            if !ch.is_whitespace() {
                *remaining -= 1;
            }
        }
        end
    }

    /// The first-line value of each property the first line changes and `own` inherits
    /// from the block unchanged: an element's own value wins over the fictional
    /// first-line element's, as an inherited one would not.
    fn overrides(
        &self,
        own: &TextStyle<'static, 'static, TextBrush>,
    ) -> Vec<StyleProperty<'static, TextBrush>> {
        let (root, first) = (&self.root, &self.first_line);
        let mut overrides = Vec::new();
        let mut set = |is_inherited: bool, is_changed: bool, property| {
            if is_inherited && is_changed {
                overrides.push(property);
            }
        };
        set(
            own.font_family == root.font_family,
            first.font_family != root.font_family,
            StyleProperty::FontFamily(first.font_family.clone()),
        );
        set(
            own.font_size == root.font_size,
            first.font_size != root.font_size,
            StyleProperty::FontSize(first.font_size),
        );
        set(
            own.font_width == root.font_width,
            first.font_width != root.font_width,
            StyleProperty::FontWidth(first.font_width),
        );
        set(
            own.font_style == root.font_style,
            first.font_style != root.font_style,
            StyleProperty::FontStyle(first.font_style),
        );
        set(
            own.font_weight == root.font_weight,
            first.font_weight != root.font_weight,
            StyleProperty::FontWeight(first.font_weight),
        );
        set(
            own.font_variations == root.font_variations,
            first.font_variations != root.font_variations,
            StyleProperty::FontVariations(first.font_variations.clone()),
        );
        set(
            own.font_features == root.font_features,
            first.font_features != root.font_features,
            StyleProperty::FontFeatures(first.font_features.clone()),
        );
        set(
            own.line_height == root.line_height,
            first.line_height != root.line_height,
            StyleProperty::LineHeight(first.line_height),
        );
        set(
            own.word_spacing == root.word_spacing,
            first.word_spacing != root.word_spacing,
            StyleProperty::WordSpacing(first.word_spacing),
        );
        set(
            own.letter_spacing == root.letter_spacing,
            first.letter_spacing != root.letter_spacing,
            StyleProperty::LetterSpacing(first.letter_spacing),
        );
        overrides
    }
}

impl BaseDocument {
    /// Rebuild the inline layout of `node_id` with its `::first-line` style over
    /// `extent`, keeping the sizes its inline boxes were given.
    pub(crate) fn build_first_line(
        &mut self,
        node_id: NodeId,
        inline_layout: &mut TextLayout,
        extent: FirstLineExtent,
    ) {
        let box_sizes: Vec<(f32, f32)> = inline_layout
            .layout
            .inline_boxes()
            .iter()
            .map(|ibox| (ibox.width, ibox.height))
            .collect();
        let mut font_ctx = self.font_ctx.lock().unwrap();
        build_inline_layout_into(
            &self.nodes,
            &mut self.layout_ctx,
            &mut font_ctx,
            inline_layout,
            self.viewport.scale(),
            node_id,
            Some(extent),
        );
        inline_layout.content_widths = None;
        // The rebuild pushes the same boxes in the same order.
        for (ibox, (width, height)) in inline_layout
            .layout
            .inline_boxes_mut()
            .iter_mut()
            .zip(box_sizes)
        {
            ibox.width = width;
            ibox.height = height;
        }
    }

    /// Break line 1 of `inline_layout`, built with all its text restyled, at
    /// `line_max_advance`, then rebuild it with only that line's text restyled. Returns
    /// the advance to cap line 1 at, or `None` when everything fits on line 1.
    /// `line_indent` is the `text-indent` of line 1.
    pub(crate) fn split_first_line(
        &mut self,
        node_id: NodeId,
        inline_layout: &mut TextLayout,
        layout_max_advance: f32,
        line_max_advance: f32,
        line_indent: f32,
    ) -> Option<f32> {
        let mut breaker = inline_layout.layout.break_lines();
        breaker
            .state_mut()
            .set_layout_max_advance(layout_max_advance);
        breaker.state_mut().set_line_max_advance(line_max_advance);
        while let Some(yield_data) = breaker.break_next() {
            match yield_data {
                YieldData::LineBreak(_) => break,
                // As in the floats layout loop, a float takes its advance on the line.
                #[cfg(feature = "floats")]
                YieldData::InlineBoxBreak(data) => breaker
                    .state_mut()
                    .append_inline_box_to_line(data.advance, 0.0),
                _ => {}
            }
        }
        let is_one_line = breaker.is_done();
        breaker.finish();
        if is_one_line {
            return None;
        }

        let line = inline_layout.layout.lines().next()?;
        let metrics = line.metrics();
        let cap = metrics.advance - metrics.trailing_whitespace + line_indent + LINE_CAP_SLACK;
        let chars = inline_layout.text[..line.text_range().end]
            .chars()
            .filter(|ch| !ch.is_whitespace())
            .count();
        self.build_first_line(node_id, inline_layout, FirstLineExtent::Chars(chars));
        Some(cap)
    }
}
