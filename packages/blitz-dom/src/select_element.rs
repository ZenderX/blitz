//! What a drop-down `<select>` displays: its list of options, the option it shows, and
//! each option's label.
//!
//! The shown option follows markup (the `selected` content attribute) unless the host,
//! which may keep script selectedness of its own, overrides it with
//! [`BaseDocument::set_select_display`]. There is no popup.

use blitz_traits::node_id::NodeId;
use markup5ever::local_name;
use style::selector_parser::RestyleDamage;

use crate::BaseDocument;

/// Width of a drop-down select's arrow box, in ems of the select's font size. Layout
/// reserves it beside the widest label and paint draws the arrow inside it.
pub const SELECT_ARROW_WIDTH_EM: f32 = 1.0;

/// What a `<select>` displays, as set by [`BaseDocument::set_select_display`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectDisplay {
    /// The markup selectedness: the last option with `selected`, else the first option
    /// that is not disabled.
    #[default]
    Markup,
    /// This option while it is in the select's list of options, nothing once it leaves.
    Option(NodeId),
    /// No option: a blank label, as for `selectedIndex = -1`.
    Blank,
}

impl BaseDocument {
    /// Set what the select `select_id` displays, for a host that keeps selectedness the
    /// markup does not show (script `selectedIndex`, `value`, `option.selected`). A change
    /// marks the select for repaint. Returns `false`, changing nothing, when the node is
    /// not a `<select>`.
    pub fn set_select_display(&mut self, select_id: NodeId, display: SelectDisplay) -> bool {
        let node = &mut self.nodes[select_id];
        if !node.data.is_element_with_tag_name(&local_name!("select")) {
            return false;
        }
        let element = node
            .element_data_mut()
            .expect("a select node is an element");
        if element.select_display != display {
            element.select_display = display;
            node.insert_damage(RestyleDamage::REPAINT);
            self.shell_provider.request_redraw();
        }
        true
    }

    /// The select's list of options in tree order: its `<option>` children and the
    /// `<option>` children of its `<optgroup>` children.
    pub fn select_options(&self, select_id: NodeId) -> Vec<NodeId> {
        let is_option = |id: &NodeId| {
            self.nodes[*id]
                .data
                .is_element_with_tag_name(&local_name!("option"))
        };
        let mut options = Vec::new();
        for &child_id in &self.nodes[select_id].children {
            let child = &self.nodes[child_id];
            if is_option(&child_id) {
                options.push(child_id);
            } else if child
                .data
                .is_element_with_tag_name(&local_name!("optgroup"))
            {
                options.extend(child.children.iter().copied().filter(is_option));
            }
        }
        options
    }

    /// The option a drop-down select displays, per its [`SelectDisplay`]. For `Markup`: the
    /// last option carrying `selected` (markup with several keeps the last, per the
    /// selectedness setting algorithm), else the first option that is not disabled.
    pub fn selected_option(&self, select_id: NodeId) -> Option<NodeId> {
        let options = self.select_options(select_id);
        let display = self.nodes[select_id]
            .element_data()
            .map_or(SelectDisplay::Markup, |element| element.select_display);
        match display {
            SelectDisplay::Option(option_id) => options.contains(&option_id).then_some(option_id),
            SelectDisplay::Blank => None,
            SelectDisplay::Markup => options
                .iter()
                .rev()
                .find(|&&id| self.nodes[id].data.has_attr(local_name!("selected")))
                .or_else(|| options.iter().find(|&&id| !self.option_is_disabled(id)))
                .copied(),
        }
    }

    /// An option's label: its non-empty `label` attribute, else its text with ASCII
    /// whitespace stripped and collapsed.
    pub fn option_label(&self, option_id: NodeId) -> String {
        let option = &self.nodes[option_id];
        match option.attr(local_name!("label")) {
            Some(label) if !label.is_empty() => label.to_string(),
            _ => option
                .text_content()
                .split_ascii_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
        }
    }

    /// The width of the widest option label of `node_id`, in CSS px, or `None` when the
    /// node is not a `<select>`. A drop-down is as wide as its widest option, whichever
    /// one is selected.
    pub(crate) fn select_label_width(&self, node_id: NodeId) -> Option<f32> {
        if !self.nodes[node_id]
            .data
            .is_element_with_tag_name(&local_name!("select"))
        {
            return None;
        }
        let scale = self.viewport.scale();
        let width = self
            .select_options(node_id)
            .into_iter()
            .map(|option_id| {
                self.layout_control_text(node_id, &self.option_label(option_id))
                    .full_width()
                    / scale
            })
            .fold(0.0, f32::max);
        Some(width)
    }

    /// An option is disabled by its own `disabled` attribute or by its `<optgroup>`'s.
    fn option_is_disabled(&self, option_id: NodeId) -> bool {
        let option = &self.nodes[option_id];
        option.data.has_attr(local_name!("disabled"))
            || option.parent.is_some_and(|parent_id| {
                let parent = &self.nodes[parent_id].data;
                parent.is_element_with_tag_name(&local_name!("optgroup"))
                    && parent.has_attr(local_name!("disabled"))
            })
    }
}
