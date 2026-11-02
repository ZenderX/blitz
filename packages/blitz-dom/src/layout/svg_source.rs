//! The source an inline `<svg>` subtree is parsed from. usvg reads attributes (and the
//! subtree's own `<style>`/`style`), never the document's cascade, so the subtree's
//! markup is written out with the cascade baked in:
//!
//! - `currentColor` in an attribute becomes the element's computed `color`;
//! - each [`SVG_SOURCE_PROPERTIES`] property is written as an attribute from its
//!   computed value, in place of the attribute of the same name (which the cascade
//!   already holds as a presentation hint);
//! - a transformed element's `transform-origin` is resolved to user units against the
//!   reference box its `transform-box` selects (css-transforms-1), which usvg, knowing
//!   only the view box, cannot do. The fill and stroke boxes come from a first parse.

use std::collections::HashMap;

use html_escape::{encode_quoted_attribute_to_string, encode_text_to_string};
use markup5ever::{local_name, ns};
use style::properties::ComputedValues;
use style::values::computed::TransformBox;
use style::values::generics::basic_shape::FillRule;
use style_traits::values::ToCss as _;

use crate::node::SvgImageData;
use crate::stylo::SVG_SOURCE_PROPERTIES;
use crate::{BaseDocument, Node, NodeData, NodeId};

/// An element's fill box and stroke box, in user units of its own coordinate system
/// (before its `transform`).
type ObjectBoxes = HashMap<NodeId, (usvg::Rect, usvg::Rect)>;

/// Parse the inline `<svg>` `svg_id` from its source. A subtree whose transformed
/// elements use a `fill-box`/`stroke-box` reference box is parsed twice: once to
/// measure those elements, then with their origins resolved.
pub(crate) fn parse_inline_svg_source(
    doc: &BaseDocument,
    svg_id: NodeId,
) -> Result<SvgImageData, (usvg::Error, String)> {
    let measured = boxed_elements(doc, svg_id);
    let boxes = if measured.is_empty() {
        ObjectBoxes::new()
    } else {
        let source = SourceWriter::write(doc, svg_id, Pass::Measure(&measured));
        let svg = parse(&source).map_err(|err| (err, source))?;
        measured
            .iter()
            .filter_map(|(&node_id, id)| {
                let node = svg.tree.node_by_id(id)?;
                Some((node_id, (node.bounding_box(), node.stroke_bounding_box())))
            })
            .collect()
    };
    let source = SourceWriter::write(doc, svg_id, Pass::Final(&boxes));
    parse(&source).map_err(|err| (err, source))
}

fn parse(source: &str) -> Result<SvgImageData, usvg::Error> {
    crate::util::parse_svg_image(source.as_bytes())
}

/// Whether a restyle from `old` to `new` changes the source of the inline `<svg>` an
/// element belongs to.
pub(crate) fn source_changed(old: &ComputedValues, new: &ComputedValues) -> bool {
    let (old_svg, new_svg) = (old.get_svg(), new.get_svg());
    let (old_box, new_box) = (old.get_box(), new.get_box());
    old.clone_color() != new.clone_color()
        || old.get_inherited_svg() != new.get_inherited_svg()
        || old_svg.flood_color != new_svg.flood_color
        || old_svg.flood_opacity != new_svg.flood_opacity
        || old_svg.lighting_color != new_svg.lighting_color
        || old_box.transform_box != new_box.transform_box
        || old_box.transform_origin != new_box.transform_origin
}

/// The elements whose `transform-origin` resolves against their fill or stroke box, by
/// the `id` the measuring parse finds them under: their own, or one written for it.
fn boxed_elements(doc: &BaseDocument, svg_id: NodeId) -> HashMap<NodeId, String> {
    let mut boxed = HashMap::new();
    let mut stack = vec![svg_id];
    while let Some(node_id) = stack.pop() {
        let node = &doc.nodes[node_id];
        stack.extend(node.children.iter().copied());
        if node_id != svg_id
            && is_transformed_svg_element(node)
            && node
                .primary_styles()
                .is_some_and(|style| reference_box_kind(&style) != ReferenceBox::ViewBox)
        {
            let id = node
                .attr(local_name!("id"))
                .map_or_else(|| synthetic_id(node_id), str::to_owned);
            boxed.insert(node_id, id);
        }
    }
    boxed
}

fn synthetic_id(node_id: NodeId) -> String {
    format!("blitz-svg-box-{}", node_id.as_u64())
}

fn is_transformed_svg_element(node: &Node) -> bool {
    node.element_data()
        .is_some_and(|el| el.name.ns == ns!(svg) && node.attr(local_name!("transform")).is_some())
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)] // The variants are named after the CSS keywords
enum ReferenceBox {
    FillBox,
    StrokeBox,
    ViewBox,
}

/// An SVG element has no CSS layout box, so `content-box` is its fill box and
/// `border-box` its stroke box (css-transforms-1 `transform-box`).
fn reference_box_kind(style: &ComputedValues) -> ReferenceBox {
    match style.get_box().clone_transform_box() {
        TransformBox::FillBox | TransformBox::ContentBox => ReferenceBox::FillBox,
        TransformBox::StrokeBox | TransformBox::BorderBox => ReferenceBox::StrokeBox,
        TransformBox::ViewBox => ReferenceBox::ViewBox,
    }
}

enum Pass<'a> {
    /// Write `id`s on the elements to measure; resolve no origins.
    Measure(&'a HashMap<NodeId, String>),
    /// Resolve origins, against these measured boxes where needed.
    Final(&'a ObjectBoxes),
}

struct SourceWriter<'a> {
    doc: &'a BaseDocument,
    svg_id: NodeId,
    pass: Pass<'a>,
    out: String,
}

impl<'a> SourceWriter<'a> {
    fn write(doc: &'a BaseDocument, svg_id: NodeId, pass: Pass<'a>) -> String {
        let mut writer = Self {
            doc,
            svg_id,
            pass,
            out: String::new(),
        };
        writer.node(svg_id);
        // usvg needs the SVG namespace declared; markup from the HTML parser has none.
        if !writer.out.contains("xmlns") {
            writer.out =
                writer
                    .out
                    .replacen("<svg", "<svg xmlns=\"http://www.w3.org/2000/svg\"", 1);
        }
        writer.out
    }

    fn node(&mut self, node_id: NodeId) {
        let node = &self.doc.nodes[node_id];
        match &node.data {
            NodeData::Text(text) => {
                encode_text_to_string(&text.content, &mut self.out);
            }
            NodeData::Element(_) => self.element(node),
            _ => {}
        }
    }

    fn element(&mut self, node: &Node) {
        let el = node.element_data().unwrap();
        let style = node.primary_styles();
        let is_svg = el.name.ns == ns!(svg);
        // Only a styled SVG element has computed values to write over its attributes.
        let cascaded = style.as_ref().filter(|_| is_svg);
        let is_transformed = node.id != self.svg_id && is_transformed_svg_element(node);
        let current_color = style.as_ref().map(|s| s.clone_color().to_css_string());

        self.out.push('<');
        self.out.push_str(&el.name.local);
        for attr in el.attrs() {
            let name = &*attr.name.local;
            let is_replaced = cascaded.is_some()
                && SVG_SOURCE_PROPERTIES.contains(&name)
                && (name != "transform-origin" || is_transformed);
            if is_replaced {
                continue;
            }
            let value = match &current_color {
                Some(color) if attr.value.contains("currentColor") => {
                    attr.value.replace("currentColor", color)
                }
                _ => attr.value.to_string(),
            };
            self.attribute(name, &value);
        }

        if let Some(style) = cascaded {
            self.cascaded_attributes(node, style);
            if is_transformed {
                self.transform_origin(node, style);
            }
        }
        if let Pass::Measure(ids) = self.pass
            && node.attr(local_name!("id")).is_none()
            && let Some(id) = ids.get(&node.id)
        {
            let id = id.clone();
            self.attribute("id", &id);
        }

        if node.children.is_empty() {
            self.out.push_str(" />");
            return;
        }
        self.out.push('>');
        for &child_id in &node.children {
            self.node(child_id);
        }
        self.out.push_str("</");
        self.out.push_str(&el.name.local);
        self.out.push('>');
    }

    fn attribute(&mut self, name: &str, value: &str) {
        self.out.push(' ');
        self.out.push_str(name);
        self.out.push_str("=\"");
        encode_quoted_attribute_to_string(value, &mut self.out);
        self.out.push('"');
    }

    /// The [`SVG_SOURCE_PROPERTIES`] values usvg would not arrive at by itself: an
    /// inherited property where it differs from the parent's (the root's from the
    /// initial value), any other where it differs from the initial value.
    fn cascaded_attributes(&mut self, node: &Node, style: &ComputedValues) {
        let parent_style = (node.id != self.svg_id)
            .then(|| node.parent.and_then(|p| self.doc.nodes[p].primary_styles()))
            .flatten();
        let initial = SourceValues::initial();
        let inherited_from = parent_style.map(|parent| SourceValues::of(&parent));
        let own = SourceValues::of(style);
        let base = inherited_from.as_ref().unwrap_or(&initial);

        if own.clip_rule != base.clip_rule {
            self.attribute("clip-rule", own.clip_rule);
        }
        if own.color_interpolation_filters != base.color_interpolation_filters {
            self.attribute(
                "color-interpolation-filters",
                own.color_interpolation_filters,
            );
        }
        if own.flood_color != initial.flood_color {
            self.attribute("flood-color", &own.flood_color);
        }
        if own.flood_opacity != initial.flood_opacity {
            self.attribute("flood-opacity", &own.flood_opacity);
        }
        if own.lighting_color != initial.lighting_color {
            self.attribute("lighting-color", &own.lighting_color);
        }
    }

    /// `transform-origin` in user units: the computed origin resolved against the
    /// element's `transform-box` reference box.
    fn transform_origin(&mut self, node: &Node, style: &ComputedValues) {
        let reference = match (&self.pass, reference_box_kind(style)) {
            (_, ReferenceBox::ViewBox) => view_box(self.doc, node),
            (Pass::Measure(_), _) => return,
            (Pass::Final(boxes), kind) => {
                let Some(&(fill, stroke)) = boxes.get(&node.id) else {
                    return;
                };
                let rect = if kind == ReferenceBox::FillBox {
                    fill
                } else {
                    stroke
                };
                [rect.x(), rect.y(), rect.width(), rect.height()]
            }
        };
        let [x, y, width, height] = reference.map(style::values::computed::CSSPixelLength::new);
        let origin = &style.get_box().transform_origin;
        let origin_x = (x + origin.horizontal.resolve(width)).px();
        let origin_y = (y + origin.vertical.resolve(height)).px();
        self.attribute("transform-origin", &format!("{origin_x} {origin_y}"));
    }
}

/// The `view-box` reference box of an element: its nearest SVG viewport's `viewBox`,
/// or the viewport's `width`/`height` in user units (for the outermost `<svg>` without
/// them, its laid-out content box).
fn view_box(doc: &BaseDocument, node: &Node) -> [f32; 4] {
    let mut ancestor = node.parent;
    while let Some(id) = ancestor {
        let candidate = &doc.nodes[id];
        let Some(el) = candidate.element_data() else {
            break;
        };
        if el.name.ns == ns!(svg) && el.name.local == local_name!("svg") {
            return viewport_rect(doc, candidate);
        }
        ancestor = candidate.parent;
    }
    [0.0; 4]
}

fn viewport_rect(doc: &BaseDocument, svg: &Node) -> [f32; 4] {
    let numbers = |value: &str| -> Vec<f32> {
        value
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|part| !part.is_empty())
            .filter_map(|part| part.parse().ok())
            .collect()
    };
    if let Some(view_box) = svg.attr(local_name!("viewBox")).map(numbers)
        && let [x, y, width, height] = view_box[..]
    {
        return [x, y, width, height];
    }
    let length = |value: Option<&str>| -> Option<f32> {
        let value = value?.trim();
        value.strip_suffix("px").unwrap_or(value).parse().ok()
    };
    let is_outermost = svg
        .parent
        .and_then(|p| doc.nodes[p].element_data())
        .is_none_or(|el| el.name.ns != ns!(svg));
    let layout = svg.final_layout();
    let content = |size: f32, start: f32, end: f32| size - start - end;
    let width = length(svg.attr(local_name!("width"))).or(is_outermost.then(|| {
        let (b, p) = (layout.border, layout.padding);
        content(layout.size.width, b.left + p.left, b.right + p.right)
    }));
    let height = length(svg.attr(local_name!("height"))).or(is_outermost.then(|| {
        let (b, p) = (layout.border, layout.padding);
        content(layout.size.height, b.top + p.top, b.bottom + p.bottom)
    }));
    [0.0, 0.0, width.unwrap_or(0.0), height.unwrap_or(0.0)]
}

/// The attribute values of the non-geometry [`SVG_SOURCE_PROPERTIES`], as usvg parses
/// them. `color-interpolation-filters: auto` is `linearRGB`, which usvg and Chrome use
/// for it.
struct SourceValues {
    clip_rule: &'static str,
    color_interpolation_filters: &'static str,
    flood_color: String,
    flood_opacity: String,
    lighting_color: String,
}

impl SourceValues {
    /// The initial values, as [`Self::of`] writes them.
    fn initial() -> Self {
        Self {
            clip_rule: "nonzero",
            color_interpolation_filters: "linearRGB",
            flood_color: "rgba(0, 0, 0, 1)".to_owned(),
            flood_opacity: "1".to_owned(),
            lighting_color: "rgba(255, 255, 255, 1)".to_owned(),
        }
    }

    fn of(style: &ComputedValues) -> Self {
        use style::computed_values::color_interpolation_filters::T as Cif;
        let inherited = style.get_inherited_svg();
        let svg = style.get_svg();
        let current_color = style.clone_color();
        let color = |c: &style::values::computed::Color| {
            let [r, g, b, a] = *c
                .resolve_to_absolute(&current_color)
                .to_color_space(style::color::ColorSpace::Srgb)
                .raw_components();
            let channel = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round();
            format!(
                "rgba({}, {}, {}, {})",
                channel(r),
                channel(g),
                channel(b),
                a.clamp(0.0, 1.0)
            )
        };
        Self {
            clip_rule: match inherited.clip_rule {
                FillRule::Nonzero => "nonzero",
                FillRule::Evenodd => "evenodd",
            },
            color_interpolation_filters: match inherited.color_interpolation_filters {
                Cif::Auto | Cif::Linearrgb => "linearRGB",
                Cif::Srgb => "sRGB",
            },
            flood_color: color(&svg.flood_color),
            flood_opacity: svg.flood_opacity.to_string(),
            lighting_color: color(&svg.lighting_color),
        }
    }
}
