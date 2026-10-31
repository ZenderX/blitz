use super::ElementCx;
use crate::color::{Color, ToColorColor as _};
use anyrender::PaintScene;
use blitz_dom::{SELECT_ARROW_WIDTH_EM, local_name};
use kurbo::{Affine, BezPath, Cap, Circle, Join, Point, Rect, RoundedRect, Stroke, Vec2};
use peniko::Fill;
use style::dom::TElement as _;

impl ElementCx<'_, '_> {
    pub(super) fn draw_input(&self, scene: &mut impl PaintScene) {
        if self.node.local_name() != "input" {
            return;
        }
        let Some(checked) = self.element.checkbox_input_checked() else {
            return;
        };

        let type_attr = self.node.attr(local_name!("type"));
        let disabled = self.node.attr(local_name!("disabled")).is_some();

        // TODO this should be coming from css accent-color, but I couldn't find how to retrieve it
        let accent_color = if disabled {
            Color::from_rgba8(209, 209, 209, 255)
        } else {
            self.style.clone_color().as_srgb_color()
        };

        let width = self.frame.border_box.width();
        let height = self.frame.border_box.height();
        let min_dimension = width.min(height);
        let scale = (min_dimension - 4.0).max(0.0) / 16.0;

        let frame = self.frame.border_box.to_rounded_rect(scale * 2.0);

        match type_attr {
            Some("checkbox") => {
                draw_checkbox(scene, checked, frame, self.transform, accent_color, scale);
            }
            Some("radio") => {
                let center = frame.center();
                draw_radio_button(scene, checked, center, self.transform, accent_color, scale);
            }
            _ => {}
        }
    }

    /// A drop-down `<select>`: the selected option's label, vertically centred and clipped
    /// short of the arrow box, then a down-pointing arrow. The box itself is the UA sheet's
    /// border and background.
    pub(super) fn draw_select(&self, scene: &mut impl PaintScene) {
        if self.node.local_name() != "select" {
            return;
        }
        let dom = self.context.dom;
        let content = self.frame.content_box;
        let font_size = self.style.clone_font_size().used_size().px() as f64 * self.scale;
        let arrow_width = font_size * f64::from(SELECT_ARROW_WIDTH_EM);
        let label_box = Rect::new(
            content.x0,
            content.y0,
            (content.x1 - arrow_width).max(content.x0),
            content.y1,
        );

        if let Some(option_id) = dom.selected_option(self.node.id) {
            let layout = dom.layout_control_text(self.node.id, &dom.option_label(option_id));
            let y = content.y0 + ((content.height() - f64::from(layout.height())) / 2.0).max(0.0);
            let text_transform = self.transform * Affine::translate((content.x0, y));
            let overflows = f64::from(layout.full_width()) > label_box.width();
            self.context.layer_manager.maybe_with_layer(
                scene,
                overflows,
                1.0,
                self.transform,
                &label_box,
                None,
                None,
                |scene| {
                    crate::text::stroke_text(
                        scene,
                        layout.lines(),
                        dom,
                        text_transform,
                        self.scale,
                        self.node.id,
                        &mut self.context.draw_text_context.borrow_mut(),
                    );
                },
            );
        }

        let center = Point::new(content.x1 - arrow_width / 2.0, content.center().y);
        let half_width = arrow_width / 4.0;
        let mut arrow = BezPath::new();
        arrow.move_to((center.x - half_width, center.y - half_width / 2.0));
        arrow.line_to((center.x + half_width, center.y - half_width / 2.0));
        arrow.line_to((center.x, center.y + half_width / 2.0));
        arrow.close_path();
        let color = self.style.clone_color().as_srgb_color();
        scene.fill(Fill::NonZero, self.transform, color, None, &arrow);
    }
}

fn draw_checkbox(
    scene: &mut impl PaintScene,
    checked: bool,
    frame: RoundedRect,
    transform: Affine,
    accent_color: Color,
    scale: f64,
) {
    if checked {
        scene.fill(Fill::NonZero, transform, accent_color, None, &frame);
        //Tick code derived from masonry
        let mut path = BezPath::new();
        path.move_to((2.0, 9.0));
        path.line_to((6.0, 13.0));
        path.line_to((14.0, 2.0));

        path.apply_affine(Affine::translate(Vec2 { x: 2.0, y: 1.0 }).then_scale(scale));

        let style = Stroke {
            width: 2.0 * scale,
            join: Join::Round,
            miter_limit: 10.0,
            start_cap: Cap::Round,
            end_cap: Cap::Round,
            dash_pattern: Default::default(),
            dash_offset: 0.0,
        };

        scene.stroke(&style, transform, Color::WHITE, None, &path);
    } else {
        scene.fill(Fill::NonZero, transform, Color::WHITE, None, &frame);
        scene.stroke(&Stroke::default(), transform, accent_color, None, &frame);
    }
}

fn draw_radio_button(
    scene: &mut impl PaintScene,
    checked: bool,
    center: Point,
    transform: Affine,
    accent_color: Color,
    scale: f64,
) {
    let outer_ring = Circle::new(center, 8.0 * scale);
    let gap = Circle::new(center, 6.0 * scale);
    let inner_circle = Circle::new(center, 4.0 * scale);
    if checked {
        scene.fill(Fill::NonZero, transform, accent_color, None, &outer_ring);
        scene.fill(Fill::NonZero, transform, Color::WHITE, None, &gap);
        scene.fill(Fill::NonZero, transform, accent_color, None, &inner_circle);
    } else {
        const GRAY: Color = color::palette::css::GRAY;
        scene.fill(Fill::NonZero, transform, GRAY, None, &outer_ring);
        scene.fill(Fill::NonZero, transform, Color::WHITE, None, &gap);
    }
}
