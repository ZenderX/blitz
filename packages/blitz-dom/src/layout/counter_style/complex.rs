//! The complex predefined counter styles (css-counter-styles-3 §7), whose algorithms
//! no `system` descriptor can express: the Chinese, Japanese and Korean longhand
//! styles (§7.1) and `ethiopic-numeric` (§7.2).

/// One complex predefined style.
#[derive(Clone, Copy)]
pub(super) enum Style {
    JapaneseInformal,
    JapaneseFormal,
    KoreanHangulFormal,
    KoreanHanjaInformal,
    KoreanHanjaFormal,
    SimpChineseInformal,
    SimpChineseFormal,
    TradChineseInformal,
    TradChineseFormal,
    EthiopicNumeric,
}

/// How a CJK style writes the digits of 1..=9999.
struct Cjk {
    /// The digits 0 to 9.
    digits: [&'static str; 10],
    /// The markers for 10, 100 and 1000.
    markers: [&'static str; 3],
    /// Which "one" digits before a marker are dropped.
    drop_one: DropOne,
    /// Chinese writes a single zero digit for each run of inner zeros; Japanese and
    /// Korean drop zeros.
    keeps_inner_zero: bool,
}

#[derive(PartialEq)]
enum DropOne {
    Never,
    /// Only the tens "one" of 10..=19 (the Chinese informal styles).
    Teens,
    /// Every "one" before a marker (the Japanese and Korean informal styles).
    BeforeMarkers,
}

impl Style {
    pub(super) fn range(self) -> (i32, i32) {
        match self {
            Style::EthiopicNumeric => (1, i32::MAX),
            _ => (-9999, 9999),
        }
    }

    /// The representation of `value`, which the range keeps at 1.. for
    /// `ethiopic-numeric` and at 0..=9999 (the absolute value) for the CJK styles.
    pub(super) fn generate(self, value: i64) -> Option<String> {
        let value = u64::try_from(value).ok()?;
        let cjk = match self {
            Style::EthiopicNumeric => return ethiopic(value),
            Style::JapaneseInformal => Cjk {
                digits: ["〇", "一", "二", "三", "四", "五", "六", "七", "八", "九"],
                markers: ["十", "百", "千"],
                drop_one: DropOne::BeforeMarkers,
                keeps_inner_zero: false,
            },
            Style::JapaneseFormal => Cjk {
                digits: ["零", "壱", "弐", "参", "四", "伍", "六", "七", "八", "九"],
                markers: ["拾", "百", "阡"],
                drop_one: DropOne::Never,
                keeps_inner_zero: false,
            },
            Style::KoreanHangulFormal => Cjk {
                digits: ["영", "일", "이", "삼", "사", "오", "육", "칠", "팔", "구"],
                markers: ["십", "백", "천"],
                drop_one: DropOne::Never,
                keeps_inner_zero: false,
            },
            Style::KoreanHanjaInformal => Cjk {
                digits: ["零", "一", "二", "三", "四", "五", "六", "七", "八", "九"],
                markers: ["十", "百", "千"],
                drop_one: DropOne::BeforeMarkers,
                keeps_inner_zero: false,
            },
            Style::KoreanHanjaFormal => Cjk {
                digits: ["零", "壹", "貳", "參", "四", "五", "六", "七", "八", "九"],
                markers: ["拾", "百", "仟"],
                drop_one: DropOne::Never,
                keeps_inner_zero: false,
            },
            Style::SimpChineseInformal | Style::TradChineseInformal => Cjk {
                digits: ["零", "一", "二", "三", "四", "五", "六", "七", "八", "九"],
                markers: ["十", "百", "千"],
                drop_one: DropOne::Teens,
                keeps_inner_zero: true,
            },
            Style::SimpChineseFormal => Cjk {
                digits: ["零", "壹", "贰", "叁", "肆", "伍", "陆", "柒", "捌", "玖"],
                markers: ["拾", "佰", "仟"],
                drop_one: DropOne::Never,
                keeps_inner_zero: true,
            },
            Style::TradChineseFormal => Cjk {
                digits: ["零", "壹", "貳", "參", "肆", "伍", "陸", "柒", "捌", "玖"],
                markers: ["拾", "佰", "仟"],
                drop_one: DropOne::Never,
                keeps_inner_zero: true,
            },
        };
        cjk.generate(value)
    }
}

impl Cjk {
    fn generate(&self, value: u64) -> Option<String> {
        if value > 9999 {
            return None;
        }
        if value == 0 {
            return Some(self.digits[0].to_owned());
        }
        let mut out = String::new();
        let mut pending_zero = false;
        let mut started = false;
        for place in (0..4).rev() {
            let digit = (value / 10u64.pow(place)) % 10;
            if digit == 0 {
                // Leading and trailing zeros are never written.
                pending_zero |= started;
                continue;
            }
            if pending_zero && self.keeps_inner_zero {
                out.push_str(self.digits[0]);
            }
            pending_zero = false;
            started = true;
            let drops_one = digit == 1
                && place > 0
                && match self.drop_one {
                    DropOne::Never => false,
                    DropOne::Teens => place == 1 && value < 20,
                    DropOne::BeforeMarkers => true,
                };
            if !drops_one {
                out.push_str(self.digits[digit as usize]);
            }
            if place > 0 {
                out.push_str(self.markers[place as usize - 1]);
            }
        }
        Some(out)
    }
}

/// css-counter-styles-3 §7.2: base-100 groups, written least significant last, with
/// ፻ after each odd group and ፼ after each even group but the last.
fn ethiopic(value: u64) -> Option<String> {
    const ONES: [&str; 10] = ["", "፩", "፪", "፫", "፬", "፭", "፮", "፯", "፰", "፱"];
    const TENS: [&str; 10] = ["", "፲", "፳", "፴", "፵", "፶", "፷", "፸", "፹", "፺"];
    if value == 0 {
        return None;
    }
    if value == 1 {
        return Some(ONES[1].to_owned());
    }
    let mut groups = Vec::new();
    let mut rest = value;
    while rest != 0 {
        groups.push(rest % 100);
        rest /= 100;
    }
    let most_significant = groups.len() - 1;
    let mut out = String::new();
    for (index, &group) in groups.iter().enumerate().rev() {
        let is_odd = index % 2 == 1;
        let drops_digits = group == 0 || (group == 1 && (index == most_significant || is_odd));
        if !drops_digits {
            out.push_str(TENS[(group / 10) as usize]);
            out.push_str(ONES[(group % 10) as usize]);
        }
        if is_odd && group != 0 {
            out.push('፻');
        } else if !is_odd && index != 0 {
            out.push('፼');
        }
    }
    Some(out)
}
