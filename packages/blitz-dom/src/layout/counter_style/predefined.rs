//! The predefined counter styles of css-counter-styles-3 §6 (simple) and §7 (complex),
//! with the symbols, ranges, signs and suffixes the spec gives them.

use super::complex::Style;
use super::{AdditiveSymbols, Algorithm, Definition, Range, Symbols};

const fn numeric(digits: &'static str) -> Definition<'static> {
    Definition::new(Algorithm::Numeric(Symbols::Chars(digits)))
}

const fn alphabetic(letters: &'static str) -> Definition<'static> {
    Definition::new(Algorithm::Alphabetic(Symbols::Chars(letters)))
}

const fn additive(
    tuples: &'static [(u32, &'static str)],
    min: i32,
    max: i32,
) -> Definition<'static> {
    Definition {
        range: Range::Bounds(min, max),
        ..Definition::new(Algorithm::Additive(AdditiveSymbols::Table(tuples)))
    }
}

/// `disc`, `circle`, `square` and the disclosure triangles: one cyclic symbol and a
/// space suffix.
const fn bullet(symbol: &'static str) -> Definition<'static> {
    Definition {
        suffix: " ",
        ..Definition::new(Algorithm::Cyclic(Symbols::Chars(symbol)))
    }
}

/// The East Asian styles end in an ideographic comma.
const fn ideographic(definition: Definition<'static>) -> Definition<'static> {
    Definition {
        suffix: "\u{3001}",
        ..definition
    }
}

/// The §7.1 styles: range -9999..=9999, falling back to `cjk-decimal`.
const fn cjk(style: Style, negative: &'static str, suffix: &'static str) -> Definition<'static> {
    Definition {
        range: Range::Bounds(-9999, 9999),
        negative: (negative, ""),
        suffix,
        fallback: "cjk-decimal",
        ..Definition::new(Algorithm::Complex(style))
    }
}

const DECIMAL: &str = "0123456789";
const CJK_DECIMAL: &str = "〇一二三四五六七八九";
const LOWER_LATIN: &str = "abcdefghijklmnopqrstuvwxyz";
const UPPER_LATIN: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const KHMER: &str = "០១២៣៤៥៦៧៨៩";

const LOWER_ROMAN: &[(u32, &str)] = &[
    (1000, "m"),
    (900, "cm"),
    (500, "d"),
    (400, "cd"),
    (100, "c"),
    (90, "xc"),
    (50, "l"),
    (40, "xl"),
    (10, "x"),
    (9, "ix"),
    (5, "v"),
    (4, "iv"),
    (1, "i"),
];
const UPPER_ROMAN: &[(u32, &str)] = &[
    (1000, "M"),
    (900, "CM"),
    (500, "D"),
    (400, "CD"),
    (100, "C"),
    (90, "XC"),
    (50, "L"),
    (40, "XL"),
    (10, "X"),
    (9, "IX"),
    (5, "V"),
    (4, "IV"),
    (1, "I"),
];
#[rustfmt::skip]
const UPPER_ARMENIAN: &[(u32, &str)] = &[
    (9000, "Ք"), (8000, "Փ"), (7000, "Ւ"), (6000, "Ց"), (5000, "Ր"), (4000, "Տ"), (3000, "Վ"),
    (2000, "Ս"), (1000, "Ռ"), (900, "Ջ"), (800, "Պ"), (700, "Չ"), (600, "Ո"), (500, "Շ"),
    (400, "Ն"), (300, "Յ"), (200, "Մ"), (100, "Ճ"), (90, "Ղ"), (80, "Ձ"), (70, "Հ"), (60, "Կ"),
    (50, "Ծ"), (40, "Խ"), (30, "Լ"), (20, "Ի"), (10, "Ժ"), (9, "Թ"), (8, "Ը"), (7, "Է"),
    (6, "Զ"), (5, "Ե"), (4, "Դ"), (3, "Գ"), (2, "Բ"), (1, "Ա"),
];
#[rustfmt::skip]
const LOWER_ARMENIAN: &[(u32, &str)] = &[
    (9000, "ք"), (8000, "փ"), (7000, "ւ"), (6000, "ց"), (5000, "ր"), (4000, "տ"), (3000, "վ"),
    (2000, "ս"), (1000, "ռ"), (900, "ջ"), (800, "պ"), (700, "չ"), (600, "ո"), (500, "շ"),
    (400, "ն"), (300, "յ"), (200, "մ"), (100, "ճ"), (90, "ղ"), (80, "ձ"), (70, "հ"), (60, "կ"),
    (50, "ծ"), (40, "խ"), (30, "լ"), (20, "ի"), (10, "ժ"), (9, "թ"), (8, "ը"), (7, "է"),
    (6, "զ"), (5, "ե"), (4, "դ"), (3, "գ"), (2, "բ"), (1, "ա"),
];
#[rustfmt::skip]
const GEORGIAN: &[(u32, &str)] = &[
    (10000, "ჵ"), (9000, "ჰ"), (8000, "ჯ"), (7000, "ჴ"), (6000, "ხ"), (5000, "ჭ"), (4000, "წ"),
    (3000, "ძ"), (2000, "ც"), (1000, "ჩ"), (900, "შ"), (800, "ყ"), (700, "ღ"), (600, "ქ"),
    (500, "ფ"), (400, "ჳ"), (300, "ტ"), (200, "ს"), (100, "რ"), (90, "ჟ"), (80, "პ"), (70, "ო"),
    (60, "ჲ"), (50, "ნ"), (40, "მ"), (30, "ლ"), (20, "კ"), (10, "ი"), (9, "თ"), (8, "ჱ"),
    (7, "ზ"), (6, "ვ"), (5, "ე"), (4, "დ"), (3, "გ"), (2, "ბ"), (1, "ა"),
];
#[rustfmt::skip]
const HEBREW: &[(u32, &str)] = &[
    (10000, "י׳"), (9000, "ט׳"), (8000, "ח׳"), (7000, "ז׳"), (6000, "ו׳"), (5000, "ה׳"),
    (4000, "ד׳"), (3000, "ג׳"), (2000, "ב׳"), (1000, "א׳"), (400, "ת"), (300, "ש"), (200, "ר"),
    (100, "ק"), (90, "צ"), (80, "פ"), (70, "ע"), (60, "ס"), (50, "נ"), (40, "מ"), (30, "ל"),
    (20, "כ"), (19, "יט"), (18, "יח"), (17, "יז"), (16, "טז"), (15, "טו"), (10, "י"), (9, "ט"),
    (8, "ח"), (7, "ז"), (6, "ו"), (5, "ה"), (4, "ד"), (3, "ג"), (2, "ב"), (1, "א"),
];

/// The predefined style `name` names, which stylo has already lower-cased.
pub(super) fn lookup(name: &str) -> Option<Definition<'static>> {
    Some(match name {
        "decimal" => numeric(DECIMAL),
        "decimal-leading-zero" => Definition {
            pad: (2, "0"),
            ..numeric(DECIMAL)
        },
        "arabic-indic" => numeric("٠١٢٣٤٥٦٧٨٩"),
        "armenian" | "upper-armenian" => additive(UPPER_ARMENIAN, 1, 9999),
        "lower-armenian" => additive(LOWER_ARMENIAN, 1, 9999),
        "bengali" => numeric("০১২৩৪৫৬৭৮৯"),
        "cambodian" | "khmer" => numeric(KHMER),
        "cjk-decimal" => ideographic(numeric(CJK_DECIMAL)),
        "devanagari" => numeric("०१२३४५६७८९"),
        "georgian" => additive(GEORGIAN, 1, 19999),
        "gujarati" => numeric("૦૧૨૩૪૫૬૭૮૯"),
        "gurmukhi" => numeric("੦੧੨੩੪੫੬੭੮੯"),
        "hebrew" => additive(HEBREW, 1, 10999),
        "kannada" => numeric("೦೧೨೩೪೫೬೭೮೯"),
        "lao" => numeric("໐໑໒໓໔໕໖໗໘໙"),
        "malayalam" => numeric("൦൧൨൩൪൫൬൭൮൯"),
        "mongolian" => numeric("᠐᠑᠒᠓᠔᠕᠖᠗᠘᠙"),
        "myanmar" => numeric("၀၁၂၃၄၅၆၇၈၉"),
        "oriya" => numeric("୦୧୨୩୪୫୬୭୮୯"),
        "persian" => numeric("۰۱۲۳۴۵۶۷۸۹"),
        "lower-roman" => additive(LOWER_ROMAN, 1, 3999),
        "upper-roman" => additive(UPPER_ROMAN, 1, 3999),
        "tamil" => numeric("௦௧௨௩௪௫௬௭௮௯"),
        "telugu" => numeric("౦౧౨౩౪౫౬౭౮౯"),
        "thai" => numeric("๐๑๒๓๔๕๖๗๘๙"),
        "tibetan" => numeric("༠༡༢༣༤༥༦༧༨༩"),
        "lower-alpha" | "lower-latin" => alphabetic(LOWER_LATIN),
        "upper-alpha" | "upper-latin" => alphabetic(UPPER_LATIN),
        "lower-greek" => alphabetic("αβγδεζηθικλμνξοπρστυφχψω"),
        "hiragana" => ideographic(alphabetic(
            "あいうえおかきくけこさしすせそたちつてとなにぬねのはひふへほまみむめもやゆよらりるれろわゐゑをん",
        )),
        "hiragana-iroha" => ideographic(alphabetic(
            "いろはにほへとちりぬるをわかよたれそつねならむうゐのおくやまけふこえてあさきゆめみしゑひもせす",
        )),
        "katakana" => ideographic(alphabetic(
            "アイウエオカキクケコサシスセソタチツテトナニヌネノハヒフヘホマミムメモヤユヨラリルレロワヰヱヲン",
        )),
        "katakana-iroha" => ideographic(alphabetic(
            "イロハニホヘトチリヌルヲワカヨタレソツネナラムウヰノオクヤマケフコエテアサキユメミシヱヒモセス",
        )),
        "cjk-earthly-branch" => ideographic(alphabetic("子丑寅卯辰巳午未申酉戌亥")),
        "cjk-heavenly-stem" => ideographic(alphabetic("甲乙丙丁戊己庚辛壬癸")),
        "disc" => bullet("•"),
        "circle" => bullet("◦"),
        "square" => bullet("▪"),
        "disclosure-open" => bullet("▾"),
        "disclosure-closed" => bullet("▸"),
        "japanese-informal" => cjk(Style::JapaneseInformal, "マイナス", "\u{3001}"),
        "japanese-formal" => cjk(Style::JapaneseFormal, "マイナス", "\u{3001}"),
        "korean-hangul-formal" => cjk(Style::KoreanHangulFormal, "마이너스 ", ", "),
        "korean-hanja-informal" => cjk(Style::KoreanHanjaInformal, "마이너스 ", ", "),
        "korean-hanja-formal" => cjk(Style::KoreanHanjaFormal, "마이너스 ", ", "),
        "simp-chinese-informal" => cjk(Style::SimpChineseInformal, "负", "\u{3001}"),
        "simp-chinese-formal" => cjk(Style::SimpChineseFormal, "负", "\u{3001}"),
        "trad-chinese-informal" | "cjk-ideographic" => {
            cjk(Style::TradChineseInformal, "負", "\u{3001}")
        }
        "trad-chinese-formal" => cjk(Style::TradChineseFormal, "負", "\u{3001}"),
        "ethiopic-numeric" => Definition {
            suffix: "/ ",
            ..Definition::new(Algorithm::Complex(Style::EthiopicNumeric))
        },
        _ => return None,
    })
}
