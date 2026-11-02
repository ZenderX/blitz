//! `font-palette` (css-fonts-4 §9): the CPAL palette, and the `override-colors`, that
//! a run's COLR glyphs are painted with.
//!
//! `PaintScene::draw_glyphs` carries no palette, and a scene may be recorded and
//! replayed later, so the palette travels with the font: a run with a palette other
//! than the font's default draws with a font of its own (the same bytes under a new
//! blob id), and [`font_palette`] answers that font's palette to the `PaintScene` that
//! rasterizes it. Two recordings that differ only in a palette then differ in a font,
//! and compare unequal.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use blitz_dom::BaseDocument;
use color::{AlphaColor, Srgb};
use peniko::{Blob, FontData, WeakBlob};
use skrifa::raw::tables::cpal::{Cpal, PaletteType};
use skrifa::raw::{FontRef, TableProvider as _};
use style::Atom;
use style::properties::ComputedValues;
use style::stylesheets::FontPaletteValuesRule;
use style::stylesheets::font_palette_values_rule::FontPaletteBase;
use style::values::computed::font::SingleFontFamily;

use crate::color::ToColorColor as _;

/// A COLR palette selection: the CPAL palette, and the entries drawn in another color,
/// as `(entry index, color)` with a later override of an entry winning.
#[derive(Clone, Debug, PartialEq)]
pub struct FontPalette {
    pub index: u16,
    pub overrides: Vec<(u16, AlphaColor<Srgb>)>,
}

/// A font standing for another font painted with a palette.
struct PaletteFont {
    /// The blob id and face index of the font it stands for.
    source: (u64, u32),
    font: WeakBlob<u8>,
    palette: Arc<FontPalette>,
}

/// The live palette fonts, by blob id.
static PALETTE_FONTS: LazyLock<Mutex<HashMap<u64, PaletteFont>>> = LazyLock::new(Default::default);

/// The palette `font` was made to paint with by [`font_with_palette`], `None` for any
/// other font.
pub fn font_palette(font: &FontData) -> Option<Arc<FontPalette>> {
    let fonts = PALETTE_FONTS.lock().unwrap();
    fonts
        .get(&font.data.id())
        .map(|entry| entry.palette.clone())
}

/// `font` under the blob id that stands for `palette`: the same one for as long as a
/// scene holds it.
pub(crate) fn font_with_palette(font: &FontData, palette: FontPalette) -> FontData {
    let source = (font.data.id(), font.index);
    let mut fonts = PALETTE_FONTS.lock().unwrap();
    let existing = fonts
        .values()
        .filter(|entry| entry.source == source && *entry.palette == palette)
        .find_map(|entry| entry.font.upgrade());
    if let Some(blob) = existing {
        return FontData::new(blob, font.index);
    }
    fonts.retain(|_, entry| entry.font.upgrade().is_some());
    let blob = Blob::new(Arc::new(font.data.clone()));
    fonts.insert(
        blob.id(),
        PaletteFont {
            source,
            font: blob.downgrade(),
            palette: Arc::new(palette),
        },
    );
    FontData::new(blob, font.index)
}

/// The palette `style`'s `font-palette` selects in `font`, `None` for the font's
/// default palette or a font without palettes.
pub(crate) fn resolve(
    doc: &BaseDocument,
    style: &ComputedValues,
    font: &FontData,
) -> Option<FontPalette> {
    let value = style.clone_font_palette();
    let value = value.as_atom();
    if &**value == "normal" {
        return None;
    }
    let font_ref = FontRef::from_index(font.data.data(), font.index).ok()?;
    let cpal = font_ref.cpal().ok()?;
    let typed = |usage| {
        Some(FontPalette {
            index: typed_palette(&cpal, usage)?,
            overrides: Vec::new(),
        })
    };
    let palette = match &**value {
        "light" => typed(PaletteType::USABLE_WITH_LIGHT_BACKGROUND)?,
        "dark" => typed(PaletteType::USABLE_WITH_DARK_BACKGROUND)?,
        _ => palette_of_rule(&cpal, palette_values_rule(doc, style, value)?),
    };
    (palette.index != 0 || !palette.overrides.is_empty()).then_some(palette)
}

/// The `@font-palette-values` rule named `name` for the first family of `style`'s
/// `font-family` that one names; the last such rule wins.
fn palette_values_rule<'a>(
    doc: &'a BaseDocument,
    style: &ComputedValues,
    name: &Atom,
) -> Option<&'a FontPaletteValuesRule> {
    style
        .get_font()
        .font_family
        .families
        .iter()
        .find_map(|family| {
            let SingleFontFamily::FamilyName(family) = family else {
                return None;
            };
            doc.font_palette_values()
                .filter(|rule| rule.name.0 == *name)
                .filter(|rule| {
                    rule.family_names
                        .iter()
                        .any(|named| named.name.eq_ignore_ascii_case(&family.name))
                })
                .last()
        })
}

/// The first palette the font marks with `usage`, `None` when it marks none.
fn typed_palette(cpal: &Cpal<'_>, usage: PaletteType) -> Option<u16> {
    let types = cpal.palette_types_array()?.ok()?;
    let index = types.iter().position(|kind| kind.get().contains(usage))?;
    u16::try_from(index).ok()
}

/// The palette `rule`'s `base-palette` and `override-colors` select.
fn palette_of_rule(cpal: &Cpal<'_>, rule: &FontPaletteValuesRule) -> FontPalette {
    let index = match &rule.base_palette {
        Some(FontPaletteBase::Light) => {
            typed_palette(cpal, PaletteType::USABLE_WITH_LIGHT_BACKGROUND)
        }
        Some(FontPaletteBase::Dark) => {
            typed_palette(cpal, PaletteType::USABLE_WITH_DARK_BACKGROUND)
        }
        Some(FontPaletteBase::Index(index)) => index
            .0
            .resolve()
            .and_then(|index| u16::try_from(index).ok())
            .filter(|&index| index < cpal.num_palettes()),
        None => None,
    }
    .unwrap_or(0);
    // An override of an entry the palettes do not have is ignored.
    let overrides = rule
        .override_colors
        .iter()
        .filter_map(|color| {
            let entry = u16::try_from(color.index()).ok()?;
            (entry < cpal.num_palette_entries())
                .then(|| (entry, color.absolute_color().as_srgb_color()))
        })
        .collect();
    FontPalette { index, overrides }
}
