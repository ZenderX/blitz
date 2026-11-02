//! Drawing a parsed SVG tree. anyrender_svg draws it as vector paths, but it drops filter
//! effects and masks and clips with only the first path of a clip path (under whatever
//! fill rule the scene last used), so a tree using any of those is rasterised by resvg
//! at the device scale and drawn as an image.

use std::sync::Arc;

use anyrender::PaintScene;
use kurbo::Affine;
use peniko::{Blob, ImageAlphaType, ImageBrush, ImageData, ImageFormat};
use resvg::tiny_skia;

/// Draw `tree` with `transform` mapping its user space (of size `tree.size()`) to the
/// scene.
pub(crate) fn draw_svg_tree(scene: &mut impl PaintScene, tree: &usvg::Tree, transform: Affine) {
    if needs_raster(tree.root()) {
        draw_rasterised(scene, tree, transform);
    } else {
        anyrender_svg::render_svg_tree(scene, tree, transform);
    }
}

/// Whether anything in `group` is beyond anyrender_svg.
fn needs_raster(group: &usvg::Group) -> bool {
    !group.filters().is_empty()
        || group.mask().is_some()
        || group.clip_path().is_some_and(|clip| !is_plain_clip(clip))
        || group.children().iter().any(|child| match child {
            usvg::Node::Group(group) => needs_raster(group),
            usvg::Node::Image(image) => {
                matches!(image.kind(), usvg::ImageKind::SVG(tree) if needs_raster(tree.root()))
            }
            usvg::Node::Path(_) | usvg::Node::Text(_) => false,
        })
}

/// A clip path anyrender_svg clips with correctly: a single non-zero path, with no
/// transform or clip path of its own.
fn is_plain_clip(clip: &usvg::ClipPath) -> bool {
    clip.transform().is_identity()
        && clip.clip_path().is_none()
        && matches!(
            clip.root().children(),
            [usvg::Node::Path(path)]
                if path.fill().is_some_and(|fill| fill.rule() == usvg::FillRule::NonZero)
        )
}

fn draw_rasterised(scene: &mut impl PaintScene, tree: &usvg::Tree, transform: Affine) {
    let [a, b, c, d, _, _] = transform.as_coeffs();
    let (scale_x, scale_y) = (a.hypot(b), c.hypot(d));
    let size = tree.size();
    let width = (f64::from(size.width()) * scale_x).ceil() as u32;
    let height = (f64::from(size.height()) * scale_y).ceil() as u32;
    // A zero-sized pixmap draws nothing, as the tree would.
    let Some(mut pixmap) = tiny_skia::Pixmap::new(width, height) else {
        return;
    };
    resvg::render(
        tree,
        tiny_skia::Transform::from_scale(scale_x as f32, scale_y as f32),
        &mut pixmap.as_mut(),
    );
    let image = ImageBrush::new(ImageData {
        data: Blob::new(Arc::new(pixmap.take())),
        format: ImageFormat::Rgba8,
        alpha_type: ImageAlphaType::AlphaPremultiplied,
        width,
        height,
    });
    scene.draw_image(
        image.as_ref(),
        transform * Affine::scale_non_uniform(1.0 / scale_x, 1.0 / scale_y),
    );
}
