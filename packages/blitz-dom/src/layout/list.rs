use blitz_traits::node_id::NodeId;
use markup5ever::local_name;
use parley::FontFamily;
use style::computed_values::list_style_position::T as ListStylePosition;
use style::computed_values::list_style_type::T as ListStyleType;

use crate::{
    BaseDocument,
    node::{ListItemLayout, ListItemLayoutPosition, Marker},
    stylo_to_parley,
};

pub(super) const BULLET_FONT_FAMILY: &str = "Bullet, monospace, sans-serif";

/// Give each list item among `node_id`'s children the marker the counters pass
/// (`layout::counters`) resolved for it.
pub(super) fn collect_list_item_children(doc: &mut BaseDocument, node_id: NodeId) {
    let children = doc.nodes[node_id].flat_children().into_owned();
    for child in children.into_iter() {
        let layout = node_list_item_child(doc, child);
        if let Some(element_data) = doc.nodes[child].element_data_mut() {
            element_data.list_item_data = layout.map(Box::new);
        }
    }
}

// Return a child node which is of display: list-item
fn node_list_item_child(doc: &mut BaseDocument, child_id: NodeId) -> Option<ListItemLayout> {
    let node = &doc.nodes[child_id];

    // We only care about elements with display: list-item (li's have this automatically)
    if !node
        .primary_styles()
        .is_some_and(|style| style.get_box().display.is_list_item())
    {
        return None;
    }

    //Break on container elements when already in a list
    if node
        .element_data()
        .map(|element_data| {
            matches!(
                element_data.name.local,
                local_name!("ol") | local_name!("ul"),
            )
        })
        .unwrap_or(false)
    {
        return None;
    };

    let styles = node.primary_styles().unwrap();
    let list_style_type = styles.clone_list_style_type();
    let list_style_position = styles.clone_list_style_position();
    let marker = doc.counter_values.list_marker(child_id)?.clone();

    let position = match list_style_position {
        ListStylePosition::Inside => ListItemLayoutPosition::Inside,
        ListStylePosition::Outside => {
            let mut parley_style = stylo_to_parley::style(child_id, &styles);

            if let Some(font_family) = font_for_bullet_style(list_style_type) {
                parley_style.font_family = font_family;
            }

            // Create a parley tree builder
            let mut font_ctx = doc.font_ctx.lock().unwrap();
            let mut builder = doc.layout_ctx.tree_builder(
                &mut font_ctx,
                doc.viewport.scale(),
                true,
                &parley_style,
            );

            match &marker {
                Marker::Char(char) => {
                    let mut buf = [0u8; 4];
                    builder.push_text(char.encode_utf8(&mut buf));
                }
                Marker::String(str) => builder.push_text(str),
            };

            let mut layout = builder.build().0;
            let width = layout.calculate_content_widths().max;
            layout.break_all_lines(Some(width));

            ListItemLayoutPosition::Outside(Box::new(layout))
        }
    };

    Some(ListItemLayout { marker, position })
}

// Override the font to our specific bullet font when rendering bullets
fn font_for_bullet_style(list_style_type: ListStyleType) -> Option<FontFamily<'static>> {
    if list_style_type.0.is_bullet() {
        return Some(BULLET_FONT_FAMILY.into());
    }

    None
}
