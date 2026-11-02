//! Counter representations (css-counter-styles-3 §3): the text a counter value shows
//! as, for `counter()`/`counters()` and for list markers.
//!
//! Covers the document's `@counter-style` rules (every `system`, `extends` included,
//! and the `negative`, `prefix`, `suffix`, `range`, `pad` and `fallback` descriptors),
//! the anonymous `symbols()` styles and every predefined style, the simple ones of §6
//! and the complex ones of §7. A name no rule or predefined style defines falls back to
//! `decimal` (§3.1).

mod complex;
mod predefined;

use style::Atom;
use style::counter_style::{
    AdditiveTuple, CounterBound, CounterRange, CounterStyle, CounterStyleRule, Symbol, SymbolsType,
    System,
};
use style::values::specified::Integer;

use crate::node::Marker;

/// Representations longer than this many symbols fall back, so a huge counter in a
/// symbolic or additive style cannot allocate without bound. css-counter-styles-3
/// §3.1.3 and §3.1.6 let UAs cap the length; Gecko uses the same 60.
const MAX_SYMBOL_REPEATS: u64 = 60;

/// The symbols of a counter style. Predefined styles list one character per symbol;
/// `@counter-style` and `symbols()` list stylo symbols.
#[derive(Clone, Copy)]
enum Symbols<'a> {
    Chars(&'static str),
    List(&'a [Symbol]),
}

impl Symbols<'_> {
    fn len(self) -> u64 {
        match self {
            Symbols::Chars(chars) => chars.chars().count() as u64,
            Symbols::List(list) => list.len() as u64,
        }
    }

    fn push(self, index: u64, out: &mut String) {
        match self {
            Symbols::Chars(chars) => out.extend(chars.chars().nth(index as usize)),
            Symbols::List(list) => out.push_str(symbol_str(&list[index as usize])),
        }
    }
}

fn symbol_str(symbol: &Symbol) -> &str {
    match symbol {
        Symbol::String(string) => string,
        Symbol::Ident(ident) => &ident.0,
    }
}

/// The `additive-symbols` of a predefined style or of an `@counter-style` rule, in
/// descending weight order (stylo rejects any other order).
#[derive(Clone, Copy)]
enum AdditiveSymbols<'a> {
    Table(&'static [(u32, &'static str)]),
    List(&'a [AdditiveTuple]),
}

impl<'a> AdditiveSymbols<'a> {
    fn tuples(self) -> impl Iterator<Item = (u64, &'a str)> {
        let (table, list) = match self {
            AdditiveSymbols::Table(table) => (table, &[][..]),
            AdditiveSymbols::List(list) => (&[][..], list),
        };
        let table = table
            .iter()
            .map(|&(weight, symbol)| (u64::from(weight), symbol));
        // stylo only accepts weights that resolve to non-negative integers.
        let list = list.iter().filter_map(|tuple| {
            let weight = u64::try_from(tuple.weight.resolve()?).ok()?;
            Some((weight, symbol_str(&tuple.symbol)))
        });
        table.chain(list)
    }
}

/// A counter style's `system` (css-counter-styles-3 §3.1).
#[derive(Clone, Copy)]
enum Algorithm<'a> {
    Cyclic(Symbols<'a>),
    Fixed(i32, Symbols<'a>),
    Symbolic(Symbols<'a>),
    Alphabetic(Symbols<'a>),
    Numeric(Symbols<'a>),
    Additive(AdditiveSymbols<'a>),
    Complex(complex::Style),
}

impl Algorithm<'_> {
    /// The `auto` range (css-counter-styles-3 §3.5).
    fn auto_range(self) -> (i32, i32) {
        match self {
            Algorithm::Cyclic(_) | Algorithm::Fixed(..) | Algorithm::Numeric(_) => {
                (i32::MIN, i32::MAX)
            }
            Algorithm::Symbolic(_) | Algorithm::Alphabetic(_) => (1, i32::MAX),
            Algorithm::Additive(_) => (0, i32::MAX),
            Algorithm::Complex(style) => style.range(),
        }
    }

    /// Whether negative values use the `negative` sign (css-counter-styles-3 §3.3).
    fn uses_negative(self) -> bool {
        match self {
            Algorithm::Cyclic(_) | Algorithm::Fixed(..) => false,
            Algorithm::Symbolic(_)
            | Algorithm::Alphabetic(_)
            | Algorithm::Numeric(_)
            | Algorithm::Additive(_)
            | Algorithm::Complex(_) => true,
        }
    }

    /// The initial representation, or `None` where the algorithm cannot represent
    /// `value`. Styles that use a negative sign get the absolute value.
    fn generate(self, value: i64) -> Option<String> {
        let mut out = String::new();
        match self {
            Algorithm::Cyclic(symbols) => {
                symbols.push(
                    (value - 1).rem_euclid(symbols.len() as i64) as u64,
                    &mut out,
                );
            }
            Algorithm::Fixed(first, symbols) => {
                let index = value - i64::from(first);
                if index < 0 || index as u64 >= symbols.len() {
                    return None;
                }
                symbols.push(index as u64, &mut out);
            }
            Algorithm::Symbolic(symbols) => {
                let value = u64::try_from(value).ok().filter(|v| *v >= 1)?;
                let n = symbols.len();
                let repeats = (value - 1) / n + 1;
                if repeats > MAX_SYMBOL_REPEATS {
                    return None;
                }
                for _ in 0..repeats {
                    symbols.push((value - 1) % n, &mut out);
                }
            }
            Algorithm::Alphabetic(symbols) => {
                let mut value = u64::try_from(value).ok().filter(|v| *v >= 1)?;
                let n = symbols.len();
                let mut digits = Vec::new();
                while value != 0 {
                    value -= 1;
                    digits.push(value % n);
                    value /= n;
                }
                for digit in digits.into_iter().rev() {
                    symbols.push(digit, &mut out);
                }
            }
            Algorithm::Numeric(symbols) => {
                let mut value = u64::try_from(value).ok()?;
                let n = symbols.len();
                let mut digits = vec![value % n];
                value /= n;
                while value != 0 {
                    digits.push(value % n);
                    value /= n;
                }
                for digit in digits.into_iter().rev() {
                    symbols.push(digit, &mut out);
                }
            }
            Algorithm::Additive(tuples) => {
                let mut value = u64::try_from(value).ok()?;
                if value == 0 {
                    let (_, zero) = tuples.tuples().find(|(weight, _)| *weight == 0)?;
                    return Some(zero.to_owned());
                }
                let mut used = 0;
                for (weight, symbol) in tuples.tuples() {
                    if weight == 0 || value < weight {
                        continue;
                    }
                    let repeats = value / weight;
                    used += repeats;
                    if used > MAX_SYMBOL_REPEATS {
                        return None;
                    }
                    for _ in 0..repeats {
                        out.push_str(symbol);
                    }
                    value -= weight * repeats;
                    if value == 0 {
                        break;
                    }
                }
                if value != 0 {
                    return None;
                }
            }
            Algorithm::Complex(style) => return style.generate(value),
        }
        Some(out)
    }
}

/// A counter style's `range` (css-counter-styles-3 §3.5).
#[derive(Clone, Copy)]
enum Range<'a> {
    Auto,
    Bounds(i32, i32),
    /// An `@counter-style` rule's ranges; none is `auto`.
    List(&'a [CounterRange]),
}

impl Range<'_> {
    fn contains(self, algorithm: Algorithm<'_>, value: i32) -> bool {
        let within = |(min, max): (i32, i32)| (min..=max).contains(&value);
        match self {
            Range::Bounds(min, max) => within((min, max)),
            Range::List(ranges) if !ranges.is_empty() => ranges.iter().any(|range| {
                // A bound stylo cannot resolve at parse time bounds nothing.
                let bound = |bound: &CounterBound, infinite: i32| match bound {
                    CounterBound::Integer(integer) => integer.resolve(),
                    CounterBound::Infinite => Some(infinite),
                };
                match (bound(&range.start, i32::MIN), bound(&range.end, i32::MAX)) {
                    (Some(min), Some(max)) => within((min, max)),
                    _ => false,
                }
            }),
            Range::Auto | Range::List(_) => within(algorithm.auto_range()),
        }
    }
}

/// The descriptors of one counter style (css-counter-styles-3 §3).
#[derive(Clone, Copy)]
pub(crate) struct Definition<'a> {
    algorithm: Algorithm<'a>,
    range: Range<'a>,
    negative: (&'a str, &'a str),
    prefix: &'a str,
    suffix: &'a str,
    pad: (usize, &'a str),
    /// The name of the style to fall back to.
    fallback: &'a str,
}

impl<'a> Definition<'a> {
    const fn new(algorithm: Algorithm<'a>) -> Self {
        Self {
            algorithm,
            range: Range::Auto,
            negative: ("-", ""),
            prefix: "",
            suffix: ". ",
            pad: (0, ""),
            fallback: "decimal",
        }
    }

    /// "Generate a counter representation" (css-counter-styles-3 §3.1), without
    /// the prefix and suffix, through the fallback styles where this one cannot.
    fn represent<'s: 'a>(&self, styles: &CounterStyles<'s>, value: i32) -> String {
        let mut visited = Vec::new();
        let mut style = *self;
        loop {
            if let Some(text) = style.represent_alone(value) {
                return text;
            }
            // §3.7: a loop of fallbacks ends in `decimal`.
            if visited.contains(&style.fallback) {
                return decimal()
                    .represent_alone(value)
                    .expect("decimal represents all");
            }
            visited.push(style.fallback);
            style = styles.named(&Atom::from(style.fallback));
        }
    }

    /// The representation in this style alone, `None` where it needs the fallback.
    fn represent_alone(&self, value: i32) -> Option<String> {
        if !self.range.contains(self.algorithm, value) {
            return None;
        }
        let is_negative = value < 0 && self.algorithm.uses_negative();
        let magnitude = if is_negative {
            -i64::from(value)
        } else {
            i64::from(value)
        };
        let initial = self.algorithm.generate(magnitude)?;

        let (pad_length, pad_symbol) = self.pad;
        let mut length = initial.chars().count();
        if is_negative {
            length += self.negative.0.chars().count() + self.negative.1.chars().count();
        }
        let padding = pad_symbol.repeat(pad_length.saturating_sub(length));
        Some(if is_negative {
            format!("{}{padding}{initial}{}", self.negative.0, self.negative.1)
        } else {
            padding + &initial
        })
    }

    /// `prefix`, representation and `suffix`: a list marker's text (css-lists-3 §3.1).
    fn marker_text<'s: 'a>(&self, styles: &CounterStyles<'s>, value: i32) -> String {
        format!(
            "{}{}{}",
            self.prefix,
            self.represent(styles, value),
            self.suffix
        )
    }
}

fn decimal() -> Definition<'static> {
    predefined::lookup("decimal").expect("decimal is predefined")
}

/// The counter styles a document defines: its `@counter-style` rules, which stylo's
/// stylist collects (only those that define a counter style), over the predefined
/// styles.
pub(crate) struct CounterStyles<'a> {
    rule: &'a dyn Fn(&Atom) -> Option<&'a CounterStyleRule>,
}

impl<'a> CounterStyles<'a> {
    /// The styles of the rules `rule` finds by name.
    pub(crate) fn new(rule: &'a dyn Fn(&Atom) -> Option<&'a CounterStyleRule>) -> Self {
        Self { rule }
    }

    /// The style a `<counter-style-name>` names: an `@counter-style` rule, else a
    /// predefined style, else `decimal`.
    fn named(&self, name: &Atom) -> Definition<'a> {
        match (self.rule)(name) {
            // A rule without the symbols its system needs would leave no symbol to pick.
            Some(rule) if rule.defines_counter_style() => self.defined_by(rule),
            _ => predefined::lookup(name).unwrap_or_else(decimal),
        }
    }

    /// The style an `@counter-style` rule defines (css-counter-styles-3 §3).
    fn defined_by(&self, rule: &'a CounterStyleRule) -> Definition<'a> {
        let descriptors = rule.descriptors();
        let symbols = Symbols::List(descriptors.symbols.as_ref().map_or(&[], |s| &s.0));
        let base = match rule.resolved_system() {
            // §3.1.7: unset descriptors come from the extended style, and every style
            // in a loop of `extends` extends `decimal` instead.
            System::Extends(_) if self.extends_into_loop(rule) => decimal(),
            System::Extends(other) => self.named(&other.0),
            System::Cyclic => Definition::new(Algorithm::Cyclic(symbols)),
            System::Numeric => Definition::new(Algorithm::Numeric(symbols)),
            System::Alphabetic => Definition::new(Algorithm::Alphabetic(symbols)),
            System::Symbolic => Definition::new(Algorithm::Symbolic(symbols)),
            System::Fixed { first_symbol_value } => Definition::new(Algorithm::Fixed(
                first_symbol_value
                    .as_ref()
                    .and_then(Integer::resolve)
                    .unwrap_or(1),
                symbols,
            )),
            System::Additive => Definition::new(Algorithm::Additive(AdditiveSymbols::List(
                descriptors.additive_symbols.as_ref().map_or(&[], |a| &a.0),
            ))),
        };
        Definition {
            range: descriptors
                .range
                .as_ref()
                .map_or(base.range, |ranges| Range::List(&ranges.0)),
            negative: descriptors
                .negative
                .as_ref()
                .map_or(base.negative, |negative| {
                    (
                        symbol_str(&negative.0),
                        negative.1.as_ref().map_or("", symbol_str),
                    )
                }),
            prefix: descriptors.prefix.as_ref().map_or(base.prefix, symbol_str),
            suffix: descriptors.suffix.as_ref().map_or(base.suffix, symbol_str),
            pad: descriptors.pad.as_ref().map_or(base.pad, |pad| {
                let length = pad
                    .0
                    .resolve()
                    .and_then(|length| usize::try_from(length).ok());
                (length.unwrap_or(0), symbol_str(&pad.1))
            }),
            fallback: descriptors
                .fallback
                .as_ref()
                .map_or(base.fallback, |fallback| &fallback.0.0),
            ..base
        }
    }

    /// Whether following `extends` from `rule` comes back to `rule`.
    fn extends_into_loop(&self, rule: &CounterStyleRule) -> bool {
        let start = &rule.name().0;
        let mut visited: Vec<&Atom> = Vec::new();
        let mut current = rule;
        while let System::Extends(next) = current.resolved_system() {
            if next.0 == *start {
                return true;
            }
            if visited.contains(&&next.0) {
                return false;
            }
            visited.push(&next.0);
            match (self.rule)(&next.0) {
                Some(next_rule) => current = next_rule,
                None => return false,
            }
        }
        false
    }

    /// The anonymous style `symbols()` defines (css-counter-styles-3 §6.1): no prefix
    /// and a space suffix.
    fn anonymous(ty: SymbolsType, symbols: &[Symbol]) -> Definition<'_> {
        let symbols = Symbols::List(symbols);
        let algorithm = match ty {
            SymbolsType::Cyclic => Algorithm::Cyclic(symbols),
            SymbolsType::Numeric => Algorithm::Numeric(symbols),
            SymbolsType::Alphabetic => Algorithm::Alphabetic(symbols),
            SymbolsType::Symbolic => Algorithm::Symbolic(symbols),
            SymbolsType::Fixed => Algorithm::Fixed(1, symbols),
        };
        Definition {
            suffix: " ",
            ..Definition::new(algorithm)
        }
    }

    /// The text `counter(name, style)` shows for `value` (css-lists-3 §4.5): the
    /// representation alone, without prefix or suffix. `none` shows nothing.
    pub(crate) fn counter_text(&self, style: &CounterStyle, value: i32) -> String {
        match style {
            CounterStyle::None => String::new(),
            CounterStyle::Name(name) => self.named(&name.0).represent(self, value),
            CounterStyle::Symbols { ty, symbols } => {
                Self::anonymous(*ty, &symbols.0).represent(self, value)
            }
            CounterStyle::String(string) => string.to_string(),
        }
    }

    /// The list marker `list-style-type` shows for a list item whose `list-item`
    /// counter is `value` (css-lists-3 §3.1): prefix, representation and suffix, or
    /// the string itself. The bullet styles, whose names no `@counter-style` rule can
    /// take, keep a bare character so they can use the bullet font.
    pub(crate) fn marker(&self, style: &CounterStyle, value: i32) -> Option<Marker> {
        Some(match style {
            CounterStyle::None => return None,
            CounterStyle::Name(name) if style.is_bullet() => {
                Marker::Char(self.named(&name.0).represent(self, value).chars().next()?)
            }
            CounterStyle::Name(name) => {
                Marker::String(self.named(&name.0).marker_text(self, value))
            }
            CounterStyle::Symbols { ty, symbols } => {
                Marker::String(Self::anonymous(*ty, &symbols.0).marker_text(self, value))
            }
            CounterStyle::String(string) => Marker::String(string.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use style::context::QuirksMode;
    use style::media_queries::MediaList;
    use style::servo_arc::Arc;
    use style::shared_lock::{Locked, SharedRwLock};
    use style::stylesheets::{AllowImportRules, CssRule, Origin, Stylesheet, UrlExtraData};
    use style::{Atom, counter_style::CounterStyle, values::CustomIdent};

    use super::{CounterStyleRule, CounterStyles};
    use crate::node::Marker;

    fn style(name: &str) -> CounterStyle {
        CounterStyle::Name(CustomIdent(Atom::from(name)))
    }

    fn no_rules(_: &Atom) -> Option<&'static CounterStyleRule> {
        None
    }

    fn text(name: &str, value: i32) -> String {
        CounterStyles::new(&no_rules).counter_text(&style(name), value)
    }

    fn marker_string(name: &str, value: i32) -> Option<Marker> {
        CounterStyles::new(&no_rules).marker(&style(name), value)
    }

    /// The `@counter-style` rules of `css` that define a counter style, the last of
    /// each name winning, as stylo's stylist collects them.
    fn rules(css: &str) -> (SharedRwLock, HashMap<Atom, Arc<Locked<CounterStyleRule>>>) {
        let lock = SharedRwLock::new();
        let url = UrlExtraData(Arc::new(url::Url::parse("about:blank").unwrap()));
        let sheet = Stylesheet::from_str(
            css,
            url,
            Origin::Author,
            Arc::new(lock.wrap(MediaList::empty())),
            lock.clone(),
            None,
            None,
            QuirksMode::NoQuirks,
            AllowImportRules::No,
        );
        let mut found = HashMap::new();
        {
            let guard = lock.read();
            for rule in sheet.contents.read_with(&guard).rules(&guard) {
                if let CssRule::CounterStyle(rule) = rule {
                    let read = rule.read_with(&guard);
                    if read.defines_counter_style() {
                        found.insert(read.name().0.clone(), rule.clone());
                    }
                }
            }
        }
        (lock, found)
    }

    /// `counter(n, name)` at each value, with the counter styles of `css`.
    fn author_texts(css: &str, name: &str, values: &[i32]) -> Vec<String> {
        let (lock, found) = rules(css);
        let guard = lock.read();
        let lookup = |name: &Atom| found.get(name).map(|rule| rule.read_with(&guard));
        let styles = CounterStyles::new(&lookup);
        let style = style(name);
        values
            .iter()
            .map(|&value| styles.counter_text(&style, value))
            .collect()
    }

    #[test]
    fn bullets_are_bare_characters() {
        assert_eq!(marker_string("disc", 1), Some(Marker::Char('•')));
        assert_eq!(marker_string("circle", 7), Some(Marker::Char('◦')));
        assert_eq!(marker_string("square", -3), Some(Marker::Char('▪')));
        assert_eq!(marker_string("disclosure-open", 1), Some(Marker::Char('▾')));
        assert_eq!(
            marker_string("disclosure-closed", 1),
            Some(Marker::Char('▸'))
        );
    }

    #[test]
    fn markers_carry_the_style_suffix() {
        assert_eq!(
            marker_string("decimal", 1),
            Some(Marker::String("1. ".into()))
        );
        assert_eq!(
            marker_string("decimal", 2),
            Some(Marker::String("2. ".into()))
        );
        assert_eq!(
            marker_string("lower-alpha", 1),
            Some(Marker::String("a. ".into()))
        );
        assert_eq!(
            marker_string("upper-alpha", 2),
            Some(Marker::String("B. ".into()))
        );
        // css-counter-styles-3 §6.1: the CJK styles end in an ideographic comma.
        assert_eq!(
            marker_string("cjk-decimal", 3),
            Some(Marker::String("三、".into()))
        );
        assert_eq!(
            CounterStyles::new(&no_rules).marker(&CounterStyle::None, 1),
            None
        );
    }

    #[test]
    fn alphabetic_styles_count_without_a_zero() {
        assert_eq!(text("lower-alpha", 26), "z");
        assert_eq!(text("lower-alpha", 27), "aa");
        assert_eq!(text("lower-alpha", 28), "ab");
        assert_eq!(text("upper-alpha", 27), "AA");
        assert_eq!(text("upper-latin", 28), "AB");
        // The alphabetic range starts at 1: 0 and negatives fall back to decimal.
        assert_eq!(text("lower-alpha", 0), "0");
        assert_eq!(text("lower-alpha", -1), "-1");
    }

    #[test]
    fn additive_styles_follow_their_ranges() {
        assert_eq!(text("lower-roman", 1), "i");
        assert_eq!(text("lower-roman", 4), "iv");
        assert_eq!(text("upper-roman", 3999), "MMMCMXCIX");
        // Outside 1..=3999 roman falls back to decimal.
        assert_eq!(text("upper-roman", 4000), "4000");
        assert_eq!(text("upper-roman", 0), "0");
        assert_eq!(text("hebrew", 15), "טו");
        assert_eq!(text("hebrew", 16), "טז");
        assert_eq!(text("hebrew", 1000), "א׳");
        assert_eq!(text("armenian", 12), "ԺԲ");
        assert_eq!(text("lower-armenian", 12), "ժբ");
        assert_eq!(text("georgian", 12), "იბ");
    }

    #[test]
    fn numeric_styles_sign_and_pad() {
        assert_eq!(text("decimal", 0), "0");
        assert_eq!(text("decimal", -12), "-12");
        assert_eq!(text("decimal", i32::MIN), "-2147483648");
        assert_eq!(text("decimal-leading-zero", 5), "05");
        assert_eq!(text("decimal-leading-zero", 12), "12");
        // The negative sign counts towards the pad length (css-counter-styles-3 §3.6).
        assert_eq!(text("decimal-leading-zero", -5), "-5");
        assert_eq!(text("arabic-indic", 12), "١٢");
        assert_eq!(text("tibetan", 12), "༡༢");
        assert_eq!(text("cjk-decimal", 2026), "二〇二六");
    }

    #[test]
    fn unknown_names_fall_back_to_decimal() {
        assert_eq!(text("no-such-style", 12), "12");
    }

    #[test]
    fn chinese_styles_collapse_zeros() {
        assert_eq!(text("simp-chinese-informal", 0), "零");
        assert_eq!(text("simp-chinese-informal", 10), "十");
        assert_eq!(text("simp-chinese-informal", 11), "十一");
        assert_eq!(text("simp-chinese-informal", 20), "二十");
        assert_eq!(text("simp-chinese-informal", 101), "一百零一");
        assert_eq!(text("simp-chinese-informal", 110), "一百一十");
        assert_eq!(text("simp-chinese-informal", 1001), "一千零一");
        assert_eq!(text("simp-chinese-informal", -5), "负五");
        assert_eq!(text("trad-chinese-informal", -5), "負五");
        assert_eq!(text("simp-chinese-formal", 10), "壹拾");
        assert_eq!(text("trad-chinese-formal", 1010), "壹仟零壹拾");
        assert_eq!(text("cjk-ideographic", 12), "十二");
        // The CJK range is -9999..=9999; outside it they fall back to cjk-decimal.
        assert_eq!(text("simp-chinese-informal", 10000), "一〇〇〇〇");
    }

    #[test]
    fn japanese_and_korean_styles_drop_zeros() {
        assert_eq!(text("japanese-informal", 1000), "千");
        assert_eq!(text("japanese-informal", 1111), "千百十一");
        assert_eq!(text("japanese-informal", 105), "百五");
        assert_eq!(text("japanese-informal", 0), "〇");
        assert_eq!(text("japanese-formal", 12), "壱拾弐");
        assert_eq!(text("japanese-formal", -1), "マイナス壱");
        assert_eq!(text("korean-hangul-formal", 12), "일십이");
        assert_eq!(text("korean-hanja-informal", 12), "十二");
        assert_eq!(text("korean-hanja-formal", 12), "壹拾貳");
    }

    #[test]
    fn ethiopic_numeric_groups_pairs_of_digits() {
        assert_eq!(text("ethiopic-numeric", 1), "፩");
        assert_eq!(text("ethiopic-numeric", 12), "፲፪");
        assert_eq!(text("ethiopic-numeric", 100), "፻");
        assert_eq!(text("ethiopic-numeric", 10000), "፼");
        // Range 1.. : 0 falls back to decimal.
        assert_eq!(text("ethiopic-numeric", 0), "0");
    }

    #[test]
    fn none_shows_nothing() {
        assert_eq!(
            CounterStyles::new(&no_rules).counter_text(&CounterStyle::None, 3),
            ""
        );
    }

    /// css-counter-styles-3 §3.1: each system's algorithm over author symbols.
    #[test]
    fn author_systems_follow_their_algorithms() {
        let css = r#"
            @counter-style cyc { system: cyclic; symbols: a b c; }
            @counter-style fix { system: fixed 3; symbols: x y; }
            @counter-style sym { system: symbolic; symbols: "*" "+"; }
            @counter-style alp { system: alphabetic; symbols: a b; }
            @counter-style num { system: numeric; symbols: "0" "1"; }
            @counter-style add { system: additive; additive-symbols: 10 X, 5 V, 1 I; }
        "#;
        let values = [1, 2, 3, 4, 5];
        assert_eq!(author_texts(css, "cyc", &values), ["a", "b", "c", "a", "b"]);
        // `fixed 3` covers 3 and 4 only; the rest falls back to decimal.
        assert_eq!(author_texts(css, "fix", &values), ["1", "2", "x", "y", "5"]);
        assert_eq!(
            author_texts(css, "sym", &values),
            ["*", "+", "**", "++", "***"]
        );
        assert_eq!(
            author_texts(css, "alp", &values),
            ["a", "b", "aa", "ab", "ba"]
        );
        assert_eq!(
            author_texts(css, "num", &values),
            ["1", "10", "11", "100", "101"]
        );
        assert_eq!(author_texts(css, "add", &[11, 4, 0]), ["XI", "IIII", "0"]);
    }

    /// §3.3-§3.7: `negative`, `pad`, `range` and `fallback` of an author style.
    #[test]
    fn author_descriptors_shape_the_representation() {
        let css = r#"
            @counter-style neg { system: numeric; symbols: "0" "1" "2" "3" "4" "5" "6" "7" "8" "9"; negative: "(" ")"; }
            @counter-style pad { system: numeric; symbols: "0" "1" "2" "3" "4" "5" "6" "7" "8" "9"; pad: 3 "0"; }
            @counter-style rng { system: cyclic; symbols: "*"; range: 1 2, 5 infinite; fallback: upper-roman; }
        "#;
        assert_eq!(author_texts(css, "neg", &[-7, 7]), ["(7)", "7"]);
        assert_eq!(
            author_texts(css, "pad", &[1, 12, 1234]),
            ["001", "012", "1234"]
        );
        assert_eq!(
            author_texts(css, "rng", &[1, 2, 3, 4, 5]),
            ["*", "*", "III", "IV", "*"]
        );
    }

    /// §3.1.7 `extends`: the extended algorithm with this rule's descriptors; a loop
    /// of `extends` extends `decimal`; an unknown name is `decimal`.
    #[test]
    fn extends_takes_the_extended_algorithm() {
        let css = r#"
            @counter-style paren-roman { system: extends upper-roman; prefix: "("; negative: "~"; }
            @counter-style a { system: extends b; negative: "a"; }
            @counter-style b { system: extends a; negative: "b"; }
            @counter-style c { system: extends nothing-here; pad: 2 "0"; }
        "#;
        assert_eq!(author_texts(css, "paren-roman", &[4]), ["IV"]);
        // upper-roman's range 1..=3999 carries over, so 0 falls back to decimal.
        assert_eq!(author_texts(css, "paren-roman", &[0]), ["0"]);
        assert_eq!(author_texts(css, "a", &[-3, 12]), ["a3", "12"]);
        assert_eq!(author_texts(css, "b", &[-3]), ["b3"]);
        assert_eq!(author_texts(css, "c", &[3]), ["03"]);
    }

    /// §3.7: a loop of fallbacks ends in decimal.
    #[test]
    fn a_fallback_loop_ends_in_decimal() {
        let css = r#"
            @counter-style p { system: cyclic; symbols: p; range: 1 1; fallback: q; }
            @counter-style q { system: cyclic; symbols: q; range: 2 2; fallback: p; }
        "#;
        assert_eq!(author_texts(css, "p", &[1, 2, 3]), ["p", "q", "3"]);
    }

    /// The marker of an author style carries its prefix and suffix, which a rule
    /// that sets neither takes from the initial `""` and `". "`.
    #[test]
    fn author_markers_carry_prefix_and_suffix() {
        let (lock, found) = rules(
            r#"@counter-style br { system: cyclic; symbols: "*"; prefix: "["; suffix: "] "; }
               @counter-style plain { system: cyclic; symbols: "*"; }"#,
        );
        let guard = lock.read();
        let lookup = |name: &Atom| found.get(name).map(|rule| rule.read_with(&guard));
        let styles = CounterStyles::new(&lookup);
        assert_eq!(
            styles.marker(&style("br"), 1),
            Some(Marker::String("[*] ".into()))
        );
        assert_eq!(
            styles.marker(&style("plain"), 1),
            Some(Marker::String("*. ".into()))
        );
    }
}
