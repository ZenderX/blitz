use blitz_dom::util::ToColorColor as _;
use blitz_dom::{BaseDocument, ElementData, Node};
use kurbo::Rect;
use style::color::AbsoluteColor;
pub(crate) use style::computed_values::filter::single_value::T as StyloFilter;
use style::values::computed::url::ComputedUrl;

use anyrender::filters::{EdgeMode, Filter, FilterEffect};

/// A `filter`/`backdrop-filter` list as one filter graph, and the region its result
/// covers when the list references SVG `<filter>` elements (`None` for filter
/// functions alone, whose output stays where the element paints).
pub(crate) struct ConvertedFilter {
    pub filter: Filter,
    pub region: Option<Rect>,
}

/// Convert a filter list, expanding each `url()` into the primitives of the `<filter>`
/// it references. `bbox` is the element's border box in device px and `scale` the
/// device scale, which resolve the referenced filter's region.
///
/// A `url()` that does not resolve to a `<filter>` this can draw makes the whole list
/// `None`: filter-effects-1 ignores the whole chain when a reference is not a filter.
pub(crate) fn convert_filters(
    filters: &[StyloFilter],
    doc: &BaseDocument,
    bbox: Rect,
    scale: f64,
) -> Option<ConvertedFilter> {
    if filters.is_empty() {
        return None;
    }

    let mut effects = Vec::with_capacity(filters.len());
    let mut region: Option<Rect> = None;
    for filter in filters {
        if let StyloFilter::Url(url) = filter {
            let Some(svg_filter) = svg_filter(doc, url, bbox, scale) else {
                #[cfg(feature = "tracing")]
                tracing::warn!(?url, "filter: url() is not a <filter> that can be drawn");
                return None;
            };
            effects.extend(svg_filter.effects);
            region = Some(region.map_or(svg_filter.region, |r| r.union(svg_filter.region)));
        } else {
            effects.extend(convert_single_filter(filter));
        }
    }

    Some(ConvertedFilter {
        filter: Filter::linear_list(effects.into_iter()),
        region,
    })
}

pub(crate) fn convert_single_filter(filter: &StyloFilter) -> Option<FilterEffect> {
    Some(match filter {
        StyloFilter::Blur(radius) => FilterEffect::blur(radius.px()),
        StyloFilter::Brightness(amount) => FilterEffect::brightness(amount.0),
        StyloFilter::Contrast(amount) => FilterEffect::contrast(amount.0),
        StyloFilter::Grayscale(amount) => FilterEffect::grayscale(amount.0),
        StyloFilter::HueRotate(angle) => FilterEffect::hue_rotate(angle.radians()),
        StyloFilter::Invert(amount) => FilterEffect::invert(amount.0),
        StyloFilter::Opacity(amount) => FilterEffect::opacity(amount.0),
        StyloFilter::Saturate(amount) => FilterEffect::saturate(amount.0),
        StyloFilter::Sepia(amount) => FilterEffect::sepia(amount.0),
        StyloFilter::DropShadow(shadow) => FilterEffect::drop_shadow(
            shadow.horizontal.px(),
            shadow.vertical.px(),
            shadow.blur.px(),
            // TODO: pass in correct currentColor
            shadow
                .color
                .resolve_to_absolute(&AbsoluteColor::BLACK)
                .as_color_color(),
        ),
        StyloFilter::Url(_) => return None,
    })
}

/// The primitives of a referenced `<filter>` element and its filter region.
struct SvgFilter {
    effects: Vec<FilterEffect>,
    region: Rect,
}

/// Resolve `url` to a `<filter>` element of this document and convert its primitives.
///
/// Only same-document references resolve. The primitives must form a chain (each one
/// reads the previous result), as `Filter::linear_list` wires them, and each must be one
/// the filter graph can express: `feFlood`, `feGaussianBlur`, `feOffset` and
/// `feDropShadow`. Primitive values are in user space (CSS px), like the CSS filter
/// functions above; `primitiveUnits="objectBoundingBox"` is not supported.
fn svg_filter(doc: &BaseDocument, url: &ComputedUrl, bbox: Rect, scale: f64) -> Option<SvgFilter> {
    let url = url.url()?;
    let id = url.fragment()?;
    let mut document_url = doc.url().clone();
    document_url.set_fragment(Some(id));
    if **url != document_url {
        return None;
    }

    let filter = doc.get_node(doc.get_element_by_id(id)?)?;
    let element = filter.element_data()?;
    if element.name.ns != blitz_dom::ns!(svg) || &*element.name.local != "filter" {
        return None;
    }
    if attr(element, "primitiveUnits") == Some("objectBoundingBox") {
        return None;
    }

    let mut effects = Vec::new();
    let mut previous_result: Option<&str> = None;
    for &child_id in &filter.children {
        let child = doc.get_node(child_id)?;
        let Some(primitive) = child.element_data() else {
            continue;
        };
        let reads_previous = match attr(primitive, "in") {
            None => true,
            Some("SourceGraphic") => effects.is_empty(),
            Some(name) => previous_result == Some(name),
        };
        if !reads_previous {
            return None;
        }
        effects.push(svg_filter_primitive(child)?);
        previous_result = attr(primitive, "result");
    }

    Some(SvgFilter {
        effects,
        region: filter_region(element, bbox, scale),
    })
}

fn svg_filter_primitive(node: &Node) -> Option<FilterEffect> {
    let element = node.element_data()?;
    let number = |name: &str, default: f32| -> Option<f32> {
        attr(element, name).map_or(Some(default), |value| value.trim().parse().ok())
    };
    // A single `stdDeviation`, or an equal pair: the graph blurs both axes alike.
    let std_deviation = |default: f32| -> Option<f32> {
        let Some(value) = attr(element, "stdDeviation") else {
            return Some(default);
        };
        let mut parts = value
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|part| !part.is_empty())
            .map(|part| part.parse::<f32>().ok());
        let x = parts.next()??;
        let y = parts.next().unwrap_or(Some(x))?;
        (x == y && x >= 0.0 && parts.next().is_none()).then_some(x)
    };

    Some(match &*element.name.local {
        "feFlood" => FilterEffect::Flood(flood_color(node)?),
        "feGaussianBlur" => {
            let edge_mode = match attr(element, "edgeMode") {
                None | Some("none") => EdgeMode::None,
                Some("duplicate") => EdgeMode::Duplicate,
                Some("wrap") => EdgeMode::Wrap,
                Some("mirror") => EdgeMode::Mirror,
                Some(_) => return None,
            };
            let mut blur = FilterEffect::blur(std_deviation(0.0)?);
            if let FilterEffect::GaussianBlur(filter) = &mut blur {
                filter.edge_mode = edge_mode;
            }
            blur
        }
        "feOffset" => FilterEffect::Offset(kurbo::Vec2::new(
            f64::from(number("dx", 0.0)?),
            f64::from(number("dy", 0.0)?),
        )),
        "feDropShadow" => FilterEffect::drop_shadow(
            number("dx", 2.0)?,
            number("dy", 2.0)?,
            std_deviation(2.0)?,
            flood_color(node)?,
        ),
        _ => return None,
    })
}

/// `flood-color` with `flood-opacity` applied, from the primitive's computed style.
fn flood_color(node: &Node) -> Option<peniko::Color> {
    let style = node.primary_styles()?;
    let svg = style.get_svg();
    let color = svg
        .flood_color
        .resolve_to_absolute(&style.get_inherited_text().color)
        .as_color_color();
    Some(color.multiply_alpha(svg.flood_opacity.clamp(0.0, 1.0)))
}

/// The filter region (`x`, `y`, `width`, `height` of `<filter>`) in device px, for an
/// element whose bounding box is `bbox` (its border box). The region defaults to
/// -10%/-10%/120%/120% of the bounding box (filter-effects-1 `filter` element).
///
/// With `filterUnits="userSpaceOnUse"` a number is CSS px from the border box origin;
/// percentages resolve against the bounding box in both units.
fn filter_region(filter: &ElementData, bbox: Rect, scale: f64) -> Rect {
    let is_user_space = attr(filter, "filterUnits") == Some("userSpaceOnUse");
    let coordinate = |name: &str, default: f64, origin: f64, size: f64| -> f64 {
        let value = attr(filter, name).map(str::trim);
        let fraction = |v: &str| v.strip_suffix('%')?.parse::<f64>().ok().map(|p| p / 100.0);
        match value {
            Some(v) if v.ends_with('%') => origin + fraction(v).unwrap_or(default) * size,
            Some(v) => match v.strip_suffix("px").unwrap_or(v).parse::<f64>() {
                Ok(n) if is_user_space => origin + n * scale,
                Ok(n) => origin + n * size,
                Err(_) => origin + default * size,
            },
            None => origin + default * size,
        }
    };
    let x = coordinate("x", -0.1, bbox.x0, bbox.width());
    let y = coordinate("y", -0.1, bbox.y0, bbox.height());
    let width = coordinate("width", 1.2, 0.0, bbox.width());
    let height = coordinate("height", 1.2, 0.0, bbox.height());
    Rect::new(x, y, x + width, y + height)
}

/// An attribute by its (case-sensitive SVG) local name.
fn attr<'a>(element: &'a ElementData, name: &str) -> Option<&'a str> {
    element
        .attrs()
        .iter()
        .find(|attr| &*attr.name.local == name)
        .map(|attr| &*attr.value)
}
