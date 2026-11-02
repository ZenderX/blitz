//! `image-orientation` (css-images-3 §5.3) for raster images. Decode stores the pixels
//! already turned by their EXIF orientation, which is what the initial `from-image`
//! shows; `none` shows the pixels as encoded, so it undoes that orientation.

use image::metadata::Orientation;
use kurbo::Affine;
use style::computed_values::image_orientation::T as ImageOrientation;

use super::RasterImageData;

impl RasterImageData {
    /// The image's natural size as `image_orientation` shows it.
    pub fn displayed_size(&self, image_orientation: ImageOrientation) -> (u32, u32) {
        let swaps_axes = matches!(
            self.orientation,
            Orientation::Rotate90
                | Orientation::Rotate270
                | Orientation::Rotate90FlipH
                | Orientation::Rotate270FlipH
        );
        match image_orientation {
            ImageOrientation::None if swaps_axes => (self.height, self.width),
            ImageOrientation::None | ImageOrientation::FromImage => (self.width, self.height),
        }
    }

    /// The map from the stored pixels to the image as `image_orientation` shows it, both
    /// in pixels: the inverse of decode's EXIF orientation for `none`, else the identity.
    pub fn display_transform(&self, image_orientation: ImageOrientation) -> Affine {
        if image_orientation == ImageOrientation::FromImage {
            return Affine::IDENTITY;
        }
        let (w, h) = (self.width as f64, self.height as f64);
        // Each is the inverse of `DynamicImage::apply_orientation` for that value, in the
        // stored (oriented) w × h space: [a, b, c, d, e, f] maps (x, y) to
        // (a·x + c·y + e, b·x + d·y + f).
        Affine::new(match self.orientation {
            Orientation::NoTransforms => [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
            Orientation::Rotate90 => [0.0, -1.0, 1.0, 0.0, 0.0, w],
            Orientation::Rotate180 => [-1.0, 0.0, 0.0, -1.0, w, h],
            Orientation::Rotate270 => [0.0, 1.0, -1.0, 0.0, h, 0.0],
            Orientation::FlipHorizontal => [-1.0, 0.0, 0.0, 1.0, w, 0.0],
            Orientation::FlipVertical => [1.0, 0.0, 0.0, -1.0, 0.0, h],
            Orientation::Rotate90FlipH => [0.0, 1.0, 1.0, 0.0, 0.0, 0.0],
            Orientation::Rotate270FlipH => [0.0, -1.0, -1.0, 0.0, h, w],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, Rgba, RgbaImage};
    use std::sync::Arc;

    const ALL: [Orientation; 8] = [
        Orientation::NoTransforms,
        Orientation::Rotate90,
        Orientation::Rotate180,
        Orientation::Rotate270,
        Orientation::FlipHorizontal,
        Orientation::FlipVertical,
        Orientation::Rotate90FlipH,
        Orientation::Rotate270FlipH,
    ];

    /// `none` must show exactly the encoded pixels: every raw pixel centre, oriented the
    /// way decode does it, lands back on itself through `display_transform`.
    #[test]
    fn none_undoes_each_decode_orientation() {
        let (raw_w, raw_h) = (3u32, 2u32);
        let raw = RgbaImage::from_fn(raw_w, raw_h, |x, y| Rgba([x as u8, y as u8, 0, 255]));
        for orientation in ALL {
            let mut stored = DynamicImage::ImageRgba8(raw.clone());
            stored.apply_orientation(orientation);
            let stored = stored.into_rgba8();
            let mut image = RasterImageData::new(
                stored.width(),
                stored.height(),
                Arc::new(stored.clone().into_raw()),
            );
            image.orientation = orientation;

            assert_eq!(
                image.displayed_size(ImageOrientation::None),
                (raw_w, raw_h),
                "{orientation:?}"
            );
            assert_eq!(
                image.displayed_size(ImageOrientation::FromImage),
                (stored.width(), stored.height())
            );
            let to_raw = image.display_transform(ImageOrientation::None);
            for (x, y, pixel) in stored.enumerate_pixels() {
                let p = to_raw * kurbo::Point::new(x as f64 + 0.5, y as f64 + 0.5);
                let [raw_x, raw_y, ..] = pixel.0;
                assert_eq!(
                    (p.x - 0.5, p.y - 0.5),
                    (raw_x as f64, raw_y as f64),
                    "{orientation:?} stored ({x}, {y})"
                );
            }
            assert_eq!(
                image.display_transform(ImageOrientation::FromImage),
                Affine::IDENTITY
            );
        }
    }
}
