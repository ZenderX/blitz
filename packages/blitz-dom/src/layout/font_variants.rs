//! Text that `font-variant-caps` and `font-variant-emoji` rewrite before shaping.
//!
//! Caps the primary font has no OpenType features for fall back or are synthesized
//! (css-fonts-4 §6.8), following Chromium's `OpenTypeCapsSupport`: a run splits into
//! letters that need uppercasing and the rest, each case-mapped and shaped with the
//! caps features the font can take, at 0.7× the font size where synthesized.
//! `font-variant-emoji` (css-fonts-4 §6.12) appends the variation selector of the
//! requested presentation to each emoji that has no selector of its own.

use std::borrow::Cow;
use std::collections::HashMap;

use blitz_traits::node_id::NodeId;
use icu_properties::props::{Emoji, EmojiComponent, EmojiPresentation, Script};
use icu_properties::{CodePointMapData, CodePointSetData, PropertyNamesShort};
use markup5ever::local_name;
use parley::fontique::{Attributes, FallbackKey};
use parley::{FontContext, FontFeatures, StyleProperty, TreeBuilder};
use skrifa::raw::TableProvider as _;
use style::computed_values::font_variant_caps::T as FontVariantCaps;
use style::computed_values::font_variant_emoji::T as FontVariantEmoji;
use style::properties::style_structs::Font;
use style::values::computed::Display;
use style::values::computed::font::FontSynthesis;
use style::values::specified::box_::{DisplayInside, DisplayOutside};

use super::replaced::is_replaced_element;
use crate::font_metrics::find_font_for;
use crate::node::TextBrush;
use crate::{Node, NodeData, stylo_to_parley};

/// Chromium's synthetic small-caps size, relative to the font size.
const SMALL_CAPS_SCALE: f32 = 0.7;
const TEXT_PRESENTATION: char = '\u{FE0E}';
const EMOJI_PRESENTATION: char = '\u{FE0F}';

/// How much of the requested caps the primary font's GSUB table carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FontSupport {
    Full,
    /// No petite-caps or unicase features, but small caps.
    Fallback,
    None,
}

/// Which letters synthesized caps shrink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CapsSynthesis {
    LowerToSmallCaps,
    UpperToSmallCaps,
    BothToSmallCaps,
    None,
}

/// A run of letters that uppercasing changes, or of everything else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaseRun {
    SameCase,
    UppercaseNeeded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaseChange {
    Keep,
    Upper,
    Lower,
}

/// How one text node's requested caps render with its primary font.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CapsPlan {
    requested: FontVariantCaps,
    support: FontSupport,
    may_synthesize: bool,
}

impl CapsPlan {
    fn new(
        requested: FontVariantCaps,
        has: impl Fn(&[u8; 4]) -> bool,
        may_synthesize: bool,
    ) -> Self {
        use FontVariantCaps as C;
        let support = |full: bool, fallback: bool| match (full, fallback) {
            (true, _) => FontSupport::Full,
            (false, true) => FontSupport::Fallback,
            (false, false) => FontSupport::None,
        };
        let support = match requested {
            C::Normal => FontSupport::Full,
            C::SmallCaps => support(has(b"smcp"), false),
            C::AllSmallCaps => support(has(b"smcp") && has(b"c2sc"), false),
            C::PetiteCaps => support(has(b"pcap"), has(b"smcp")),
            C::AllPetiteCaps => support(has(b"pcap") && has(b"c2pc"), has(b"smcp") && has(b"c2sc")),
            C::Unicase => support(has(b"unic"), has(b"smcp")),
            C::TitlingCaps => support(has(b"titl"), false),
        };
        Self {
            requested,
            support,
            may_synthesize,
        }
    }

    fn synthesis(self) -> CapsSynthesis {
        use FontVariantCaps as C;
        match self.requested {
            C::SmallCaps | C::PetiteCaps => CapsSynthesis::LowerToSmallCaps,
            C::AllSmallCaps | C::AllPetiteCaps => CapsSynthesis::BothToSmallCaps,
            C::Unicase => CapsSynthesis::UpperToSmallCaps,
            C::Normal | C::TitlingCaps => CapsSynthesis::None,
        }
    }

    /// Missing titling caps are never synthesized.
    fn splits_case(self) -> bool {
        self.support != FontSupport::Full
            && self.requested != FontVariantCaps::TitlingCaps
            && self.may_synthesize
    }

    /// The caps whose OpenType features a run is shaped with.
    fn feature_caps(self, run: CaseRun) -> FontVariantCaps {
        use FontVariantCaps as C;
        match (self.support, self.requested) {
            (FontSupport::Full, requested) => requested,
            (FontSupport::Fallback, C::AllPetiteCaps) => C::AllSmallCaps,
            (FontSupport::Fallback, C::PetiteCaps) => C::SmallCaps,
            (FontSupport::Fallback, C::Unicase) if run == CaseRun::SameCase => C::SmallCaps,
            _ => C::Normal,
        }
    }

    fn is_synthetic(self, run: CaseRun) -> bool {
        use CapsSynthesis as S;
        self.support == FontSupport::None
            && matches!(
                (run, self.synthesis()),
                (
                    CaseRun::UppercaseNeeded,
                    S::LowerToSmallCaps | S::BothToSmallCaps
                ) | (CaseRun::SameCase, S::UpperToSmallCaps | S::BothToSmallCaps)
            )
    }

    fn case_change(self, run: CaseRun) -> CaseChange {
        use CapsSynthesis as S;
        let synthesis = self.synthesis();
        match (self.support, run) {
            (FontSupport::Full, _) => CaseChange::Keep,
            (FontSupport::Fallback, CaseRun::SameCase)
                if matches!(synthesis, S::BothToSmallCaps | S::UpperToSmallCaps) =>
            {
                CaseChange::Lower
            }
            (FontSupport::None, CaseRun::UppercaseNeeded)
                if matches!(synthesis, S::LowerToSmallCaps | S::BothToSmallCaps) =>
            {
                CaseChange::Upper
            }
            _ => CaseChange::Keep,
        }
    }
}

/// The caps plan of every text node laid out in the inline context of `root_id` whose
/// element asks for caps other than `normal`. Font support is read here, before the
/// parley builder borrows `font_ctx` for the whole build.
pub(crate) fn plan_caps(
    nodes: &crate::NodeTree,
    font_ctx: &mut FontContext,
    root_id: NodeId,
) -> HashMap<NodeId, CapsPlan> {
    let mut plans = HashMap::new();
    let root = &nodes[root_id];
    for child_id in root
        .before()
        .into_iter()
        .chain(root.flat_children().iter().copied())
        .chain(root.after())
    {
        collect_caps_plans(nodes, font_ctx, child_id, &mut plans);
    }
    plans
}

/// Mirrors the descent of `build_inline_layout_into`: atomic inlines lay out their own
/// inline contexts.
fn collect_caps_plans(
    nodes: &crate::NodeTree,
    font_ctx: &mut FontContext,
    node_id: NodeId,
    plans: &mut HashMap<NodeId, CapsPlan>,
) {
    let node: &Node = &nodes[node_id];
    match &node.data {
        NodeData::Element(element) | NodeData::AnonymousBlock(element) => {
            let display = node.display_style().unwrap_or(Display::inline());
            let has_pseudos = match (display.outside(), display.inside()) {
                (DisplayOutside::None, DisplayInside::Contents) => false,
                (DisplayOutside::Inline, DisplayInside::Flow) => {
                    let tag = &element.name.local;
                    if is_replaced_element(tag)
                        || *tag == local_name!("input")
                        || *tag == local_name!("textarea")
                        || *tag == local_name!("button")
                        || *tag == local_name!("br")
                    {
                        return;
                    }
                    true
                }
                _ => return,
            };
            let before = node.before().filter(|_| has_pseudos);
            let after = node.after().filter(|_| has_pseudos);
            for child_id in before
                .into_iter()
                .chain(node.flat_children().iter().copied())
                .chain(after)
            {
                collect_caps_plans(nodes, font_ctx, child_id, plans);
            }
        }
        NodeData::Text(data) => {
            let Some(styles) = node.parent.and_then(|id| nodes[id].primary_styles()) else {
                return;
            };
            let font = styles.get_font();
            if font.font_variant_caps == FontVariantCaps::Normal {
                return;
            }
            // Whitespace alone keeps the element's own caps features.
            let Some(features) = primary_font_features(font_ctx, font, &data.content) else {
                return;
            };
            plans.insert(
                node_id,
                CapsPlan::new(
                    font.font_variant_caps,
                    |tag| features.contains(tag),
                    font.font_synthesis_small_caps == FontSynthesis::Auto,
                ),
            );
        }
        _ => {}
    }
}

/// The GSUB feature tags of the font `text` is set in: the first of its families, then
/// of the script's fallbacks, that maps its first letter (or first non-space character).
/// Empty for a font without GSUB, or when no font maps that character; `None` when
/// `text` is all whitespace.
fn primary_font_features(
    font_ctx: &mut FontContext,
    font: &Font,
    text: &str,
) -> Option<Vec<[u8; 4]>> {
    let probe = text
        .chars()
        .find(|c| c.is_alphabetic())
        .or_else(|| text.chars().find(|c| !c.is_whitespace()))?;
    let script = PropertyNamesShort::<Script>::new()
        .get(CodePointMapData::<Script>::new().get(probe))
        .unwrap_or("Zzzz")
        .parse()
        .unwrap_or(parley::fontique::Script::UNKNOWN);

    let mut query = font_ctx.collection.query(&mut font_ctx.source_cache);
    query.set_families(
        font.font_family
            .families
            .iter()
            .map(stylo_to_parley::query_font_family),
    );
    query.set_attributes(Attributes {
        width: stylo_to_parley::font_width(font.font_stretch),
        weight: stylo_to_parley::font_weight(font.font_weight),
        style: stylo_to_parley::font_style(font.font_style),
    });
    query.set_fallbacks(FallbackKey::new(script, None));
    let Some(found) = find_font_for(&mut query, probe) else {
        return Some(Vec::new());
    };
    // `find_font_for` already parsed this font.
    let font_ref = skrifa::FontRef::from_index(found.blob.as_ref(), found.index).ok()?;
    let Ok(feature_list) = font_ref.gsub().and_then(|gsub| gsub.feature_list()) else {
        return Some(Vec::new());
    };
    Some(
        feature_list
            .feature_records()
            .iter()
            .map(|record| record.feature_tag().to_be_bytes())
            .collect(),
    )
}

/// Push `text`, set in `font`, into `builder` with its caps planned by `caps` and its
/// emoji presented as `font-variant-emoji` asks.
pub(crate) fn push_text(
    builder: &mut TreeBuilder<'_, TextBrush>,
    text: &str,
    font: &Font,
    caps: Option<CapsPlan>,
) {
    let text = present_emoji(text, font.font_variant_emoji);
    let Some(plan) = caps.filter(|plan| plan.support != FontSupport::Full) else {
        builder.push_text(&text);
        return;
    };
    if !plan.splits_case() {
        push_with_caps(
            builder,
            &text,
            font,
            plan.feature_caps(CaseRun::SameCase),
            false,
        );
        return;
    }
    for (run, segment) in case_runs(&text) {
        let segment = match plan.case_change(run) {
            CaseChange::Keep => Cow::Borrowed(segment),
            CaseChange::Upper => Cow::Owned(segment.to_uppercase()),
            CaseChange::Lower => Cow::Owned(segment.to_lowercase()),
        };
        push_with_caps(
            builder,
            &segment,
            font,
            plan.feature_caps(run),
            plan.is_synthetic(run),
        );
    }
}

fn push_with_caps(
    builder: &mut TreeBuilder<'_, TextBrush>,
    text: &str,
    font: &Font,
    caps: FontVariantCaps,
    is_synthetic: bool,
) {
    if caps == font.font_variant_caps && !is_synthetic {
        builder.push_text(text);
        return;
    }
    let features = stylo_to_parley::font_features_with_caps(font, caps);
    let mut properties = vec![StyleProperty::FontFeatures(FontFeatures::List(Cow::Owned(
        features,
    )))];
    if is_synthetic {
        properties.push(StyleProperty::FontSize(
            font.font_size.used_size.0.px() * SMALL_CAPS_SCALE,
        ));
    }
    builder.push_style_modification_span(&properties);
    builder.push_text(text);
    builder.pop_style_span();
}

fn case_run(ch: char) -> CaseRun {
    let mut upper = ch.to_uppercase();
    if upper.len() == 1 && upper.next() == Some(ch) {
        CaseRun::SameCase
    } else {
        CaseRun::UppercaseNeeded
    }
}

/// `text` split into maximal runs of one [`CaseRun`].
fn case_runs(text: &str) -> impl Iterator<Item = (CaseRun, &str)> {
    let mut rest = text;
    std::iter::from_fn(move || {
        let run = case_run(rest.chars().next()?);
        let end = rest
            .char_indices()
            .find(|(_, ch)| case_run(*ch) != run)
            .map_or(rest.len(), |(index, _)| index);
        let (segment, tail) = rest.split_at(end);
        rest = tail;
        Some((run, segment))
    })
}

/// `text` with the variation selector of the presentation `emoji` asks for after each
/// emoji character that carries none: `text` → VS15 on every emoji, `emoji` → VS16 on
/// those that default to text, `unicode` → each one's default (`Emoji_Presentation`).
/// Components (keycap bases, regional indicators, skin tones) take no selector.
fn present_emoji(text: &str, emoji: FontVariantEmoji) -> Cow<'_, str> {
    const IS_EMOJI: icu_properties::CodePointSetDataBorrowed<'static> =
        CodePointSetData::new::<Emoji>();
    const IS_COMPONENT: icu_properties::CodePointSetDataBorrowed<'static> =
        CodePointSetData::new::<EmojiComponent>();
    const IS_EMOJI_PRESENTATION: icu_properties::CodePointSetDataBorrowed<'static> =
        CodePointSetData::new::<EmojiPresentation>();

    if emoji == FontVariantEmoji::Normal {
        return Cow::Borrowed(text);
    }
    let selector = |ch: char| {
        if !IS_EMOJI.contains(ch) || IS_COMPONENT.contains(ch) {
            return None;
        }
        let is_emoji_default = IS_EMOJI_PRESENTATION.contains(ch);
        match emoji {
            FontVariantEmoji::Text => Some(TEXT_PRESENTATION),
            FontVariantEmoji::Emoji => (!is_emoji_default).then_some(EMOJI_PRESENTATION),
            FontVariantEmoji::Unicode if is_emoji_default => Some(EMOJI_PRESENTATION),
            FontVariantEmoji::Unicode => Some(TEXT_PRESENTATION),
            FontVariantEmoji::Normal => None,
        }
    };

    let mut out: Option<String> = None;
    let mut chars = text.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        let next = chars.peek().map(|(_, next)| *next);
        let added =
            selector(ch).filter(|_| !matches!(next, Some(TEXT_PRESENTATION | EMOJI_PRESENTATION)));
        match (&mut out, added) {
            (Some(out), added) => {
                out.push(ch);
                out.extend(added);
            }
            (None, Some(added)) => {
                let mut copy = String::with_capacity(text.len() + 8);
                copy.push_str(&text[..index]);
                copy.push(ch);
                copy.push(added);
                out = Some(copy);
            }
            (None, None) => {}
        }
    }
    out.map_or(Cow::Borrowed(text), Cow::Owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(caps: FontVariantCaps, tags: &[&[u8; 4]], may_synthesize: bool) -> CapsPlan {
        CapsPlan::new(caps, |tag| tags.contains(&tag), may_synthesize)
    }

    /// One `(case, caps features, synthetic)` per case run of `text`, as `push_text`
    /// shapes them.
    fn shaped(plan: CapsPlan, text: &str) -> Vec<(String, FontVariantCaps, bool)> {
        case_runs(text)
            .map(|(run, segment)| {
                let segment = match plan.case_change(run) {
                    CaseChange::Keep => segment.to_owned(),
                    CaseChange::Upper => segment.to_uppercase(),
                    CaseChange::Lower => segment.to_lowercase(),
                };
                (segment, plan.feature_caps(run), plan.is_synthetic(run))
            })
            .collect()
    }

    /// Chromium with a font that has no caps features (headless Edge 156 widths of
    /// "Loki" in Noto Sans without GSUB): small caps shrink the uppercased lowercase
    /// letters, all-small-caps shrink every letter, unicase only the capitals.
    #[test]
    fn caps_without_font_support_are_synthesized() {
        use FontVariantCaps as C;
        assert_eq!(
            shaped(plan(C::SmallCaps, &[], true), "Loki"),
            [
                ("L".into(), C::Normal, false),
                ("OKI".into(), C::Normal, true)
            ]
        );
        assert_eq!(
            shaped(plan(C::AllSmallCaps, &[], true), "Loki"),
            [
                ("L".into(), C::Normal, true),
                ("OKI".into(), C::Normal, true)
            ]
        );
        assert_eq!(
            shaped(plan(C::Unicase, &[], true), "Loki"),
            [
                ("L".into(), C::Normal, true),
                ("oki".into(), C::Normal, false)
            ]
        );
        assert!(!plan(C::TitlingCaps, &[], true).splits_case());
        assert!(!plan(C::SmallCaps, &[], false).splits_case());
    }

    /// With small caps but no petite caps or unicase, those fall back to the small-caps
    /// features; unicase lowercases the capitals into small caps.
    #[test]
    fn missing_petite_caps_and_unicase_fall_back_to_small_caps() {
        use FontVariantCaps as C;
        let tags: &[&[u8; 4]] = &[b"smcp", b"c2sc"];
        assert_eq!(
            shaped(plan(C::PetiteCaps, tags, true), "Loki"),
            [
                ("L".into(), C::SmallCaps, false),
                ("oki".into(), C::SmallCaps, false)
            ]
        );
        assert_eq!(
            plan(C::PetiteCaps, tags, false).feature_caps(CaseRun::SameCase),
            C::SmallCaps
        );
        assert_eq!(
            plan(C::AllPetiteCaps, tags, false).feature_caps(CaseRun::SameCase),
            C::AllSmallCaps
        );
        assert_eq!(
            shaped(plan(C::Unicase, tags, true), "Loki"),
            [
                ("l".into(), C::SmallCaps, false),
                ("oki".into(), C::Normal, false)
            ]
        );
        assert_eq!(plan(C::SmallCaps, tags, true).support, FontSupport::Full);
    }

    #[test]
    fn emoji_presentation_adds_the_requested_selector() {
        let text = "©\u{1F600}1x\u{2764}\u{FE0F}";
        assert_eq!(present_emoji(text, FontVariantEmoji::Normal), text);
        assert_eq!(
            present_emoji(text, FontVariantEmoji::Text),
            "©\u{FE0E}\u{1F600}\u{FE0E}1x\u{2764}\u{FE0F}"
        );
        assert_eq!(
            present_emoji(text, FontVariantEmoji::Emoji),
            "©\u{FE0F}\u{1F600}1x\u{2764}\u{FE0F}"
        );
        assert_eq!(
            present_emoji(text, FontVariantEmoji::Unicode),
            "©\u{FE0E}\u{1F600}\u{FE0F}1x\u{2764}\u{FE0F}"
        );
    }
}
