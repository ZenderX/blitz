use super::ElementCx;
use crate::color::{Color, ToColorColor as _};
use anyrender::PaintScene;
use kurbo::{Insets, Rect, Vec2};
use peniko::{Compose, Fill, Mix};

impl ElementCx<'_, '_> {
    pub(super) fn draw_outset_box_shadow(&self, scene: &mut impl PaintScene) {
        let box_shadow = &self.style.get_effects().box_shadow.0;
        let has_outset_shadow = box_shadow.iter().any(|s| !s.inset);
        if !has_outset_shadow {
            return;
        }

        let current_color = self.style.clone_color();
        let opacity = self.style.get_effects().opacity;
        let bg_color = self
            .style
            .get_background()
            .background_color
            .resolve_to_absolute(&current_color)
            .as_srgb_color();
        let bg_is_opaque = bg_color.components[3] >= 1.0;
        let needs_clip = opacity < 1.0 || !bg_is_opaque;

        let max_shadow_rect = box_shadow.iter().fold(Rect::ZERO, |prev, shadow| {
            let x = shadow.base.horizontal.px() as f64 * self.scale;
            let y = shadow.base.vertical.px() as f64 * self.scale;
            let blur = shadow.base.blur.px() as f64 * self.scale;
            let spread = shadow.spread.px() as f64 * self.scale;
            let offset = spread + blur * 2.5;

            let rect = self.frame.border_box.inflate(offset, offset) + Vec2::new(x, y);

            prev.union(rect)
        });

        self.context.layer_manager.maybe_with_layer(
            scene,
            needs_clip,
            1.0,
            self.transform,
            &self.frame.shadow_clip(max_shadow_rect),
            None,
            None,
            |scene| {
                for shadow in box_shadow.iter().filter(|s| !s.inset).rev() {
                    let shadow_color = shadow
                        .base
                        .color
                        .resolve_to_absolute(&current_color)
                        .as_srgb_color();

                    let alpha = shadow_color.components[3];
                    if alpha != 0.0 {
                        let transform = self.transform.then_translate(Vec2 {
                            x: shadow.base.horizontal.px() as f64 * self.scale,
                            y: shadow.base.vertical.px() as f64 * self.scale,
                        });

                        let (rect, radius) = outset_shadow_shape(
                            self.frame.border_box,
                            // TODO draw shadows with matching individual radii instead of averaging
                            self.frame.border_radii.average(),
                            shadow.spread.px() as f64 * self.scale,
                        );

                        // Fill the color
                        scene.draw_box_shadow(
                            transform,
                            rect,
                            shadow_color,
                            radius,
                            shadow.base.blur.px() as f64 * self.scale,
                        );
                    }
                }
            },
        )
    }

    pub(super) fn draw_inset_box_shadow(&self, scene: &mut impl PaintScene) {
        let current_color = self.style.clone_color();
        let box_shadow = &self.style.get_effects().box_shadow.0;
        let has_inset_shadow = box_shadow.iter().any(|s| s.inset);
        if !has_inset_shadow {
            return;
        }

        let padding_box = self.frame.padding_box_path();

        for shadow in box_shadow.iter().filter(|s| s.inset) {
            let shadow_color = shadow
                .base
                .color
                .resolve_to_absolute(&current_color)
                .as_srgb_color();
            // A transparent shadow paints nothing, but later shadows in the list still do
            if shadow_color == Color::TRANSPARENT {
                continue;
            }

            let (x, y) = inset_shadow_offset(
                shadow.base.horizontal.px(),
                shadow.base.vertical.px(),
                self.scale,
            );
            let transform = self.transform.then_translate(Vec2 { x, y });
            let (hole, radius) = inset_shadow_hole(
                self.frame.padding_box,
                // TODO draw shadows with matching individual radii instead of averaging
                inner_radius(self.frame.border_radii.average(), &self.frame.border_width),
                shadow.spread.px() as f64 * self.scale,
            );

            scene.push_layer(Mix::Normal, 1.0, self.transform, &padding_box, None, None);
            scene.fill(
                Fill::NonZero,
                self.transform,
                shadow_color,
                None,
                &padding_box,
            );

            scene.push_layer(
                Compose::DestOut,
                1.0,
                self.transform,
                &padding_box,
                None,
                None,
            );
            scene.draw_box_shadow(
                transform,
                hole,
                Color::WHITE,
                radius,
                shadow.base.blur.px() as f64 * self.scale,
            );

            scene.pop_layer();
            scene.pop_layer();
        }
    }
}

/// Device-pixel offset of a shadow given its CSS-px offsets.
fn inset_shadow_offset(horizontal: f32, vertical: f32, scale: f64) -> (f64, f64) {
    (horizontal as f64 * scale, vertical as f64 * scale)
}

/// A corner radius grown by `grow` (> 0) device px.
///
/// CSS Backgrounds 3, "Shadow Shape, Spread, and Knockout": when the radius is less than
/// the distance, the distance is first multiplied by `1 + (r - 1)^3` with
/// `r = radius / distance`, so a square corner stays sharp.
fn grown_radius(radius: f64, grow: f64) -> f64 {
    if radius >= grow {
        radius + grow
    } else {
        let r = radius / grow;
        radius + grow * (1.0 + (r - 1.0).powi(3))
    }
}

/// The rect and corner radius of an outset shadow: the border box grown by the spread,
/// with the radius adjusted per CSS Backgrounds 3 "Shadow Shape, Spread, and Knockout"
/// (see `grown_radius`), floored at 0.
fn outset_shadow_shape(border_box: Rect, radius: f64, spread: f64) -> (Rect, f64) {
    let radius = if spread > 0.0 {
        grown_radius(radius, spread)
    } else {
        (radius + spread).max(0.0)
    };
    (border_box.inflate(spread, spread), radius)
}

/// The padding-box (inner) corner radius: the border-box radius reduced by the average
/// border width, floored at 0.
fn inner_radius(border_radius: f64, border_width: &Insets) -> f64 {
    let avg_width = (border_width.x0 + border_width.y0 + border_width.x1 + border_width.y1) / 4.0;
    (border_radius - avg_width).max(0.0)
}

/// The rect (and corner radius) of the unshadowed area inside an inset shadow.
///
/// CSS Backgrounds 3 section 7.2.2: the inset shadow is the padding box with a hole
/// cut out of it, and the hole is the padding box shrunk by the spread distance (grown
/// for a negative spread). A shrinking hole's radius is reduced by the spread; a growing
/// one follows "Shadow Shape, Spread, and Knockout" (see `grown_radius`), so a square
/// corner stays sharp. `radius` is the padding box's own (inner) radius.
fn inset_shadow_hole(padding_box: Rect, radius: f64, spread: f64) -> (Rect, f64) {
    let radius = if spread < 0.0 {
        grown_radius(radius, -spread)
    } else {
        (radius - spread).max(0.0)
    };
    (padding_box.inflate(-spread, -spread), radius)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inset_spread_shrinks_the_hole() {
        // `inset 0 0 0 1px` at scale 2: a 2 device px ring around a 100x40 padding box
        let (hole, radius) = inset_shadow_hole(Rect::new(10.0, 10.0, 110.0, 50.0), 20.0, 2.0);
        assert_eq!(hole, Rect::new(12.0, 12.0, 108.0, 48.0));
        assert_eq!(radius, 18.0);
    }

    #[test]
    fn inset_negative_spread_grows_the_hole_and_radius_never_goes_negative() {
        let (hole, radius) = inset_shadow_hole(Rect::new(0.0, 0.0, 100.0, 40.0), 5.0, -3.0);
        assert_eq!(hole, Rect::new(-3.0, -3.0, 103.0, 43.0));
        assert_eq!(radius, 8.0);

        let (_, radius) = inset_shadow_hole(Rect::new(0.0, 0.0, 100.0, 40.0), 1.0, 5.0);
        assert_eq!(radius, 0.0);
    }

    #[test]
    fn outset_spread_grows_rect_and_radius_at_scale_2() {
        // `0 0 0 3px` at scale 2: 6 device px spread around a 100x40 box with 10px radius
        let (rect, radius) = outset_shadow_shape(Rect::new(10.0, 10.0, 110.0, 50.0), 10.0, 6.0);
        assert_eq!(rect, Rect::new(4.0, 4.0, 116.0, 56.0));
        assert_eq!(radius, 16.0);

        let (_, radius) = outset_shadow_shape(Rect::new(0.0, 0.0, 100.0, 40.0), 2.0, -5.0);
        assert_eq!(radius, 0.0);
    }

    #[test]
    fn outset_spread_keeps_square_corners_sharp() {
        let (_, radius) = outset_shadow_shape(Rect::new(0.0, 0.0, 100.0, 40.0), 0.0, 6.0);
        assert_eq!(radius, 0.0);
    }

    #[test]
    fn outset_spread_larger_than_radius_is_scaled_by_the_spec_curve() {
        // r = 0.4: 4 + 10 * (1 + (0.4 - 1)^3) = 11.84
        let (_, radius) = outset_shadow_shape(Rect::new(0.0, 0.0, 100.0, 40.0), 4.0, 10.0);
        assert!((radius - 11.84).abs() < 1e-6, "{radius}");
    }

    #[test]
    fn inset_negative_spread_keeps_square_corners_sharp() {
        let (_, radius) = inset_shadow_hole(Rect::new(0.0, 0.0, 100.0, 40.0), 0.0, -3.0);
        assert_eq!(radius, 0.0);
    }

    #[test]
    fn bordered_rounded_box_with_zero_spread_has_inner_radius_hole() {
        // 20px outer radius with a 4px border: padding-box corners are 16px
        let border = Insets::uniform(4.0);
        let inner = inner_radius(20.0, &border);
        assert_eq!(inner, 16.0);
        let (hole, radius) = inset_shadow_hole(Rect::new(4.0, 4.0, 96.0, 36.0), inner, 0.0);
        assert_eq!(hole, Rect::new(4.0, 4.0, 96.0, 36.0));
        assert_eq!(radius, 16.0);
        assert_eq!(inner_radius(2.0, &border), 0.0);
    }

    #[test]
    fn inset_offsets_are_scaled_to_device_pixels() {
        // 1 CSS px at scale 2 is 2 device px
        assert_eq!(inset_shadow_offset(0.0, 1.0, 2.0), (0.0, 2.0));
    }
}
