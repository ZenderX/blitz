//! Painting a view transition (css-view-transitions-1): the `::view-transition`
//! overlay over the document, and the snapshots an embedder captures for it.
//!
//! While a transition is shown, each new element paints only as its
//! `::view-transition-new()` image, live, and never in place. The root element's
//! new image is the whole document. Each `::view-transition-group()` box takes its
//! (animated) `width`, `height` and `transform` from its style, at the viewport
//! origin (`::view-transition` is `position: fixed; inset: 0`). Its old and new images
//! fill the group's width at their natural aspect ratio (`inline-size: 100%;
//! block-size: auto`) with their own `opacity` and `mix-blend-mode`, inside the
//! image pair's isolation group.

use blitz_dom::node::RasterImageData;
use blitz_dom::{CapturedGeometry, NodeId, ViewTransitionCapture, ViewTransitionGroupStyles};
use kurbo::{Affine, Rect, Vec2};
use peniko::{BlendMode, Compose, Mix};
use style::computed_values::isolation::T as Isolation;
use style::computed_values::mix_blend_mode::T as MixBlendMode;
use style::properties::ComputedValues;
use style::values::computed::{CSSPixelLength, Size};

use crate::render::BlitzDomPainter;

/// An opacity just below 1.0, so the renderer keeps the image pair's isolation layer
/// as its own buffer (see `render::mask`).
const ISOLATION_ALPHA: f32 = 1.0 - f32::EPSILON;

/// Large enough that a snapshot of an element outside the viewport is not culled.
const SNAPSHOT_CLIP: Rect = Rect::new(-1.0e9, -1.0e9, 1.0e9, 1.0e9);

impl BlitzDomPainter<'_, '_> {
    /// The document, with the shown view transition's overlay over it once its
    /// pseudo-element tree is styled and its content set.
    pub(crate) fn paint_scene_with_view_transition(&self, scene: &mut impl anyrender::PaintScene) {
        let dom = self.dom;
        let Some(tree) = dom.view_transition_tree() else {
            self.paint_scene(scene);
            return;
        };
        let root_name = self.root_element_id.and_then(|root| {
            tree.groups
                .iter()
                .find(|group| dom.view_transition_new_element(&group.name) == Some(root))
                .map(|group| group.name.clone())
        });
        // A captured root paints as its group's new image; any other document paints
        // under the overlay.
        if root_name.is_none() {
            self.paint_scene(scene);
        }
        let captures = dom.view_transition_captures();
        for group in &tree.groups {
            let Some(capture) = captures.iter().find(|capture| capture.name == group.name) else {
                continue;
            };
            let is_root = root_name.as_ref() == Some(&group.name);
            self.paint_view_transition_group(scene, group, capture, is_root);
        }
    }

    fn paint_view_transition_group(
        &self,
        scene: &mut impl anyrender::PaintScene,
        group: &ViewTransitionGroupStyles,
        capture: &ViewTransitionCapture,
        is_root: bool,
    ) {
        let geometry = capture
            .new
            .or(capture.old)
            .expect("a view transition capture has a geometry");
        let width = used_px(&group.group.get_position().width).unwrap_or(geometry.width);
        let height = used_px(&group.group.get_position().height).unwrap_or(geometry.height);
        let group_transform = Affine::translate((self.initial_x, self.initial_y))
            * self.device_transform(&group.group, width, height);
        let viewport = Rect::new(
            self.initial_x,
            self.initial_y,
            self.initial_x + f64::from(self.width),
            self.initial_y + f64::from(self.height),
        );

        let is_isolated = group.image_pair.get_box().isolation == Isolation::Isolate;
        if is_isolated {
            scene.push_layer(
                Mix::Normal,
                ISOLATION_ALPHA,
                Affine::IDENTITY,
                &viewport,
                None,
                None,
            );
        }
        if let (Some(style), Some(image), Some(old)) = (
            &group.old,
            self.dom.view_transition_old_image(&group.name),
            capture.old,
        ) {
            self.with_image_layer(scene, style, group_transform, width, old, |scene, size| {
                let brush = snapshot_brush(image);
                let image_scale = Affine::scale_non_uniform(
                    size.x / f64::from(image.width),
                    size.y / f64::from(image.height),
                );
                scene.draw_image(brush.as_ref(), group_transform * image_scale);
            });
        }
        if let (Some(style), Some(new)) = (&group.new, capture.new) {
            if is_root {
                self.with_image_layer(scene, style, Affine::IDENTITY, width, new, |scene, _| {
                    self.paint_scene(scene);
                });
            } else if let Some(element) = self.dom.view_transition_new_element(&group.name) {
                self.with_image_layer(scene, style, group_transform, width, new, |scene, _| {
                    let image_scale = f64::from(width / new.width);
                    self.paint_captured_element(
                        scene,
                        element,
                        group_transform * Affine::scale(image_scale),
                        viewport,
                    );
                });
            }
        }
        if is_isolated {
            scene.pop_layer();
        }
    }

    /// Run `paint` inside a layer with `style`'s `opacity` and `mix-blend-mode`,
    /// clipped to the image's box: `group_width` wide at the natural aspect ratio of
    /// `natural`. `paint` gets the box size in device pixels.
    fn with_image_layer<S: anyrender::PaintScene>(
        &self,
        scene: &mut S,
        style: &ComputedValues,
        transform: Affine,
        group_width: f32,
        natural: CapturedGeometry,
        paint: impl FnOnce(&mut S, Vec2),
    ) {
        let opacity = style.get_effects().opacity;
        if opacity == 0.0 || natural.width <= 0.0 || natural.height <= 0.0 {
            return;
        }
        let size = Vec2::new(
            f64::from(group_width),
            f64::from(group_width * natural.height / natural.width),
        ) * self.scale;
        let blend = blend_mode(style.get_effects().mix_blend_mode);
        let clip = Rect::from_origin_size((0.0, 0.0), (size.x, size.y));
        scene.push_layer(blend, opacity, transform, &clip, None, None);
        paint(scene, size);
        scene.pop_layer();
    }

    /// Paint `element`, a captured element, with its border box at `transform`'s origin.
    fn paint_captured_element(
        &self,
        scene: &mut impl anyrender::PaintScene,
        element: NodeId,
        transform: Affine,
        clip_rect: Rect,
    ) {
        let location = self.dom.as_ref().tree()[element].final_layout().location;
        // `render_element` translates by the element's own location again.
        let unplaced = Affine::translate(-Vec2::new(
            f64::from(location.x) * self.scale,
            f64::from(location.y) * self.scale,
        ));
        self.live_capture.set(Some(element));
        self.render_element(scene, element, transform * unplaced, clip_rect, None);
        self.live_capture.set(None);
    }

    /// The old snapshot of `element` (css-view-transitions-1 §7.3.1 "capture the
    /// image"): the root element's is the document in the viewport, any other's is the
    /// element with its border box at the origin. The painter's captured elements are
    /// left out.
    pub(crate) fn paint_view_transition_snapshot(
        &self,
        scene: &mut impl anyrender::PaintScene,
        element: NodeId,
    ) {
        if self.root_element_id == Some(element) {
            self.paint_scene(scene);
        } else {
            self.paint_captured_element(scene, element, Affine::IDENTITY, SNAPSHOT_CLIP);
        }
    }

    /// `style`'s transform for a `width` x `height` CSS-pixel box, in device pixels.
    fn device_transform(&self, style: &ComputedValues, width: f32, height: f32) -> Affine {
        let reference_box = euclid::default::Rect::new(
            euclid::default::Point2D::new(CSSPixelLength::new(0.0), CSSPixelLength::new(0.0)),
            euclid::default::Size2D::new(CSSPixelLength::new(width), CSSPixelLength::new(height)),
        );
        let Some(transform) = blitz_dom::resolve_2d_transform(style.get_box(), reference_box)
        else {
            return Affine::IDENTITY;
        };
        // S * T * S^-1, as `Node::transform` converts an element's.
        let [m11, m12, m21, m22, m41, m42] = transform.as_coeffs();
        Affine::new([m11, m12, m21, m22, m41 * self.scale, m42 * self.scale])
    }
}

/// A length's pixels, `None` for `auto` and the intrinsic keywords.
fn used_px(size: &Size) -> Option<f32> {
    match size {
        Size::LengthPercentage(length) => Some(length.0.resolve(CSSPixelLength::new(0.0)).px()),
        _ => None,
    }
}

/// The brush of an old snapshot: premultiplied, as the embedder rasterized it.
fn snapshot_brush(image: &RasterImageData) -> peniko::ImageBrush {
    peniko::ImageBrush {
        image: peniko::ImageData {
            data: image.data.clone(),
            format: peniko::ImageFormat::Rgba8,
            width: image.width,
            height: image.height,
            alpha_type: peniko::ImageAlphaType::AlphaPremultiplied,
        },
        sampler: peniko::ImageSampler {
            x_extend: peniko::Extend::Pad,
            y_extend: peniko::Extend::Pad,
            quality: peniko::ImageQuality::Medium,
            alpha: 1.0,
        },
    }
}

/// `mix-blend-mode` as a blend mode (compositing-1 §10): `plus-lighter` is a
/// Porter-Duff operator, every other value a mix over source-over.
fn blend_mode(mode: MixBlendMode) -> BlendMode {
    let mix = match mode {
        MixBlendMode::PlusLighter => return BlendMode::new(Mix::Normal, Compose::PlusLighter),
        MixBlendMode::Normal => Mix::Normal,
        MixBlendMode::Multiply => Mix::Multiply,
        MixBlendMode::Screen => Mix::Screen,
        MixBlendMode::Overlay => Mix::Overlay,
        MixBlendMode::Darken => Mix::Darken,
        MixBlendMode::Lighten => Mix::Lighten,
        MixBlendMode::ColorDodge => Mix::ColorDodge,
        MixBlendMode::ColorBurn => Mix::ColorBurn,
        MixBlendMode::HardLight => Mix::HardLight,
        MixBlendMode::SoftLight => Mix::SoftLight,
        MixBlendMode::Difference => Mix::Difference,
        MixBlendMode::Exclusion => Mix::Exclusion,
        MixBlendMode::Hue => Mix::Hue,
        MixBlendMode::Saturation => Mix::Saturation,
        MixBlendMode::Color => Mix::Color,
        MixBlendMode::Luminosity => Mix::Luminosity,
    };
    BlendMode::new(mix, Compose::SrcOver)
}
