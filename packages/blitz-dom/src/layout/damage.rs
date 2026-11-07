use blitz_traits::node_id::NodeId;
use std::ops::Range;

use super::fixed::establishes_fixed_containing_block;
use super::writing_mode;
use crate::Node;
use crate::net::ResourceHandler;
use crate::node::{LayoutFlow, NodeFlags};
use crate::{
    BaseDocument, net::ImageHandler, node::ImageResourceData, node::Status, util::ImageLayerKind,
};
use style::properties::ComputedValues;
use style::properties::generated::longhands::position::computed_value::T as Position;
use style::selector_parser::RestyleDamage;
use style::url::ComputedUrl;
use style::values::computed::Float;
use style::values::computed::Overflow;
use style::values::computed::basic_shape::ClipPath;
use style::values::computed::image::Image as ComputedImage;
use style::values::generics::basic_shape::ShapeOutside;
use style::values::generics::image::Image as StyloImage;
use style::values::specified::align::AlignFlags;
use style::values::specified::box_::DisplayInside;
use style::values::specified::box_::DisplayOutside;
use taffy::Rect;

pub(crate) const CONSTRUCT_BOX: RestyleDamage =
    RestyleDamage::from_bits_retain(0b_0000_0000_0001_0000);
pub(crate) const CONSTRUCT_FC: RestyleDamage =
    RestyleDamage::from_bits_retain(0b_0000_0000_0010_0000);
pub(crate) const CONSTRUCT_DESCENDENT: RestyleDamage =
    RestyleDamage::from_bits_retain(0b_0000_0000_0100_0000);

pub(crate) const ONLY_RELAYOUT: RestyleDamage =
    RestyleDamage::from_bits_retain(0b_0000_0000_0000_1000);

pub(crate) const ALL_DAMAGE: RestyleDamage =
    RestyleDamage::from_bits_retain(0b_0000_0000_0111_1111);

impl BaseDocument {
    pub(crate) fn propagate_damage_flags(
        &mut self,
        node_id: NodeId,
        damage_from_parent: RestyleDamage,
    ) -> RestyleDamage {
        let mut damage = if let Some(data) = self.nodes[node_id]
            .stylo_element_data_opt_mut()
            .and_then(|s| s.get_mut())
        {
            data.damage
        } else {
            return RestyleDamage::empty();
        };
        damage |= damage_from_parent;

        // Skip subtrees which contain no damage. Anonymous nodes are never
        // skipped themselves because damage marking walks the DOM parent
        // chain, which bypasses anonymous boxes: a damaged node's flagged
        // ancestors may reach it only through an unflagged anonymous wrapper.
        {
            let node = &self.nodes[node_id];
            if damage.is_empty() && !node.has_damaged_descendants() && !node.is_anonymous() {
                return RestyleDamage::empty();
            }
        }

        let damage_for_children = RestyleDamage::empty();
        let children = self.nodes[node_id].take_children_for_iteration();
        let layout_children = std::mem::take(self.nodes[node_id].layout_children.get_mut());
        let use_layout_children = self.nodes[node_id].should_traverse_layout_children();
        if use_layout_children {
            let layout_children = layout_children.as_ref().unwrap();
            for child in layout_children.iter() {
                damage |= self.propagate_damage_flags(*child, damage_for_children);
            }
        } else {
            for child in children.ids() {
                damage |= self.propagate_damage_flags(*child, damage_for_children);
            }
            if let Some(before_id) = self.nodes[node_id].before() {
                damage |= self.propagate_damage_flags(before_id, damage_for_children);
            }
            if let Some(after_id) = self.nodes[node_id].after() {
                damage |= self.propagate_damage_flags(after_id, damage_for_children);
            }
        }

        let node = &mut self.nodes[node_id];

        // Put children back
        node.restore_children(children);
        *node.layout_children.get_mut() = layout_children;

        if damage.contains(CONSTRUCT_BOX) {
            damage.insert(RestyleDamage::RELAYOUT);
        }

        // Compute damage to propagate to parent
        let damage_for_parent = damage; // & RestyleDamage::RELAYOUT;

        // If the node or any of it's children have been mutated or their layout styles
        // have changed, then we should clear it's layout cache.
        if damage.intersects(ONLY_RELAYOUT | CONSTRUCT_BOX) {
            node.cache_mut().clear();
            if let Some(inline_layout) = node
                .data
                .downcast_element_mut()
                .and_then(|el| el.inline_layout_data.as_mut())
            {
                inline_layout.content_widths = None;
            }
            damage.remove(ONLY_RELAYOUT);
        }

        // Store damage for current node
        node.set_damage(damage);

        // let _is_fc_root = node
        //     .primary_styles()
        //     .map(|s| is_fc_root(&s))
        //     .unwrap_or(false);

        // if damage.contains(CONSTRUCT_BOX) {
        //     // damage_for_parent.insert(CONSTRUCT_FC | CONSTRUCT_DESCENDENT);
        //     damage_for_parent.insert(CONSTRUCT_BOX);
        // }

        // if damage.contains(CONSTRUCT_FC) {
        //     damage_for_parent.insert(CONSTRUCT_DESCENDENT);
        //     // if !is_fc_root {
        //     damage_for_parent.insert(CONSTRUCT_FC);
        //     // }
        // }

        // Propagate damage to parent
        damage_for_parent
    }

    /// Clear damage and the `damaged_descendants`/`dirty_descendants` flags
    /// on all nodes which may carry them, using the `damaged_descendants`
    /// flags to skip clean subtrees (mirroring `propagate_damage_flags`).
    pub(crate) fn clear_damage_and_dirty_flags(&mut self, node_id: NodeId) {
        {
            let node = &self.nodes[node_id];
            let has_damage = node.damage().is_some_and(|d| !d.is_empty());
            if !has_damage && !node.has_damaged_descendants() && !node.is_anonymous() {
                return;
            }
        }

        let children = self.nodes[node_id].take_children_for_iteration();
        let layout_children = std::mem::take(self.nodes[node_id].layout_children.get_mut());
        for child in children.ids() {
            self.clear_damage_and_dirty_flags(*child);
        }
        if let Some(layout_children) = layout_children.as_ref() {
            for child in layout_children.iter() {
                self.clear_damage_and_dirty_flags(*child);
            }
        }
        if let Some(before_id) = self.nodes[node_id].before() {
            self.clear_damage_and_dirty_flags(before_id);
        }
        if let Some(after_id) = self.nodes[node_id].after() {
            self.clear_damage_and_dirty_flags(after_id);
        }

        let node = &mut self.nodes[node_id];
        node.restore_children(children);
        *node.layout_children.get_mut() = layout_children;
        node.clear_damage_mut();
        node.unset_damaged_descendants();
        node.unset_dirty_descendants();
    }
}

// fn is_fc_root(style: &ComputedValues) -> bool {
//     let display = style.clone_display();
//     let display_inside = display.inside();

//     match display_inside {
//         DisplayInside::Flow => {
//             // Depends on parent context
//             false
//         }

//         DisplayInside::None => true,
//         DisplayInside::FlowRoot => true,
//         DisplayInside::Flex => true,
//         DisplayInside::Grid => true,
//         DisplayInside::Table => true,
//         DisplayInside::TableCell => true,

//         DisplayInside::Contents => false,
//         DisplayInside::TableRowGroup => false,
//         DisplayInside::TableColumn => false,
//         DisplayInside::TableColumnGroup => false,
//         DisplayInside::TableHeaderGroup => false,
//         DisplayInside::TableFooterGroup => false,
//         DisplayInside::TableRow => false,
//     }
// }

/// The `shape-outside` image as an image layer list of one layer, or of none.
pub(crate) fn shape_outside_layers(style: &ComputedValues) -> &[ComputedImage] {
    match &style.get_box().shape_outside {
        ShapeOutside::Image(image) => std::slice::from_ref(image),
        ShapeOutside::None | ShapeOutside::Shape(..) | ShapeOutside::Box(_) => &[],
    }
}

pub(crate) fn compute_layout_damage(old: &ComputedValues, new: &ComputedValues) -> RestyleDamage {
    let box_tree_needs_rebuild = || {
        let old_box = old.get_box();
        let new_box = new.get_box();

        if old_box.display != new_box.display
            || old_box.float != new_box.float
            || old_box.position != new_box.position
            || old_box.contain != new_box.contain
            || old.clone_visibility() != new.clone_visibility()
        {
            return true;
        }

        if old.get_font() != new.get_font() {
            return true;
        }

        if new_box.display.outside() == DisplayOutside::Block
            && new_box.display.inside() == DisplayInside::Flow
        {
            let alignment_establishes_new_block_formatting_context = |style: &ComputedValues| {
                style.get_position().align_content.primary() != AlignFlags::NORMAL
            };

            let old_column = old.get_column();
            let new_column = new.get_column();
            if old_box.overflow_x.is_scrollable() != new_box.overflow_x.is_scrollable()
                || old_column.is_multicol() != new_column.is_multicol()
                || old_column.column_span != new_column.column_span
                || alignment_establishes_new_block_formatting_context(old)
                    != alignment_establishes_new_block_formatting_context(new)
            {
                return true;
            }
        }

        if old_box.display.is_list_item() {
            let old_list = old.get_list();
            let new_list = new.get_list();
            if old_list.list_style_position != new_list.list_style_position
                || old_list.list_style_image != new_list.list_style_image
                || (new_list.list_style_image == StyloImage::None
                    && old_list.list_style_type != new_list.list_style_type)
            {
                return true;
            }
        }

        if new.is_pseudo_style() && old.get_counters().content != new.get_counters().content {
            return true;
        }

        false
    };

    let text_shaping_needs_recollect = || {
        if old.clone_direction() != new.clone_direction()
            || old.clone_unicode_bidi() != new.clone_unicode_bidi()
        {
            return true;
        }

        let old_text = old.get_inherited_text();
        let new_text = new.get_inherited_text();
        if !std::ptr::eq(old_text, new_text)
            && (old_text.white_space_collapse != new_text.white_space_collapse
                || old_text.text_transform != new_text.text_transform
                || old_text.word_break != new_text.word_break
                || old_text.overflow_wrap != new_text.overflow_wrap
                || old_text.letter_spacing != new_text.letter_spacing
                || old_text.word_spacing != new_text.word_spacing
                || old_text.text_rendering != new_text.text_rendering
                || old_text.hyphens != new_text.hyphens
                || old_text.text_emphasis_style != new_text.text_emphasis_style)
        {
            return true;
        }

        false
    };

    #[allow(
        clippy::if_same_then_else,
        reason = "these branches will soon be different"
    )]
    if box_tree_needs_rebuild() {
        ALL_DAMAGE
    } else if text_shaping_needs_recollect() {
        ALL_DAMAGE
    } else {
        // This element needs to be laid out again, but does not have any damage to
        // its box. In the future, we will distinguish between types of damage to the
        // fragment as well.
        RestyleDamage::RELAYOUT
    }
}

/// A child with a z_index that is hoisted up to it's containing Stacking Context for paint purposes
#[derive(Debug, Clone)]
pub struct HoistedPaintChild {
    pub node_id: NodeId,
    pub z_index: i32,
    pub position: taffy::Point<f32>,
}

#[derive(Debug)]
pub struct HoistedPaintChildren {
    pub children: Vec<HoistedPaintChild>,
    /// The number of hoisted point children with negative z_index
    pub negative_z_count: u32,

    pub content_area: taffy::Rect<f32>,
}

impl HoistedPaintChildren {
    fn new() -> Self {
        Self {
            children: Vec::new(),
            negative_z_count: 0,
            content_area: taffy::Rect::ZERO,
        }
    }

    pub fn reset(&mut self) {
        self.children.clear();
        self.negative_z_count = 0;
    }

    pub fn compute_content_size(&mut self, doc: &BaseDocument) {
        fn child_pos(child: &HoistedPaintChild, doc: &BaseDocument) -> Rect<f32> {
            let layout = doc.nodes[child.node_id].final_layout();
            let left = child.position.x + layout.location.x;
            let top = child.position.y + layout.location.y;
            let overflow = layout.scrollable_overflow_rect;
            let right = left + layout.size.width.max(overflow.right);
            let bottom = top + layout.size.height.max(overflow.bottom);

            taffy::Rect {
                top,
                left,
                bottom,
                right,
            }
        }

        if self.children.is_empty() {
            self.content_area = taffy::Rect::ZERO;
        } else {
            self.content_area = child_pos(&self.children[0], doc);
            for child in self.children[1..].iter() {
                let pos = child_pos(child, doc);
                self.content_area.left = self.content_area.left.min(pos.left);
                self.content_area.top = self.content_area.top.min(pos.top);
                self.content_area.right = self.content_area.right.max(pos.right);
                self.content_area.bottom = self.content_area.bottom.max(pos.bottom);
            }
        }
    }

    pub fn sort(&mut self) {
        self.children.sort_by_key(|c| c.z_index);
        self.negative_z_count = self.children.iter().take_while(|c| c.z_index < 0).count() as u32;
    }

    pub fn neg_z_range(&self) -> Range<usize> {
        0..(self.negative_z_count as usize)
    }

    pub fn pos_z_range(&self) -> Range<usize> {
        (self.negative_z_count as usize)..self.children.len()
    }

    pub fn neg_z_hoisted_children(
        &self,
    ) -> impl ExactSizeIterator<Item = &HoistedPaintChild> + DoubleEndedIterator {
        self.children[self.neg_z_range()].iter()
    }

    pub fn pos_z_hoisted_children(
        &self,
    ) -> impl ExactSizeIterator<Item = &HoistedPaintChild> + DoubleEndedIterator {
        self.children[self.pos_z_range()].iter()
    }
}

impl BaseDocument {
    pub(crate) fn invalidate_inline_contexts(&mut self) {
        let scale = self.viewport.scale();

        let font_ctx = &self.font_ctx;
        let layout_ctx = &mut self.layout_ctx;

        let mut anon_nodes = Vec::new();

        for (_, node) in self.nodes.iter_mut() {
            if !(node.flags.contains(NodeFlags::IS_IN_DOCUMENT)) {
                continue;
            }

            let Some(element) = node.data.downcast_element_mut() else {
                continue;
            };

            if element.inline_layout_data.is_some() {
                if node.is_anonymous() {
                    anon_nodes.push(node.id);
                } else {
                    node.insert_damage(ALL_DAMAGE);
                }
            } else if let Some(input) = element.text_input_data_mut() {
                input.editor.set_scale(scale);
                let mut font_ctx = font_ctx.lock().unwrap();
                input.editor.refresh_layout(&mut font_ctx, layout_ctx);
                node.insert_damage(ONLY_RELAYOUT);
            }
        }

        for node_id in anon_nodes {
            if let Some(parent_id) = *(self.nodes[node_id].layout_parent.get_mut()) {
                self.nodes[parent_id].insert_damage(ALL_DAMAGE);
            }
        }
    }

    pub fn flush_styles_to_layout(&mut self, node_id: NodeId) {
        self.flush_styles_to_layout_impl(node_id, None, LayoutFlow::HorizontalTb, &mut false, true);
    }

    /// Set the position and content area of every hoisted child from the finished layout.
    /// The hoisting itself runs in the style flush, before that layout is known.
    pub(crate) fn place_hoisted_children(&mut self) {
        let mut stack = vec![self.root_element().id];
        while let Some(id) = stack.pop() {
            if let Some(children) = self.nodes[id].layout_children.borrow().as_ref() {
                stack.extend(children.iter().copied());
            }
            let Some(mut context) = self.nodes[id].stacking_context.take() else {
                continue;
            };
            for child in &mut context.children {
                child.position = self.offset_from_stacking_context(child.node_id, id);
            }
            context.compute_content_size(self);
            self.nodes[id].stacking_context = Some(context);
        }
    }

    /// The origin of `id`'s layout parent in the frame the stacking context `root` paints its
    /// children in.
    fn offset_from_stacking_context(&self, id: NodeId, root: NodeId) -> taffy::Point<f32> {
        let mut offset = taffy::Point::ZERO;
        let mut ancestor = self.nodes[id].layout_parent.get();
        while let Some(id) = ancestor.filter(|&id| id != root) {
            let node = &self.nodes[id];
            let location = node.final_layout().location;
            let scroll_offset = node.scroll_offset();
            offset.x += location.x - scroll_offset.x as f32;
            offset.y += location.y - scroll_offset.y as f32;
            ancestor = node.layout_parent.get();
        }
        offset
    }

    /// The `z-index` to hoist `child_id` to its stacking context with, or `None` to paint it
    /// with its parent. A non-zero `z-index` is hoisted. A fixed box of `z-index` auto or 0
    /// is too, as it paints above in-flow content even where its parent would paint below a
    /// later sibling; then so is every positioned box after it, which paints above it (CSS
    /// 2.1 Appendix E step 8).
    fn hoisted_z_index(
        &self,
        child_id: NodeId,
        is_flex_or_grid: bool,
        can_hoist_auto_z: bool,
        follows_fixed_box: bool,
    ) -> Option<i32> {
        let child = &self.nodes[child_id];
        let style = child.primary_styles()?;
        let position = style.clone_position();
        let z_index = style.clone_z_index().integer_or(0);
        if z_index != 0 {
            // z-index applies to static flex/grid items too
            // (css-flexbox-1 §painting, css-grid-1 §z-order).
            return (position != Position::Static || is_flex_or_grid).then_some(z_index);
        }
        let is_at_fixed_box_level = match position {
            Position::Fixed => true,
            Position::Relative | Position::Absolute | Position::Sticky => follows_fixed_box,
            Position::Static => false,
        };
        (is_at_fixed_box_level && can_hoist_auto_z && child.style().display != taffy::Display::None)
            .then_some(0)
    }

    /// Flush the image layers of nodes whose style changed during the last
    /// style traversal (or whose pseudo-element boxes were (re)constructed).
    pub(crate) fn flush_pending_style_images(&mut self) {
        let mut pending = std::mem::take(&mut self.pending_style_image_nodes);
        pending.sort_unstable();
        pending.dedup();
        for node_id in pending {
            // Anonymous boxes (including pseudo-elements) can be removed from
            // the slab between queueing and flushing; skip stale IDs.
            if !self.nodes.contains_key(node_id) {
                continue;
            }
            self.flush_image_layers_from_style(node_id, ImageLayerKind::Background);
            self.flush_image_layers_from_style(node_id, ImageLayerKind::Mask);
            self.flush_image_layers_from_style(node_id, ImageLayerKind::ShapeOutside);
        }
    }

    /// Flush a CSS image layer list (`background-image`, `mask-image` or the
    /// `shape-outside` image) from style to dedicated storage on the node, fetching
    /// any images which are not yet loaded.
    fn flush_image_layers_from_style(&mut self, node_id: NodeId, kind: ImageLayerKind) {
        let doc_id = self.id();
        let node = self.nodes.get_mut(node_id).unwrap();
        // Clone the primary style `Arc` into an owned value so the immutable
        // borrow of `node` (held by the stylo element data guard) is released
        // before we take a mutable borrow of `node.data` below.
        let style = {
            let stylo_element_data = node.stylo_element_data_opt().and_then(|s| s.get());
            let primary_styles = stylo_element_data
                .as_ref()
                .and_then(|data| data.styles.get_primary());
            let Some(style) = primary_styles else {
                return;
            };
            style.clone()
        };
        let Some(elem) = node.data.downcast_element_mut() else {
            return;
        };

        let (style_images, elem_images): (&[ComputedImage], _) = match kind {
            ImageLayerKind::Background => (
                &style.get_background().background_image.0,
                &mut elem.background_images,
            ),
            ImageLayerKind::Mask => (&style.get_svg().mask_image.0, &mut elem.mask_images),
            ImageLayerKind::ShapeOutside => {
                (shape_outside_layers(&style), &mut elem.shape_outside_images)
            }
        };

        let len = style_images.len();
        elem_images.resize_with(len, || None);

        for idx in 0..len {
            let style_image = &style_images[idx];
            let new_image = match style_image {
                StyloImage::Url(ComputedUrl::Valid(new_url)) => {
                    let old_image = elem_images[idx].as_ref();
                    let old_image_url = old_image.map(|data| &data.url);
                    if old_image_url.is_some_and(|old_url| **new_url == **old_url) {
                        continue;
                    }

                    // Check cache first
                    let url_str = new_url.as_str();
                    if let Some(cached_image) = self.image_cache.get(url_str) {
                        #[cfg(feature = "tracing")]
                        tracing::info!("Loading image {url_str} from cache");
                        Some(ImageResourceData {
                            url: new_url.clone(),
                            status: Status::Ok,
                            image: cached_image,
                        })
                    } else if let Some(waiting_list) = self.pending_images.get_mut(url_str) {
                        // Image is already being fetched, queue this node
                        #[cfg(feature = "tracing")]
                        tracing::info!("Image {url_str} already pending, queueing node {node_id}");
                        waiting_list.push((node_id, kind.image_type(idx)));
                        Some(ImageResourceData::new(new_url.clone()))
                    } else {
                        // Start fetch and track as pending
                        #[cfg(feature = "tracing")]
                        tracing::info!("Fetching image {url_str}");
                        self.pending_images
                            .insert(url_str.to_string(), vec![(node_id, kind.image_type(idx))]);

                        self.net_provider.fetch(
                            doc_id,
                            crate::net::stamped_request(
                                (**new_url).clone(),
                                self.abort_signal.as_ref(),
                            ),
                            ResourceHandler::boxed(
                                self.tx.clone(),
                                doc_id,
                                None, // Don't pass node_id, we'll handle via pending_images
                                self.shell_provider.clone(),
                                ImageHandler::new(kind.image_type(idx)),
                            ),
                        );

                        Some(ImageResourceData::new(new_url.clone()))
                    }
                }
                _ => None,
            };

            // Element will always exist due to resize_with above
            elem_images[idx] = new_image;
        }
    }

    /// Walk the whole tree, converting styles to layout
    /// `parent_flow` is the writing mode the node's layout parent lays it out in.
    fn flush_styles_to_layout_impl(
        &mut self,
        node_id: NodeId,
        parent_stacking_context: Option<&mut HoistedPaintChildren>,
        parent_flow: LayoutFlow,
        fixed_box_seen: &mut bool,
        can_hoist_auto_z: bool,
    ) {
        let mut new_stacking_context: HoistedPaintChildren = HoistedPaintChildren::new();
        let stacking_context = &mut new_stacking_context;

        let flow = {
            let node = &self.nodes[node_id];
            let parent_style = node
                .parent
                .and_then(|parent| self.nodes[parent].primary_styles());
            node.primary_styles()
                .map_or(LayoutFlow::HorizontalTb, |style| {
                    writing_mode::flow_of(&style, parent_style.as_deref().map(|style| &**style))
                })
        };

        let incremental = self.incremental_layout;
        let display = {
            let node = self.nodes.get_mut(node_id).unwrap();
            let _damage = node.damage().unwrap_or(ALL_DAMAGE);

            // Compute the owned taffy style and display in an inner scope so the
            // immutable borrow of `node` (held by the stylo element data guard)
            // is released before we mutably access `node` below.
            let (mut taffy_style, display_constructed_as) = {
                let stylo_element_data = node.stylo_element_data_opt().and_then(|s| s.get());
                let primary_styles = stylo_element_data
                    .as_ref()
                    .and_then(|data| data.styles.get_primary());

                let Some(style) = primary_styles else {
                    return;
                };

                (stylo_taffy::to_taffy_style(style), style.clone_display())
            };
            taffy_style.item_is_replaced = node
                .data
                .downcast_element()
                .is_some_and(|el| crate::layout::replaced::is_replaced_element(&el.name.local));
            let (taffy_style, parent_frame_style) =
                writing_mode::styles_for_flow(taffy_style, flow, parent_flow);

            // if damage.intersects(RestyleDamage::RELAYOUT | CONSTRUCT_BOX) {
            *node.style_mut() = taffy_style;
            *node.display_constructed_as_mut() = display_constructed_as;
            if let Some(element) = node.element_data_mut() {
                element.layout_flow = flow;
                element.parent_layout_flow = parent_flow;
                element.parent_frame_style = parent_frame_style;
            }
            // }

            // In non-incremental mode we unconditionally clear the Taffy cache.
            // In incremental mode this is handled as part of damage propagation.
            if !incremental {
                node.cache_mut().clear();
                if let Some(inline_layout) = node
                    .data
                    .downcast_element_mut()
                    .and_then(|el| el.inline_layout_data.as_mut())
                {
                    inline_layout.content_widths = None;
                }
            }

            node.style().display
        };

        // If the node has children, then take those children and...
        let children = self.nodes[node_id].layout_children.borrow_mut().take();
        if let Some(mut children) = children {
            let is_flex_or_grid = matches!(display, taffy::Display::Flex | taffy::Display::Grid);

            // Sort layout_children
            if is_flex_or_grid {
                children.sort_by(|left, right| {
                    let left_node = self.nodes.get(*left).unwrap();
                    let right_node = self.nodes.get(*right).unwrap();
                    left_node.order().cmp(&right_node.order())
                });
            }

            let children_can_hoist_auto_z = parent_stacking_context.is_none()
                || (can_hoist_auto_z
                    && !self.nodes[node_id]
                        .primary_styles()
                        .is_some_and(|style| confines_descendants(&style)));

            // Reserve space for paint_children
            let mut paint_children = self.nodes[node_id]
                .paint_children
                .borrow_mut()
                .take()
                .unwrap_or_default();
            paint_children.clear();
            paint_children.reserve(children.len());

            // Recursively call flush_styles_to_layout on each child, and push it to either
            // paint_children or the stacking context. A hoisted child goes before what its
            // descendants hoisted, so that a stacking context's children stay in tree order.
            for &child_id in children.iter() {
                let first_hoisted_below = stacking_context.children.len();
                let follows_fixed_box = *fixed_box_seen;

                if self.nodes[child_id].is_stacking_context_root(is_flex_or_grid) {
                    self.flush_styles_to_layout_impl(child_id, None, flow, &mut false, true);
                } else {
                    self.flush_styles_to_layout_impl(
                        child_id,
                        Some(stacking_context),
                        flow,
                        fixed_box_seen,
                        children_can_hoist_auto_z,
                    );
                }

                // TODO: more complete hoisting detection
                match self.hoisted_z_index(
                    child_id,
                    is_flex_or_grid,
                    children_can_hoist_auto_z,
                    follows_fixed_box,
                ) {
                    Some(z_index) => {
                        // A box at z-index 0 is here as the fixed box, or after one.
                        *fixed_box_seen |= z_index == 0;
                        stacking_context.children.insert(
                            first_hoisted_below,
                            HoistedPaintChild {
                                node_id: child_id,
                                z_index,
                                position: taffy::Point::ZERO,
                            },
                        );
                    }
                    None => paint_children.push(child_id),
                }
            }

            // Sort paint_children
            paint_children.sort_by(|left, right| {
                let left_node = self.nodes.get(*left).unwrap();
                let right_node = self.nodes.get(*right).unwrap();
                node_to_paint_order(left_node, is_flex_or_grid)
                    .cmp(&node_to_paint_order(right_node, is_flex_or_grid))
            });

            // Put children back
            *self.nodes[node_id].paint_children.borrow_mut() = Some(paint_children);
            *self.nodes[node_id].layout_children.borrow_mut() = Some(children);
        }

        if let Some(parent_stacking_context) = parent_stacking_context {
            parent_stacking_context
                .children
                .extend(stacking_context.children.iter().cloned());
            // A box that stopped being a stacking context would paint its old list twice.
            self.nodes[node_id].stacking_context = None;
        } else {
            stacking_context.sort();
            self.nodes[node_id].stacking_context = Some(Box::new(new_stacking_context));
        }
    }
}

/// Whether a box holds its descendants in a clip, a group effect or a containing block for
/// fixed boxes, which a box of `z-index` auto hoisted past it would escape. Some of these are
/// stacking contexts by spec, but `is_stacking_context_root` knows only some of them, and a
/// `transform` only from the second flush.
fn confines_descendants(style: &ComputedValues) -> bool {
    let box_style = style.get_box();
    establishes_fixed_containing_block(style)
        || !matches!(box_style.overflow_x, Overflow::Visible)
        || !matches!(box_style.overflow_y, Overflow::Visible)
        || !style.get_effects().backdrop_filter.0.is_empty()
        || !matches!(style.clone_clip_path(), ClipPath::None)
        || style
            .get_svg()
            .mask_image
            .0
            .iter()
            .any(|image| !matches!(image, StyloImage::None))
}

#[inline(always)]
fn position_to_order(pos: Position) -> i32 {
    match pos {
        Position::Static => 0,
        // All positioned descendants with z-index: auto share one paint
        // level (CSS 2.1 Appendix E step 8); the stable sort keeps them in
        // tree order among themselves, above in-flow content and floats.
        Position::Relative | Position::Sticky | Position::Absolute | Position::Fixed => 2,
    }
}
#[inline(always)]
fn float_to_order(pos: Float) -> i32 {
    match pos {
        Float::None => 0,
        _ => 1,
    }
}

/// Paint sort key: (paint level, order-modified position). Positioned
/// (z-index: auto) descendants paint above in-flow content (CSS 2.1
/// Appendix E step 8); within a level the stable sort preserves
/// (order-modified) document order.
#[inline(always)]
fn node_to_paint_order(node: &Node, is_flex_or_grid: bool) -> (i32, i32) {
    let Some(style) = node.primary_styles() else {
        return (0, 0);
    };
    let position = style.clone_position();
    if is_flex_or_grid {
        match position {
            Position::Static => (0, style.clone_order()),
            Position::Relative | Position::Sticky => (2, style.clone_order()),
            // Out-of-flow children are not flex/grid items: `order` does
            // not apply; tree order does.
            Position::Absolute | Position::Fixed => (2, 0),
        }
    } else {
        (
            position_to_order(position) + float_to_order(style.clone_float()),
            0,
        )
    }
}
