//! Column rules (css-multicol-1 §4): a line in the middle of each column gap between
//! two columns that hold content, as tall as the column boxes, painted above the
//! multi-column container's border and below its content.

use super::ElementCx;
use super::border::{dashed_ratios, grooved_edge_colors};
use crate::color::{Color, ToColorColor as _};
use crate::kurbo_css::Edge;
use anyrender::PaintScene;
use kurbo::{BezPath, Cap, Circle, Line, Point, Rect, Shape as _, Stroke};
use peniko::Fill;
use style::values::computed::BorderStyle;

impl ElementCx<'_, '_> {
    pub(super) fn draw_column_rules(&self, scene: &mut impl PaintScene) {
        let Some(columns) = self.element.multicol_info.as_deref() else {
            return;
        };
        // The column geometry is stale once the box stops being a multi-column block container.
        if columns.used_columns < 2 || !self.style.is_multicol() || self.node.flags.is_inline_root()
        {
            return;
        }
        let rule = self.style.get_column();
        let width = rule.column_rule_width.0.to_f64_px();
        let color = rule
            .column_rule_color
            .resolve_to_absolute(&self.style.clone_color())
            .as_srgb_color();
        let top = f64::from(columns.origin.y);
        let bottom = top + f64::from(columns.column_height);
        if width <= 0.0 || bottom <= top || color.components[3] <= 0.0 {
            return;
        }
        let pitch = f64::from(columns.column_width + columns.column_gap);
        let first_center =
            f64::from(columns.origin.x + columns.column_width + columns.column_gap / 2.0);
        for gap in 0..columns.used_columns - 1 {
            let center = first_center + gap as f64 * pitch;
            let line = Rect::new(center - width / 2.0, top, center + width / 2.0, bottom)
                .scale_from_origin(self.scale);
            self.draw_column_rule(scene, line, rule.column_rule_style, color);
        }
    }

    /// One vertical rule filling `line` (device pixels), drawn in `style`.
    fn draw_column_rule(
        &self,
        scene: &mut impl PaintScene,
        line: Rect,
        style: BorderStyle,
        color: Color,
    ) {
        let thickness = line.width();
        let center = line.center().x;
        let mut fill = |color: Color, shape: Rect| {
            scene.fill(Fill::NonZero, self.transform, color, None, &shape);
        };
        match style {
            BorderStyle::None | BorderStyle::Hidden => {}
            BorderStyle::Solid => fill(color, line),
            // Two lines a third of the width each, below 3px a solid line, as for borders.
            BorderStyle::Double => {
                if thickness < 3.0 * self.scale {
                    fill(color, line);
                } else {
                    let third = thickness / 3.0;
                    fill(color, Rect::new(line.x0, line.y0, line.x0 + third, line.y1));
                    fill(color, Rect::new(line.x1 - third, line.y0, line.x1, line.y1));
                }
            }
            // `column-rule-style` reads as in the collapsing border model (css-multicol-1
            // §4.2): `inset` is `ridge` and `outset` is `groove`.
            BorderStyle::Groove | BorderStyle::Outset | BorderStyle::Ridge | BorderStyle::Inset => {
                let ridge = matches!(style, BorderStyle::Ridge | BorderStyle::Inset);
                let (outer, inner) = grooved_edge_colors(color, Edge::Left, ridge);
                fill(outer, Rect::new(line.x0, line.y0, center, line.y1));
                fill(inner, Rect::new(center, line.y0, line.x1, line.y1));
            }
            BorderStyle::Dashed => {
                let (dash, gap) = dashed_ratios(thickness, self.scale);
                let stroke = Stroke::new(thickness)
                    .with_caps(Cap::Butt)
                    .with_dashes(0.0, [dash * thickness, gap * thickness]);
                let path = Line::new(Point::new(center, line.y0), Point::new(center, line.y1));
                scene.stroke(&stroke, self.transform, color, None, &path);
            }
            // Dots one rule wide, about two widths apart, the first and last at the ends.
            BorderStyle::Dotted => {
                let radius = thickness / 2.0;
                let span = (line.height() - thickness).max(0.0);
                let gaps = (span / (2.0 * thickness)).round().max(1.0);
                let mut dots = BezPath::new();
                for index in 0..=gaps as usize {
                    let y = line.y0 + radius + span * index as f64 / gaps;
                    dots.extend(Circle::new(Point::new(center, y), radius).path_elements(0.1));
                }
                scene.fill(Fill::NonZero, self.transform, color, None, &dots);
            }
        }
    }
}
