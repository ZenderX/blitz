//! `hyphens` (css-text-3 §5.4), applied to text before shaping: `none` drops the soft
//! hyphens (U+00AD) that would offer a break, `manual` keeps them, and `auto` adds one at
//! each break the language's hyphenation resource allows, unless the word already has one.
//! A line that breaks at a soft hyphen shows a hyphen ([`TextLayout::line_hyphen`]).
//!
//! The only resource is TeX's `hyph-en-us.tex` (Knuth–Liang patterns plus Knuth's
//! exception list), vendored verbatim in `assets/`, so `auto` hyphenates text whose
//! content language is English and leaves other text as `manual`.
//!
//! [`TextLayout::line_hyphen`]: crate::node::TextLayout::line_hyphen

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::OnceLock;

use blitz_traits::node_id::NodeId;
use markup5ever::local_name;
use parley::{BreakReason, IndentOptions};
use style::computed_values::hyphens::T as Hyphens;

use crate::node::TextLayout;

pub(crate) const SOFT_HYPHEN: char = '\u{AD}';

const EN_US_TEX: &str = include_str!("../../assets/hyph-en-us.tex");

/// The fewest letters kept before and after a break: the file's own `hyphenmins`.
const LEFT_MIN: usize = 2;
const RIGHT_MIN: usize = 3;

/// `text`, in an element whose content language is `lang`, with the soft hyphens its
/// `hyphens` value calls for.
pub(crate) fn apply<'t>(text: &'t str, hyphens: Hyphens, lang: Option<&str>) -> Cow<'t, str> {
    match hyphens {
        Hyphens::Manual => Cow::Borrowed(text),
        Hyphens::None if text.contains(SOFT_HYPHEN) => Cow::Owned(text.replace(SOFT_HYPHEN, "")),
        Hyphens::None => Cow::Borrowed(text),
        Hyphens::Auto if lang.is_some_and(is_english) => Cow::Owned(hyphenate(text)),
        Hyphens::Auto => Cow::Borrowed(text),
    }
}

/// The content language of `node_id`: the nearest `lang` attribute on it or an ancestor.
pub(crate) fn content_language(nodes: &crate::NodeTree, node_id: NodeId) -> Option<&str> {
    let mut current = Some(node_id);
    while let Some(id) = current {
        let node = &nodes[id];
        if let Some(lang) = node
            .element_data()
            .and_then(|element| element.attr(local_name!("lang")))
        {
            return Some(lang);
        }
        current = node.parent;
    }
    None
}

/// The room line `line` leaves for its hyphen, from the `(line, advance)` pairs of
/// [`unfitted_hyphen`].
pub(crate) fn reserved_room(reserved: &[(usize, f32)], line: usize) -> f32 {
    reserved
        .iter()
        .find(|&&(reserved, _)| reserved == line)
        .map_or(0.0, |&(_, advance)| advance)
}

/// The first line of `text_layout` that breaks at a soft hyphen with no room for the
/// hyphen and none reserved in `reserved`, with the hyphen's advance. `indent` and
/// `options` are what the layout was given: the line breaker takes the indent from each
/// line's room, and this mirrors which lines parley indents (`BreakLines::resolve_indent`).
pub(crate) fn unfitted_hyphen(
    text_layout: &TextLayout,
    indent: f32,
    options: IndentOptions,
    reserved: &[(usize, f32)],
) -> Option<(usize, f32)> {
    let mut previous_reason = None;
    for (index, line) in text_layout.layout.lines().enumerate() {
        let is_scope_line = previous_reason.is_none()
            || (options.each_line && previous_reason == Some(BreakReason::Explicit));
        previous_reason = Some(line.break_reason());
        if reserved.iter().any(|&(reserved, _)| reserved == index) {
            continue;
        }
        let Some(hyphen) = text_layout.line_hyphen(&line) else {
            continue;
        };
        let line_indent = if is_scope_line ^ options.hanging {
            indent
        } else {
            0.0
        };
        let metrics = line.metrics();
        let room = metrics.inline_max_coord - metrics.inline_min_coord - line_indent;
        if metrics.advance + hyphen.advance > room {
            return Some((index, hyphen.advance));
        }
    }
    None
}

fn is_english(lang: &str) -> bool {
    let primary = lang.split(['-', '_']).next().unwrap_or("");
    primary.eq_ignore_ascii_case("en")
}

/// `text` with a soft hyphen at each pattern break of each ASCII word. Words that hold a
/// soft hyphen keep only theirs (css-text-3 §5.4: automatic opportunities are ignored in
/// a word that contains a conditional hyphen).
fn hyphenate(text: &str) -> String {
    let patterns = en_us();
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(is_word_char) {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let end = rest.find(|ch| !is_word_char(ch)).unwrap_or(rest.len());
        let (word, tail) = rest.split_at(end);
        rest = tail;
        if word.contains(SOFT_HYPHEN) || !word.is_ascii() {
            out.push_str(word);
            continue;
        }
        let mut breaks = patterns
            .breaks(&word.to_ascii_lowercase())
            .into_iter()
            .peekable();
        for (index, ch) in word.char_indices() {
            if breaks.next_if_eq(&index).is_some() {
                out.push(SOFT_HYPHEN);
            }
            out.push(ch);
        }
    }
    out.push_str(rest);
    out
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphabetic() || ch == SOFT_HYPHEN
}

struct Patterns {
    /// Each pattern's letters (`.` marks a word edge) to the digit before each letter
    /// and after the last.
    values: HashMap<String, Vec<u8>>,
    max_len: usize,
    /// Words hyphenated by hand, to the byte offsets of their breaks.
    exceptions: HashMap<String, Vec<usize>>,
}

fn en_us() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    PATTERNS.get_or_init(|| Patterns::parse(EN_US_TEX))
}

impl Patterns {
    fn parse(tex: &str) -> Self {
        let mut values = HashMap::new();
        for pattern in tex_block(tex, "\\patterns{") {
            let mut letters = String::new();
            let mut digits = vec![0];
            for ch in pattern.chars() {
                match ch.to_digit(10) {
                    Some(digit) => *digits.last_mut().unwrap() = digit as u8,
                    None => {
                        letters.push(ch);
                        digits.push(0);
                    }
                }
            }
            values.insert(letters, digits);
        }
        let max_len = values.keys().map(String::len).max().unwrap_or(0);
        let exceptions = tex_block(tex, "\\hyphenation{")
            .map(|word| {
                let mut breaks = Vec::new();
                let mut letters = String::new();
                for ch in word.chars() {
                    if ch == '-' {
                        breaks.push(letters.len());
                    } else {
                        letters.push(ch);
                    }
                }
                (letters, breaks)
            })
            .collect();
        Self {
            values,
            max_len,
            exceptions,
        }
    }

    /// The byte offsets in lowercase ASCII `word` where it may break.
    fn breaks(&self, word: &str) -> Vec<usize> {
        let allowed = LEFT_MIN..=word.len().saturating_sub(RIGHT_MIN);
        if let Some(breaks) = self.exceptions.get(word) {
            return breaks
                .iter()
                .copied()
                .filter(|index| allowed.contains(index))
                .collect();
        }
        let dotted = format!(".{word}.");
        // `levels[i]` is the highest digit any pattern puts before `dotted[i]`.
        let mut levels = vec![0u8; dotted.len() + 1];
        for start in 0..dotted.len() {
            for end in start + 1..=dotted.len().min(start + self.max_len) {
                if let Some(digits) = self.values.get(&dotted[start..end]) {
                    for (offset, &digit) in digits.iter().enumerate() {
                        levels[start + offset] = levels[start + offset].max(digit);
                    }
                }
            }
        }
        allowed
            .filter(|&index| levels[index + 1] % 2 == 1)
            .collect()
    }
}

/// The whitespace-separated entries of the TeX command block that opens with `command`.
fn tex_block<'a>(tex: &'a str, command: &'a str) -> impl Iterator<Item = &'a str> {
    tex.lines()
        .map(|line| line.split('%').next().unwrap_or(""))
        .skip_while(move |line| !line.trim_start().starts_with(command))
        .skip(1)
        .take_while(|line| line.trim() != "}")
        .flat_map(str::split_whitespace)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Liang's worked example (*Word Hy-phen-a-tion by Com-put-er*, 1983): the patterns
    /// that apply to "hyphenation" give hy-phen-ation.
    const LIANG_EXAMPLE: &str =
        "\\patterns{\nhy3ph\nhe2n\nhena4\nhen5at\n1na\nn2at\n1tio\n2io\no2n\n}\n";

    #[test]
    fn odd_levels_are_the_breaks() {
        let patterns = Patterns::parse(LIANG_EXAMPLE);
        assert_eq!(patterns.breaks("hyphenation"), vec![2, 6]);
    }

    #[test]
    fn breaks_keep_the_file_minimum_letters_on_each_side() {
        let patterns = Patterns::parse("\\patterns{\na1b\nb1c\nc1d\nd1e\ne1f\n}\n");
        // Every gap of `abcdef` is odd; `hyphenmins` 2 and 3 leave only ab-c and abc-d.
        assert_eq!(patterns.breaks("abcdef"), vec![2, 3]);
    }

    #[test]
    fn vendored_resource_parses_whole() {
        let patterns = en_us();
        // `hyph-en-us.tex`: 4,938 patterns (lines 61–5000) and Knuth's 14 exceptions.
        assert_eq!(patterns.values.len(), 4938);
        assert_eq!(patterns.exceptions.len(), 14);
        assert_eq!(patterns.breaks("table"), vec![2]);
        assert_eq!(patterns.breaks("present"), Vec::<usize>::new());
    }

    #[test]
    fn hyphens_value_decides_the_soft_hyphens() {
        let shy = "co\u{AD}operate";
        assert_eq!(apply(shy, Hyphens::Manual, Some("en")), shy);
        assert_eq!(apply(shy, Hyphens::None, Some("en")), "cooperate");
        // A word with its own soft hyphen gets no automatic ones.
        assert_eq!(apply(shy, Hyphens::Auto, Some("en")), shy);
        // `auto` needs a language with a resource.
        assert_eq!(apply("hyphenation", Hyphens::Auto, None), "hyphenation");
        assert_eq!(
            apply("hyphenation", Hyphens::Auto, Some("fr")),
            "hyphenation"
        );
        let auto = apply("Table, extraordinarily.", Hyphens::Auto, Some("en-US"));
        assert!(auto.starts_with("Ta\u{AD}ble, "), "{auto:?}");
        assert!(
            auto.contains(SOFT_HYPHEN)
                && auto.replace(SOFT_HYPHEN, "") == "Table, extraordinarily."
        );
        assert!(
            auto["Ta\u{AD}ble, ".len()..].contains(SOFT_HYPHEN),
            "a long word breaks: {auto:?}"
        );
    }
}
