//! css-shapes-1 float areas: the outline a float's `shape-outside` (grown by
//! `shape-margin`, thresholded by `shape-image-threshold` for an image) gives the
//! lines an inline layout wraps beside it.
//!
//! Only floats that the inline layout itself places carry an area: a float placed by
//! taffy's block layout (a block-level sibling of the text) stays a margin-box
//! rectangle, because taffy's float context keeps no per-float data to attach one to.

use std::f64::consts::{FRAC_PI_2, PI};

use blitz_traits::node_id::NodeId;
use kurbo::{BezPath, PathEl, Point, Vec2};
use style::values::computed::BorderCornerRadius;
use style::values::computed::basic_shape::BasicShape;
use style::values::generics::basic_shape::{GenericBasicShape, ShapeBox, ShapeOutside};
use taffy::{BlockContext, Clear, ContentSlot, ResolveOrZero as _, Size};

use super::resolve_calc_value;
use crate::BaseDocument;
use crate::basic_shape::{ReferenceBox, basic_shape_to_path, resolve_lp};
use crate::node::{ImageData, RasterImageData, Status};

/// Curve flattening tolerance in CSS px, finer than Chromium's 1/64px layout unit.
const TOLERANCE: f64 = 0.005;

/// A float an inline layout placed, in the inline root's content-box coordinates.
pub(crate) struct PlacedFloat {
    pub is_left: bool,
    /// The margin box.
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// The `shape-outside` float area; `None` is the margin box.
    pub area: Option<FloatArea>,
}

/// A float area: an outline in the float's margin-box coordinates.
pub(crate) struct FloatArea {
    edges: Vec<(Point, Point)>,
    /// `shape-margin` in CSS px.
    margin: f64,
}

impl PlacedFloat {
    fn overlaps(&self, top: f32, bottom: f32) -> bool {
        top < self.y + self.height && (self.y < bottom || self.y <= top)
    }

    fn contains(&self, y: f32) -> bool {
        self.y <= y && y < self.y + self.height
    }

    /// The edge of the float area that lines between `top` and `bottom` keep clear of
    /// (its right edge for a left float, its left edge for a right one), or `None` when
    /// the area leaves those lines alone. The area is clipped to the margin box.
    fn line_edge(&self, top: f32, bottom: f32) -> Option<f32> {
        let Some(area) = &self.area else {
            return Some(if self.is_left {
                self.x + self.width
            } else {
                self.x
            });
        };
        let top = f64::from((top - self.y).max(0.0));
        let bottom = f64::from((bottom - self.y).min(self.height));
        let sign = if self.is_left { 1.0 } else { -1.0 };
        let reach = area.reach(top, bottom.max(top), sign)?;
        Some(self.x + (reach as f32).clamp(0.0, self.width))
    }
}

/// taffy's slot for a line from `y` (CSS px, content box), with its edges moved to the
/// float areas of `floats` over the line's block range `y..y + height`.
pub(crate) fn line_slot(
    block_ctx: &BlockContext<'_>,
    floats: &[PlacedFloat],
    y: f32,
    height: f32,
) -> ContentSlot {
    let slot = block_ctx.find_content_slot(y, Clear::None, None);
    match line_edges(floats, slot.x, slot.x + slot.width, slot.y, slot.y + height) {
        Some((left, right)) => ContentSlot {
            x: left,
            width: (right - left).max(0.0),
            ..slot
        },
        None => slot,
    }
}

/// The left and right edges (content-box x) of a line spanning `top..bottom` beside the
/// `floats` an inline layout placed, or `None` when no float area overlaps the line, so
/// that taffy's margin-box slot (`slot_left..slot_right`) stands.
///
/// taffy's slot already avoids every float's margin box at `top`. A placed float that
/// contains `top` sits beyond every earlier float on its side at `top`, so its margin
/// edge bounds them, and the slot edge (which may come from a shaped float's margin
/// box) is recomputed from the placed floats alone.
pub(crate) fn line_edges(
    floats: &[PlacedFloat],
    slot_left: f32,
    slot_right: f32,
    top: f32,
    bottom: f32,
) -> Option<(f32, f32)> {
    if !floats
        .iter()
        .any(|f| f.area.is_some() && f.overlaps(top, bottom))
    {
        return None;
    }
    let mut left = floats
        .iter()
        .filter(|f| f.is_left && f.contains(top))
        .map(|f| f.x)
        .reduce(f32::max)
        .unwrap_or(slot_left);
    let mut right = floats
        .iter()
        .filter(|f| !f.is_left && f.contains(top))
        .map(|f| f.x + f.width)
        .reduce(f32::min)
        .unwrap_or(slot_right);
    for float in floats {
        let applies = match float.area {
            Some(_) => float.overlaps(top, bottom),
            None => float.contains(top),
        };
        let Some(edge) = applies.then(|| float.line_edge(top, bottom)).flatten() else {
            continue;
        };
        if float.is_left {
            left = left.max(edge);
        } else {
            right = right.min(edge);
        }
    }
    Some((left, right))
}

impl FloatArea {
    /// The largest `sign * x` over the area grown by `shape-margin`, within the band
    /// `top..=bottom`; `None` when that part of the area is empty.
    ///
    /// The maximum lies on the outline: per edge it is the maximum of `sign * x(y)` plus
    /// the margin's reach at `y` (`margin` inside the band, the disc's half-chord
    /// outside it), which is concave on each piece, so the piece ends and the clamped
    /// stationary points are the only candidates.
    fn reach(&self, top: f64, bottom: f64, sign: f64) -> Option<f64> {
        let m = self.margin;
        let grow = |y: f64| {
            let d = if y < top {
                top - y
            } else if y > bottom {
                y - bottom
            } else {
                0.0
            };
            (d <= m).then(|| (m * m - d * d).max(0.0).sqrt())
        };
        let mut best: Option<f64> = None;
        for &(a, b) in &self.edges {
            let (p, q) = if a.y <= b.y { (a, b) } else { (b, a) };
            let lo = p.y.max(top - m);
            let hi = q.y.min(bottom + m);
            if lo > hi {
                continue;
            }
            if p.y == q.y {
                if let Some(g) = grow(p.y) {
                    let s = (sign * p.x).max(sign * q.x) + g;
                    best = Some(best.map_or(s, |v| v.max(s)));
                }
                continue;
            }
            let k = sign * (q.x - p.x) / (q.y - p.y);
            let s_at = |y: f64| sign * p.x + k * (y - p.y);
            let stationary = k * m / (1.0 + k * k).sqrt();
            for y in [
                lo,
                hi,
                top.clamp(lo, hi),
                bottom.clamp(lo, hi),
                (top + stationary).min(top).clamp(lo, hi),
                (bottom + stationary).max(bottom).clamp(lo, hi),
            ] {
                if let Some(g) = grow(y) {
                    let s = s_at(y) + g;
                    best = Some(best.map_or(s, |v| v.max(s)));
                }
            }
        }
        best.map(|v| sign * v)
    }
}

impl BaseDocument {
    /// The float area of the float `node_id`, whose border box is `size` and margins
    /// `margin`, with percentages against `parent_size`; `None` (the margin box) for
    /// `shape-outside: none` and for an image that is not a loaded raster image.
    pub(crate) fn float_area(
        &self,
        node_id: NodeId,
        size: Size<f32>,
        margin: taffy::Rect<f32>,
        parent_size: Size<Option<f32>>,
    ) -> Option<FloatArea> {
        let node = &self.nodes[node_id];
        let style = node.primary_styles()?;
        let shape_outside = &style.get_box().shape_outside;
        if let ShapeOutside::None = shape_outside {
            return None;
        }

        let taffy_style = node.style();
        let border = taffy_style
            .border
            .resolve_or_zero(parent_size.width, resolve_calc_value);
        let padding = taffy_style
            .padding
            .resolve_or_zero(parent_size.width, resolve_calc_value);
        let boxes = Boxes::new(size, margin, border, padding);

        let edges = match shape_outside {
            ShapeOutside::None => return None,
            ShapeOutside::Shape(shape, shape_box) => {
                let reference = boxes.reference(*shape_box);
                outline_edges(&shape_path(shape, reference)?)
            }
            ShapeOutside::Box(shape_box) => {
                let b = style.get_border();
                let border_radii = scale_radii(
                    [
                        &b.border_top_left_radius,
                        &b.border_top_right_radius,
                        &b.border_bottom_right_radius,
                        &b.border_bottom_left_radius,
                    ]
                    .map(|r| resolve_corner(r, f64::from(size.width), f64::from(size.height))),
                    f64::from(size.width),
                    f64::from(size.height),
                );
                let reference = boxes.reference(*shape_box);
                let radii = boxes.box_radii(*shape_box, border_radii);
                outline_edges(&rounded_rect(reference, radii))
            }
            ShapeOutside::Image(_) => {
                let image = node
                    .element_data()?
                    .shape_outside_images
                    .first()?
                    .as_ref()
                    .filter(|image| image.status == Status::Ok)?;
                let ImageData::Raster(raster) = &image.image else {
                    return None;
                };
                let threshold = style.get_box().shape_image_threshold;
                raster_edges(raster, boxes.reference(ShapeBox::ContentBox), threshold)
            }
        };
        let basis = parent_size.width.unwrap_or(0.0);
        let margin = style
            .get_box()
            .shape_margin
            .0
            .resolve(style::values::computed::CSSPixelLength::new(basis))
            .px();
        Some(FloatArea {
            edges,
            margin: f64::from(margin),
        })
    }
}

/// A float's boxes in its margin-box coordinates.
struct Boxes {
    size: Size<f64>,
    margin: taffy::Rect<f64>,
    border: taffy::Rect<f64>,
    padding: taffy::Rect<f64>,
}

impl Boxes {
    fn new(
        size: Size<f32>,
        margin: taffy::Rect<f32>,
        border: taffy::Rect<f32>,
        padding: taffy::Rect<f32>,
    ) -> Self {
        Self {
            size: size.map(f64::from),
            margin: margin.map(f64::from),
            border: border.map(f64::from),
            padding: padding.map(f64::from),
        }
    }

    /// How far `shape_box`'s edges lie inside the border box (negative: outside it).
    fn insets(&self, shape_box: ShapeBox) -> taffy::Rect<f64> {
        let zero = taffy::Rect {
            left: 0.0,
            right: 0.0,
            top: 0.0,
            bottom: 0.0,
        };
        match shape_box {
            ShapeBox::MarginBox => self.margin.map(|m| -m),
            ShapeBox::BorderBox => zero,
            ShapeBox::PaddingBox => self.border,
            ShapeBox::ContentBox => self.border + self.padding,
        }
    }

    fn reference(&self, shape_box: ShapeBox) -> ReferenceBox {
        let inset = self.insets(shape_box);
        ReferenceBox {
            x: self.margin.left + inset.left,
            y: self.margin.top + inset.top,
            width: (self.size.width - inset.left - inset.right).max(0.0),
            height: (self.size.height - inset.top - inset.bottom).max(0.0),
        }
    }

    /// The corner radii of `shape_box` (top-left, top-right, bottom-right, bottom-left)
    /// from the border box's `radii`: css-backgrounds-3 shrinks them by the border and
    /// padding inside, and css-shapes-1 §3.1.1 grows them by the margin outside.
    fn box_radii(&self, shape_box: ShapeBox, radii: [Vec2; 4]) -> [Vec2; 4] {
        let inset = self.insets(shape_box);
        let sides = [
            (inset.left, inset.top),
            (inset.right, inset.top),
            (inset.right, inset.bottom),
            (inset.left, inset.bottom),
        ];
        let adjust = |r: f64, inset: f64| {
            if inset >= 0.0 {
                return (r - inset).max(0.0);
            }
            let margin = -inset;
            let ratio = r / margin;
            if ratio >= 1.0 {
                r + margin
            } else {
                r + margin * (1.0 + (ratio - 1.0).powi(3))
            }
        };
        std::array::from_fn(|i| {
            Vec2::new(
                adjust(radii[i].x, sides[i].0),
                adjust(radii[i].y, sides[i].1),
            )
        })
    }
}

fn resolve_corner(radius: &BorderCornerRadius, width: f64, height: f64) -> Vec2 {
    Vec2::new(
        resolve_lp(&radius.0.width.0, width),
        resolve_lp(&radius.0.height.0, height),
    )
}

/// css-backgrounds-3 §5.5: radii whose sum exceeds a side all shrink by one factor.
fn scale_radii(radii: [Vec2; 4], width: f64, height: f64) -> [Vec2; 4] {
    let [tl, tr, br, bl] = radii;
    let fit = |side: f64, sum: f64| if sum > 0.0 { side / sum } else { 1.0 };
    let factor = fit(width, tl.x + tr.x)
        .min(fit(width, bl.x + br.x))
        .min(fit(height, tl.y + bl.y))
        .min(fit(height, tr.y + br.y))
        .min(1.0);
    radii.map(|r| r * factor)
}

/// A basic shape's outline; `inset()` keeps its `round` corners.
fn shape_path(shape: &BasicShape, reference: ReferenceBox) -> Option<BezPath> {
    let GenericBasicShape::Rect(inset) = shape else {
        return basic_shape_to_path(shape, reference, TOLERANCE);
    };
    let (w, h) = (reference.width, reference.height);
    let top = resolve_lp(&inset.rect.0, h);
    let right = resolve_lp(&inset.rect.1, w);
    let bottom = resolve_lp(&inset.rect.2, h);
    let left = resolve_lp(&inset.rect.3, w);
    let rect = ReferenceBox {
        x: reference.x + left,
        y: reference.y + top,
        width: w - left - right,
        height: h - top - bottom,
    };
    if rect.width <= 0.0 || rect.height <= 0.0 {
        return None;
    }
    let round = &inset.round;
    let radii = [
        &round.top_left,
        &round.top_right,
        &round.bottom_right,
        &round.bottom_left,
    ]
    .map(|r| resolve_corner(r, rect.width, rect.height));
    Some(rounded_rect(
        rect,
        scale_radii(radii, rect.width, rect.height),
    ))
}

/// A rectangle with elliptical corners (top-left, top-right, bottom-right, bottom-left).
fn rounded_rect(rect: ReferenceBox, radii: [Vec2; 4]) -> BezPath {
    let (x0, y0) = (rect.x, rect.y);
    let (x1, y1) = (rect.x + rect.width, rect.y + rect.height);
    let [tl, tr, br, bl] = radii;
    let corners = [
        (Point::new(x1 - tr.x, y0 + tr.y), tr, -FRAC_PI_2),
        (Point::new(x1 - br.x, y1 - br.y), br, 0.0),
        (Point::new(x0 + bl.x, y1 - bl.y), bl, FRAC_PI_2),
        (Point::new(x0 + tl.x, y0 + tl.y), tl, PI),
    ];
    let mut path = BezPath::new();
    path.move_to((x0 + tl.x, y0));
    for (center, r, start) in corners {
        path.line_to(center + Vec2::new(r.x * start.cos(), r.y * start.sin()));
        if r.x > 0.0 && r.y > 0.0 {
            for el in kurbo::Arc::new(center, r, start, FRAC_PI_2, 0.0).append_iter(TOLERANCE) {
                path.push(el);
            }
        }
    }
    path.close_path();
    path
}

/// The outline of `path` (every subpath closed, as fill closes it) as line segments.
fn outline_edges(path: &BezPath) -> Vec<(Point, Point)> {
    let mut edges = Vec::new();
    let mut start: Option<Point> = None;
    let mut last: Option<Point> = None;
    let close = |edges: &mut Vec<(Point, Point)>, start: Option<Point>, last: Option<Point>| {
        if let (Some(s), Some(l)) = (start, last) {
            if s != l {
                edges.push((l, s));
            }
        }
    };
    kurbo::flatten(path.iter(), TOLERANCE, |el| match el {
        PathEl::MoveTo(p) => {
            close(&mut edges, start, last);
            start = Some(p);
            last = Some(p);
        }
        PathEl::LineTo(p) => {
            if let Some(l) = last {
                edges.push((l, p));
            }
            last = Some(p);
        }
        PathEl::ClosePath => {
            close(&mut edges, start, last);
            last = start;
        }
        // `flatten` emits line segments only.
        PathEl::QuadTo(..) | PathEl::CurveTo(..) => {}
    });
    close(&mut edges, start, last);
    edges
}

/// The outline of the pixels of `image`, stretched over `content`, whose alpha exceeds
/// `threshold` (css-shapes-1 §6.1), sampled once per CSS px: each run of rows with the
/// same leftmost and rightmost such pixel contributes its two vertical sides, which is
/// all [`FloatArea::reach`] needs.
fn raster_edges(
    image: &RasterImageData,
    content: ReferenceBox,
    threshold: f32,
) -> Vec<(Point, Point)> {
    let (iw, ih) = (image.width as usize, image.height as usize);
    let cols = content.width.ceil() as usize;
    let rows = content.height.ceil() as usize;
    if iw == 0 || ih == 0 || cols == 0 || rows == 0 {
        return Vec::new();
    }
    let pixels: &[u8] = image.data.as_ref();
    let source = |i: usize, n: usize, extent: f64| {
        (((i as f64 + 0.5) / extent * n as f64) as usize).min(n - 1)
    };
    let span = |row: usize| {
        let sy = source(row, ih, content.height);
        let opaque = |col: usize| {
            let sx = source(col, iw, content.width);
            f32::from(pixels[(sy * iw + sx) * 4 + 3]) / 255.0 > threshold
        };
        let first = (0..cols).find(|&c| opaque(c))?;
        let last = (0..cols).rev().find(|&c| opaque(c))?;
        Some((first, last + 1))
    };

    let mut edges = Vec::new();
    let mut push = |run: (usize, usize), from: usize, to: usize| {
        let y0 = content.y + from as f64;
        let y1 = content.y + (to as f64).min(content.height);
        for x in [run.0, run.1] {
            let x = content.x + (x as f64).min(content.width);
            edges.push((Point::new(x, y0), Point::new(x, y1)));
        }
    };
    let mut current: Option<((usize, usize), usize)> = None;
    for row in 0..rows {
        let run = span(row);
        match (current, run) {
            (Some((open, _)), Some(run)) if open == run => {}
            _ => {
                if let Some((open, from)) = current {
                    push(open, from, row);
                }
                current = run.map(|run| (run, row));
            }
        }
    }
    if let Some((open, from)) = current {
        push(open, from, rows);
    }
    edges
}

#[cfg(test)]
mod tests {
    use super::*;

    fn circle_area(r: f64, margin: f64) -> FloatArea {
        let path = kurbo::Shape::into_path(kurbo::Circle::new((50.0, 50.0), r), TOLERANCE);
        FloatArea {
            edges: outline_edges(&path),
            margin,
        }
    }

    fn float(area: FloatArea, is_left: bool, x: f32, width: f32) -> PlacedFloat {
        PlacedFloat {
            is_left,
            x,
            y: 0.0,
            width,
            height: 100.0,
            area: Some(area),
        }
    }

    fn close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 0.02,
            "{actual} is not {expected}"
        );
    }

    /// The edges of the 20px line from `top` in a 200px box beside `floats`.
    fn edges_at(floats: &[PlacedFloat], top: f32) -> (f32, f32) {
        line_edges(floats, 0.0, 200.0, top, top + 20.0).expect("a float area")
    }

    /// Edge 156: `circle(50%)` on a 100px float, 20px lines start at 90, 98.98, 100.
    #[test]
    fn circle_lines_follow_the_curve() {
        let floats = [float(circle_area(50.0, 0.0), true, 0.0, 100.0)];
        for (top, x) in [(0.0, 90.0), (20.0, 98.98), (40.0, 100.0), (80.0, 90.0)] {
            let (left, right) = edges_at(&floats, top);
            close(left, x);
            close(right, 200.0);
        }
    }

    /// Edge 156: `shape-margin: 10px` grows the circle to radius 60 (101.95, 109.16)
    /// inside a 120px margin box, and stops at a 100px one.
    #[test]
    fn shape_margin_grows_the_area_inside_the_margin_box() {
        let wide = [float(circle_area(50.0, 10.0), true, 0.0, 120.0)];
        close(edges_at(&wide, 0.0).0, 101.95);
        close(edges_at(&wide, 20.0).0, 109.16);
        let tight = [float(circle_area(50.0, 10.0), true, 0.0, 100.0)];
        close(edges_at(&tight, 0.0).0, 100.0);
    }

    /// Edge 156: a right float's circle leaves lines ending at 110 and 100, and a
    /// triangle with `shape-margin: 5px` reaches 148.82 (the disc around an edge).
    #[test]
    fn right_floats_bound_the_line_end() {
        let circle = [float(circle_area(50.0, 0.0), false, 100.0, 100.0)];
        close(edges_at(&circle, 0.0).1, 110.0);
        close(edges_at(&circle, 40.0).1, 100.0);
        let mut triangle = BezPath::new();
        triangle.move_to((100.0, 0.0));
        triangle.line_to((0.0, 50.0));
        triangle.line_to((100.0, 100.0));
        triangle.close_path();
        let area = FloatArea {
            edges: outline_edges(&triangle),
            margin: 5.0,
        };
        let floats = [float(area, false, 100.0, 100.0)];
        close(edges_at(&floats, 0.0).1, 148.82);
    }

    /// A line the area does not reach is not shortened, though the margin box is there;
    /// a float without an area is its margin box.
    #[test]
    fn an_empty_band_leaves_the_line_alone() {
        let mut top_half = BezPath::new();
        top_half.move_to((0.0, 0.0));
        top_half.line_to((100.0, 0.0));
        top_half.line_to((0.0, 50.0));
        top_half.close_path();
        let area = FloatArea {
            edges: outline_edges(&top_half),
            margin: 0.0,
        };
        let floats = [float(area, true, 0.0, 100.0)];
        close(edges_at(&floats, 20.0).0, 60.0);
        close(edges_at(&floats, 60.0).0, 0.0);
        let plain = [PlacedFloat {
            area: None,
            ..float(circle_area(1.0, 0.0), true, 0.0, 100.0)
        }];
        assert_eq!(line_edges(&plain, 100.0, 200.0, 0.0, 20.0), None);
    }

    /// Alpha above the threshold only: a 0.4-alpha column is outside at 0.5.
    #[test]
    fn raster_threshold_keeps_only_alpha_above_it() {
        let mut data = Vec::new();
        for _ in 0..4 {
            data.extend([0, 0, 0, 255, 0, 0, 0, 102, 0, 0, 0, 0, 0, 0, 0, 0]);
        }
        let image = RasterImageData::new(4, 4, std::sync::Arc::new(data));
        let content = ReferenceBox {
            x: 0.0,
            y: 0.0,
            width: 40.0,
            height: 40.0,
        };
        let reach = |threshold| {
            FloatArea {
                edges: raster_edges(&image, content, threshold),
                margin: 0.0,
            }
            .reach(0.0, 10.0, 1.0)
        };
        assert_eq!(reach(0.5), Some(10.0));
        assert_eq!(reach(0.3), Some(20.0));
        assert_eq!(reach(1.0), None);
    }
}
