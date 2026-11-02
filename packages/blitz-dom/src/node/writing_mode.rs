//! Writing modes (css-writing-modes-4) as layout and paint use them: the frame a box lays
//! its content out in, which characters a vertical line sets upright, and which inline
//! boxes combine their text into one upright cell (`text-combine-upright: all`).

use icu_properties::CodePointMapData;
use icu_properties::props::VerticalOrientation;
use style::computed_values::text_combine_upright::T as TextCombineUpright;
use style::computed_values::text_orientation::T as TextOrientation;
use style::computed_values::writing_mode::T as WritingMode;
use style::properties::ComputedValues;
use style::values::specified::box_::{DisplayInside, DisplayOutside};

/// The writing mode a box lays its content out in. In a vertical flow, layout runs in the
/// box's logical frame (x the inline axis, y the block axis) and the result is mapped to
/// physical coordinates afterwards; glyph runs are rotated a quarter turn at paint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LayoutFlow {
    #[default]
    HorizontalTb,
    VerticalRl,
    VerticalLr,
    SidewaysRl,
    SidewaysLr,
    /// A `text-combine-upright: all` inline in a vertical line: its text is laid out as
    /// `horizontal-tb`, and the box takes one em of the line, which paint scales it into.
    CombinedText,
}

impl LayoutFlow {
    /// The flow of a box with `style`.
    pub fn of(style: &ComputedValues, is_text_combined: bool) -> Self {
        if is_text_combined {
            return Self::CombinedText;
        }
        match style.clone_writing_mode() {
            WritingMode::HorizontalTb => Self::HorizontalTb,
            WritingMode::VerticalRl => Self::VerticalRl,
            WritingMode::VerticalLr => Self::VerticalLr,
            WritingMode::SidewaysRl => Self::SidewaysRl,
            WritingMode::SidewaysLr => Self::SidewaysLr,
        }
    }

    pub fn is_vertical(self) -> bool {
        matches!(
            self,
            Self::VerticalRl | Self::VerticalLr | Self::SidewaysRl | Self::SidewaysLr
        )
    }

    /// Whether lines stack left to right (`vertical-lr`, `sideways-lr`).
    pub fn is_block_left_to_right(self) -> bool {
        matches!(self, Self::VerticalLr | Self::SidewaysLr)
    }

    /// Whether the inline axis runs bottom to top (`sideways-lr`): its glyphs turn
    /// counter-clockwise, every other vertical flow's clockwise.
    pub fn is_inline_bottom_to_top(self) -> bool {
        self == Self::SidewaysLr
    }
}

/// Whether `ch` stands upright in a line of `flow` under `orientation` (css-writing-modes-4
/// §5.1): `mixed` sets the characters UAX #50 gives `U` or `Tu` upright, `upright` every
/// character, `sideways` none; `sideways-*` writing modes set every character sideways.
pub fn is_upright(flow: LayoutFlow, orientation: TextOrientation, ch: char) -> bool {
    match flow {
        LayoutFlow::VerticalRl | LayoutFlow::VerticalLr => match orientation {
            TextOrientation::Mixed => matches!(
                CodePointMapData::<VerticalOrientation>::new().get(ch),
                VerticalOrientation::Upright | VerticalOrientation::TransformedUpright
            ),
            TextOrientation::Upright => true,
            TextOrientation::Sideways => false,
        },
        LayoutFlow::HorizontalTb
        | LayoutFlow::SidewaysRl
        | LayoutFlow::SidewaysLr
        | LayoutFlow::CombinedText => false,
    }
}

/// Whether an inline box with `style`, inside a parent with `parent_style`, sets its text
/// as one upright cell (css-writing-modes-4 §9.1): `text-combine-upright: all` in a
/// `vertical-rl`/`vertical-lr` line. Only the outermost such inline combines, so the cell
/// holds the whole run.
pub(crate) fn combines_text(style: &ComputedValues, parent_style: Option<&ComputedValues>) -> bool {
    let display = style.clone_display();
    style.clone_text_combine_upright() == TextCombineUpright::All
        && matches!(
            style.clone_writing_mode(),
            WritingMode::VerticalRl | WritingMode::VerticalLr
        )
        && display.outside() == DisplayOutside::Inline
        && display.inside() == DisplayInside::Flow
        && parent_style
            .is_none_or(|parent| parent.clone_text_combine_upright() == TextCombineUpright::None)
}
