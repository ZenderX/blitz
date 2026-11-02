//! The `resize` grip (css-ui-4 §5.1), in the square blitz-dom's
//! [`resizer_rect`](blitz_dom::Node::resizer_rect) hit-tests.

use super::ElementCx;
use crate::color::Color;
use anyrender::PaintScene;
use kurbo::{Line, Point, Stroke, Vec2};

impl ElementCx<'_, '_> {
    /// Two diagonal strokes across the grip's corner, dark over a light offset copy so
    /// the grip reads on light and dark backgrounds.
    pub(super) fn draw_resizer(&self, scene: &mut impl PaintScene) {
        let Some(grip) = self.node.resizer_rect() else {
            return;
        };
        let grip = grip.scale_from_origin(self.scale);
        let stroke = Stroke::new(self.scale);
        let dark = Color::from_rgba8(0, 0, 0, 140);
        let light = Color::from_rgba8(255, 255, 255, 140);
        let margin = 2.0 * self.scale;
        let (x1, y1) = (grip.x1 - margin, grip.y1 - margin);
        for reach in [0.4, 0.8] {
            let length = grip.width().min(grip.height()) * reach;
            let line = Line::new(Point::new(x1 - length, y1), Point::new(x1, y1 - length));
            let highlight = line + Vec2::new(self.scale, self.scale);
            scene.stroke(&stroke, self.transform, light, None, &highlight);
            scene.stroke(&stroke, self.transform, dark, None, &line);
        }
    }
}
