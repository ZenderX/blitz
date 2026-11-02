use super::ElementCx;
use blitz_dom::basic_shape::{ReferenceBox, basic_shape_to_path};
use kurbo::{BezPath, Rect, Shape};
use style::values::computed::basic_shape::ClipPath;
use style::values::generics::basic_shape::{ShapeBox, ShapeGeometryBox};

impl ElementCx<'_, '_> {
    /// Compute the clip-path BezPath (if any) for this element.
    /// Returns `None` if clip-path is `none` or unsupported.
    pub(super) fn clip_path_shape(&self) -> Option<BezPath> {
        let clip_path = self.style.clone_clip_path();
        match clip_path {
            ClipPath::None => None,
            ClipPath::Url(_) => {
                // URL references (e.g. `clip-path: url(#myClip)`) are not yet supported
                None
            }
            ClipPath::Shape(basic_shape, geometry_box) => {
                let reference_box = self.resolve_geometry_box(&geometry_box);
                basic_shape_to_path(&basic_shape, reference_box, 0.1)
            }
            ClipPath::Box(geometry_box) => {
                let reference_box = self.resolve_geometry_box(&geometry_box);
                Some(Rect::from(reference_box).into_path(0.1))
            }
        }
    }

    /// Resolve a ShapeGeometryBox to a concrete rectangle (x, y, width, height) in scaled pixels
    ///
    /// For SVG elements without associated CSS layout box, the used value for content-box and padding-box is fill-box and for border-box and margin-box is stroke-box.
    /// For elements with associated CSS layout box, the used value for fill-box is content-box and for stroke-box and view-box is border-box.
    fn resolve_geometry_box(&self, geometry_box: &ShapeGeometryBox) -> ReferenceBox {
        match geometry_box {
            ShapeGeometryBox::ElementDependent
            | ShapeGeometryBox::StrokeBox
            | ShapeGeometryBox::ViewBox
            | ShapeGeometryBox::ShapeBox(ShapeBox::BorderBox) => ReferenceBox {
                x: 0.0,
                y: 0.0,
                width: self.frame.border_box.width() / self.scale,
                height: self.frame.border_box.height() / self.scale,
            },
            ShapeGeometryBox::ShapeBox(ShapeBox::PaddingBox) => ReferenceBox {
                x: self.frame.border_width.x0 / self.scale,
                y: self.frame.border_width.y0 / self.scale,
                width: self.frame.padding_box.width() / self.scale,
                height: self.frame.padding_box.height() / self.scale,
            },
            ShapeGeometryBox::FillBox | ShapeGeometryBox::ShapeBox(ShapeBox::ContentBox) => {
                ReferenceBox {
                    x: (self.frame.border_width.x0 + self.frame.padding_width.x0) / self.scale,
                    y: (self.frame.border_width.y0 + self.frame.padding_width.y0) / self.scale,
                    width: self.frame.content_box.width() / self.scale,
                    height: self.frame.content_box.height() / self.scale,
                }
            }
            ShapeGeometryBox::ShapeBox(ShapeBox::MarginBox) => {
                // Margin box is not tracked in CssBox, fall back to border box
                ReferenceBox {
                    x: 0.0,
                    y: 0.0,
                    width: self.frame.border_box.width() / self.scale,
                    height: self.frame.border_box.height() / self.scale,
                }
            }
        }
    }
}
