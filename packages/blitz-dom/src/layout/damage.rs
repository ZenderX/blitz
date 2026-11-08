use blitz_traits::node_id::NodeId;
use std::ops::Range;

use super::fixed::{establishes_fixed_containing_block, is_fixed_positioned};
use super::writing_mode;
use crate::Node;
use crate::net::ResourceHandler;
use crate::node::{LayoutFlow, NodeFlags};
use crate::{
    BaseDocument, net::ImageHandler, node::ImageResourceData, node::Status, util::ImageLayerKind,
};
use style::Zero;
use style::properties::ComputedValues;
use style::properties::generated::longhands::position::computed_value::T as Position;
use style::selector_parser::RestyleDamage;
use style::url::ComputedUrl;
use style::values::computed::Float;
use style::values::computed::Overflow;
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
    /// For a box hoisted at `z-index` 0, the padding boxes of the overflow-clipping boxes in
    /// its containing block chain that it was hoisted past, intersected, in the frame of
    /// `position`; `None` when there are none.
    pub clip: Option<taffy::Rect<f32>>,
    /// Whether the stacking context it paints in lies strictly between it and its containing
    /// block, so that the context's own overflow clip does not reach it (CSS 2.1 §11.1.1).
    pub escapes_context_clip: bool,
}

impl HoistedPaintChild {
    /// Whether its clip, if any, leaves the point (in the frame of `position`) visible.
    pub fn is_visible_at(&self, x: f32, y: f32) -> bool {
        self.clip.is_none_or(|clip| {
            x >= clip.left && x <= clip.right && y >= clip.top && y <= clip.bottom
        })
    }
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
        self.flush_styles_to_layout_impl(
            node_id,
            None,
            LayoutFlow::HorizontalTb,
            &mut false,
            AutoZHoisting::All,
        );
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
                // Only boxes at z-index 0 are hoisted past an overflow clip.
                (child.position, child.clip, child.escapes_context_clip) =
                    self.place_in_stacking_context(child.node_id, id, child.z_index == 0);
            }
            context.compute_content_size(self);
            self.nodes[id].stacking_context = Some(context);
        }
    }

    /// The origin of `id`'s layout parent in the frame the stacking context `root` paints its
    /// children in; with `is_clipped`, the intersection of the padding boxes of the
    /// overflow-clipping boxes between them in that frame; and whether `root`'s own overflow
    /// clip passes over the box. An overflow clip reaches a box only through its containing
    /// block chain (CSS 2.1 §11.1.1), so a fixed box skips the ancestors strictly between it
    /// and its containing block.
    fn place_in_stacking_context(
        &self,
        id: NodeId,
        root: NodeId,
        is_clipped: bool,
    ) -> (taffy::Point<f32>, Option<taffy::Rect<f32>>, bool) {
        let mut offset = taffy::Point::ZERO;
        // Kept less the offset carried so far, which moves every clip alike once complete.
        let mut clip: Option<taffy::Rect<f32>> = None;
        let mut is_released = is_fixed_positioned(&self.nodes[id]);
        let mut ancestor = self.nodes[id].layout_parent.get();
        while let Some(id) = ancestor.filter(|&id| id != root) {
            let node = &self.nodes[id];
            let layout = node.final_layout();
            let location = layout.location;
            let scroll_offset = node.scroll_offset();
            offset.x += location.x - scroll_offset.x as f32;
            offset.y += location.y - scroll_offset.y as f32;
            is_released &= !node.establishes_fixed_containing_block();
            if is_clipped && !is_released && node.clips_own_overflow() {
                let padding_box = taffy::Rect {
                    left: location.x + layout.border.left - offset.x,
                    right: location.x + layout.size.width - layout.border.right - offset.x,
                    top: location.y + layout.border.top - offset.y,
                    bottom: location.y + layout.size.height - layout.border.bottom - offset.y,
                };
                clip = Some(clip.map_or(padding_box, |clip| taffy::Rect {
                    left: clip.left.max(padding_box.left),
                    right: clip.right.min(padding_box.right),
                    top: clip.top.max(padding_box.top),
                    bottom: clip.bottom.min(padding_box.bottom),
                }));
            }
            ancestor = node.layout_parent.get();
        }
        let clip = clip.map(|clip| taffy::Rect {
            left: clip.left + offset.x,
            right: clip.right + offset.x,
            top: clip.top + offset.y,
            bottom: clip.bottom + offset.y,
        });
        let escapes_root_clip =
            is_released && !self.nodes[root].establishes_fixed_containing_block();
        (offset, clip, escapes_root_clip)
    }

    /// The `z-index` to hoist `child_id` to its stacking context with, or `None` to paint it
    /// with its parent. A non-zero `z-index` is hoisted. A fixed box of `z-index` auto or 0
    /// is too, as it paints above in-flow content even where its parent would paint below a
    /// later sibling; then so is every positioned box or stacking context after it, which
    /// paints above it (CSS 2.1 Appendix E step 8).
    fn hoisted_z_index(
        &self,
        child_id: NodeId,
        is_flex_or_grid: bool,
        hoisting: AutoZHoisting,
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
        let can_hoist = match position {
            Position::Fixed => hoisting != AutoZHoisting::None,
            Position::Relative | Position::Absolute | Position::Sticky => {
                follows_fixed_box && hoisting == AutoZHoisting::All
            }
            Position::Static => {
                follows_fixed_box
                    && hoisting == AutoZHoisting::All
                    && child.is_stacking_context_root(is_flex_or_grid)
            }
        };
        (can_hoist && child.style().display != taffy::Display::None).then_some(0)
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
        hoisting: AutoZHoisting,
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

            let children_hoisting = match self.nodes[node_id].primary_styles() {
                _ if parent_stacking_context.is_none() => AutoZHoisting::All,
                // May not be a stacking context yet, and a box hoisted past it would escape it.
                Some(style) if establishes_fixed_containing_block(&style) => AutoZHoisting::None,
                Some(style) if clips_to_rounded_corners(&style) => {
                    hoisting.min(AutoZHoisting::FixedOnly)
                }
                _ => hoisting,
            };

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
                    self.flush_styles_to_layout_impl(
                        child_id,
                        None,
                        flow,
                        &mut false,
                        AutoZHoisting::All,
                    );
                } else {
                    self.flush_styles_to_layout_impl(
                        child_id,
                        Some(stacking_context),
                        flow,
                        fixed_box_seen,
                        children_hoisting,
                    );
                }

                // TODO: more complete hoisting detection
                match self.hoisted_z_index(
                    child_id,
                    is_flex_or_grid,
                    children_hoisting,
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
                                clip: None,
                                escapes_context_clip: false,
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

/// Which boxes of `z-index` auto or 0 can be hoisted past a box.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum AutoZHoisting {
    None,
    /// Only fixed boxes, which a rounded overflow clip below their containing block lets out;
    /// a positioned box needs that clip, whose shape [`HoistedPaintChild::clip`] cannot carry.
    FixedOnly,
    All,
}

/// Whether a box clips its overflow to rounded corners, a shape the rectangular
/// [`HoistedPaintChild::clip`] does not carry.
fn clips_to_rounded_corners(style: &ComputedValues) -> bool {
    let box_style = style.get_box();
    let border = style.get_border();
    (box_style.overflow_x != Overflow::Visible || box_style.overflow_y != Overflow::Visible)
        && [
            &border.border_top_left_radius,
            &border.border_top_right_radius,
            &border.border_bottom_right_radius,
            &border.border_bottom_left_radius,
        ]
        .into_iter()
        .any(|radius| !radius.is_zero())
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
/// (z-index: auto) descendants and stacking contexts paint above in-flow
/// content (CSS 2.1 Appendix E step 8); within a level the stable sort
/// preserves (order-modified) document order.
#[inline(always)]
fn node_to_paint_order(node: &Node, is_flex_or_grid: bool) -> (i32, i32) {
    let Some(style) = node.primary_styles() else {
        return (0, 0);
    };
    let position = style.clone_position();
    let is_static_stacking_context =
        position == Position::Static && node.is_stacking_context_root(is_flex_or_grid);
    if is_flex_or_grid {
        match position {
            Position::Static => (
                if is_static_stacking_context { 2 } else { 0 },
                style.clone_order(),
            ),
            Position::Relative | Position::Sticky => (2, style.clone_order()),
            // Out-of-flow children are not flex/grid items: `order` does
            // not apply; tree order does.
            Position::Absolute | Position::Fixed => (2, 0),
        }
    } else if is_static_stacking_context {
        (2, 0)
    } else {
        (
            position_to_order(position) + float_to_order(style.clone_float()),
            0,
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::{Attribute, BaseDocument, DocumentConfig, NodeId, Point, QualName, qual_name};
    use blitz_traits::shell::{ColorScheme, Viewport};

    /// `<html><body style="margin:0">` in a 400x300 viewport; returns the document and body.
    fn page() -> (BaseDocument, NodeId) {
        let mut doc = BaseDocument::new(DocumentConfig {
            viewport: Some(Viewport::new(400, 300, 1.0, ColorScheme::Light)),
            ..Default::default()
        });
        let root_id = doc.root_node().id;
        let html = append(&mut doc, root_id, "");
        let body = append(&mut doc, html, "margin:0");
        (doc, body)
    }

    /// Append a `display:block` div styled `style` (no UA stylesheet is loaded) to `parent`.
    fn append(doc: &mut BaseDocument, parent: NodeId, style: &str) -> NodeId {
        let mut mutator = doc.mutate();
        let id = mutator.create_element(
            qual_name!("div"),
            vec![Attribute {
                name: qual_name!("style"),
                value: format!("display:block;{style}"),
            }],
        );
        mutator.append_children(parent, &[id]);
        id
    }

    fn hit(doc: &BaseDocument, x: f32, y: f32) -> Option<NodeId> {
        doc.hit(x, y).map(|hit| hit.node_id)
    }

    fn hoisted_clip(doc: &BaseDocument, id: NodeId) -> Option<taffy::Rect<f32>> {
        let context = doc.root_element().stacking_context.as_ref().unwrap();
        let child = context.children.iter().find(|child| child.node_id == id);
        child.expect("hoisted to the root stacking context").clip
    }

    const CLIPPER: &str = "width:200px;height:100px;overflow:hidden";
    const FIXED: &str = "position:fixed;left:10px;top:10px;width:50px;height:50px";

    /// A real `<html><body>` in a 400x300 viewport, each with its `style`.
    fn page_with(html_style: &str, body_style: &str) -> (BaseDocument, NodeId, NodeId) {
        let mut doc = BaseDocument::new(DocumentConfig {
            viewport: Some(Viewport::new(400, 300, 1.0, ColorScheme::Light)),
            ..Default::default()
        });
        let root_id = doc.root_node().id;
        let html = append_tag(&mut doc, root_id, qual_name!("html"), html_style);
        let body = append_tag(&mut doc, html, qual_name!("body"), body_style);
        (doc, html, body)
    }

    fn append_tag(doc: &mut BaseDocument, parent: NodeId, name: QualName, style: &str) -> NodeId {
        let mut mutator = doc.mutate();
        let id = mutator.create_element(
            name,
            vec![Attribute {
                name: qual_name!("style"),
                value: format!("display:block;{style}"),
            }],
        );
        mutator.append_children(parent, &[id]);
        id
    }

    fn escapes_clip(doc: &BaseDocument, root: NodeId, id: NodeId) -> bool {
        let context = doc.nodes[root].stacking_context.as_ref().unwrap();
        let child = context.children.iter().find(|child| child.node_id == id);
        child
            .expect("hoisted to the given stacking context")
            .escapes_context_clip
    }

    /// CSS 2.1 Appendix E step 8: the fixed box paints above in-flow content, even content
    /// after its static parent inside an ancestor that clips it.
    #[test]
    fn a_fixed_box_under_a_clip_paints_above_the_in_flow_content_after_it() {
        let (mut doc, body) = page();
        let clipper = append(&mut doc, body, CLIPPER);
        let header = append(&mut doc, clipper, "height:20px");
        let fixed = append(&mut doc, header, FIXED);
        let later = append(&mut doc, clipper, "height:80px");
        doc.resolve(0.0);

        let header_paints = doc.nodes[header].paint_children.borrow().clone();
        assert!(
            !header_paints.unwrap_or_default().contains(&fixed),
            "not painted with its static parent, under the later content"
        );
        assert_eq!(
            hoisted_clip(&doc, fixed),
            None,
            "the viewport is its containing block, so the clipper does not clip it"
        );
        assert_eq!(
            hit(&doc, 20.0, 30.0),
            Some(fixed),
            "the overlap is the fixed box"
        );
        assert_eq!(
            hit(&doc, 100.0, 30.0),
            Some(later),
            "the rest is the later box"
        );
    }

    /// CSS 2.1 §11.1.1: overflow does not clip a descendant whose containing block is the
    /// viewport, so the box paints and is hit past the clipper.
    #[test]
    fn a_fixed_box_is_not_clipped_by_a_static_overflow_ancestor() {
        let (mut doc, body) = page();
        let clipper = append(&mut doc, body, CLIPPER);
        let fixed = append(
            &mut doc,
            clipper,
            "position:fixed;left:10px;top:80px;width:50px;height:50px",
        );
        doc.resolve(0.0);

        assert_eq!(hoisted_clip(&doc, fixed), None, "released from the clipper");
        assert_eq!(hit(&doc, 20.0, 90.0), Some(fixed), "inside the clipper");
        assert_eq!(hit(&doc, 20.0, 110.0), Some(fixed), "past the clipper");
    }

    /// The propagated `overflow` belongs to the viewport: `<body>` clips nothing, for a fixed
    /// box nor for a positioned box after it (CSS Overflow 3 §3.1.4).
    #[test]
    fn a_body_whose_overflow_propagates_to_the_viewport_clips_nothing() {
        let (mut doc, _, body) = page_with("", "margin:0;overflow:hidden;height:20px");
        let fixed = append(
            &mut doc,
            body,
            "position:fixed;left:10px;top:50px;width:50px;height:50px",
        );
        let later = append(
            &mut doc,
            body,
            "position:absolute;left:100px;top:50px;width:50px;height:50px",
        );
        doc.resolve(0.0);

        assert!(doc.nodes[body].is_overflow_propagated_body());
        assert_eq!(hoisted_clip(&doc, fixed), None);
        assert_eq!(hoisted_clip(&doc, later), None, "the body has no clip left");
        assert_eq!(hit(&doc, 20.0, 60.0), Some(fixed), "past the body's box");
        assert_eq!(hit(&doc, 110.0, 60.0), Some(later), "past the body's box");
    }

    /// With the root's own `overflow` set, `<body>` keeps its own and it does clip, so a
    /// positioned box inside it keeps the clip. The fixed box still ignores it: the viewport
    /// is its containing block.
    #[test]
    fn a_body_keeps_its_overflow_clip_when_the_root_sets_its_own_overflow() {
        let (mut doc, html, body) =
            page_with("overflow:hidden", "margin:0;overflow:hidden;height:20px");
        let fixed = append(
            &mut doc,
            body,
            "position:fixed;left:10px;top:50px;width:50px;height:50px",
        );
        let later = append(
            &mut doc,
            body,
            "position:relative;left:100px;top:50px;width:50px;height:50px",
        );
        doc.resolve(0.0);

        assert!(!doc.nodes[body].is_overflow_propagated_body());
        assert_eq!(hoisted_clip(&doc, fixed), None);
        assert!(hoisted_clip(&doc, later).is_some(), "the body clip stays");
        assert!(
            !escapes_clip(&doc, html, later),
            "a positioned box's containing block chain goes through the root"
        );
        assert_ne!(hit(&doc, 110.0, 60.0), Some(later), "past the body's box");
    }

    /// The transformed box is the containing block, so its own overflow does clip the box.
    #[test]
    fn a_fixed_box_stays_clipped_by_a_transformed_containing_block() {
        let (mut doc, body) = page();
        let clipper = append(
            &mut doc,
            body,
            "margin-left:30px;width:100px;height:100px;overflow:hidden;transform:translate(1px,0)",
        );
        let fixed = append(
            &mut doc,
            clipper,
            "position:fixed;left:10px;top:50px;width:50px;height:80px",
        );
        doc.resolve(0.0);
        doc.resolve(0.0);

        assert!(
            !escapes_clip(&doc, clipper, fixed),
            "stays in the clip layer"
        );
        assert_eq!(hit(&doc, 50.0, 90.0), Some(fixed), "inside the clipper");
        assert_ne!(hit(&doc, 50.0, 110.0), Some(fixed), "past the clipper");
    }

    /// An `overflow` box strictly between the fixed box and its transformed containing block
    /// is not in its containing block chain (CSS 2.1 §11.1.1).
    #[test]
    fn a_fixed_box_is_not_clipped_between_it_and_its_containing_block() {
        let (mut doc, body) = page();
        let containing_block = append(
            &mut doc,
            body,
            "width:200px;height:200px;transform:translate(1px,0)",
        );
        let clipper = append(&mut doc, containing_block, CLIPPER);
        let fixed = append(
            &mut doc,
            clipper,
            "position:fixed;left:10px;top:80px;width:50px;height:50px",
        );
        doc.resolve(0.0);
        doc.resolve(0.0);

        assert_eq!(hit(&doc, 20.0, 90.0), Some(fixed), "inside the clipper");
        assert_eq!(hit(&doc, 20.0, 110.0), Some(fixed), "past the clipper");
    }

    /// The fixed box leaves the clip of a stacking context that is not its containing block,
    /// while a positioned box of that same context, in its containing block chain, keeps it.
    #[test]
    fn a_fixed_box_leaves_the_clip_of_a_stacking_context_that_is_not_its_containing_block() {
        let (mut doc, body) = page();
        let clipper = append(&mut doc, body, &format!("{CLIPPER};opacity:0.5"));
        let fixed = append(
            &mut doc,
            clipper,
            "position:fixed;left:10px;top:80px;width:50px;height:50px",
        );
        let later = append(
            &mut doc,
            clipper,
            "position:relative;left:100px;top:80px;width:50px;height:50px",
        );
        doc.resolve(0.0);

        assert!(escapes_clip(&doc, clipper, fixed), "out of the clip layer");
        assert!(
            !escapes_clip(&doc, clipper, later),
            "stays in the clip layer"
        );
        assert_eq!(hit(&doc, 20.0, 110.0), Some(fixed), "past the clip");
        assert_ne!(hit(&doc, 110.0, 110.0), Some(later), "past the clip");
    }

    /// WPT clipping-006: the fixed child of a clipping fixed box shows outside it, the
    /// viewport being its containing block too.
    #[test]
    fn a_fixed_box_leaves_the_clip_of_a_fixed_ancestor() {
        let (mut doc, body) = page();
        let outer = append(
            &mut doc,
            body,
            "position:fixed;left:0;top:0;width:100px;height:50px;overflow:hidden",
        );
        let inner = append(
            &mut doc,
            outer,
            "position:fixed;left:10px;top:60px;width:30px;height:30px",
        );
        doc.resolve(0.0);

        assert!(
            escapes_clip(&doc, outer, inner),
            "out of the outer clip layer"
        );
        assert_eq!(hit(&doc, 20.0, 70.0), Some(inner), "past the outer box");
    }

    /// An `absolute` box after the fixed box keeps the clip of its positioned clipper: unlike
    /// the fixed box, its containing block is that clipper.
    #[test]
    fn an_absolute_sibling_after_a_released_fixed_box_stays_clipped() {
        let (mut doc, body) = page();
        let clipper = append(&mut doc, body, &format!("{CLIPPER};position:relative"));
        let fixed = append(
            &mut doc,
            clipper,
            "position:fixed;left:10px;top:80px;width:50px;height:50px",
        );
        let later = append(
            &mut doc,
            clipper,
            "position:absolute;left:100px;top:80px;width:50px;height:50px",
        );
        doc.resolve(0.0);

        assert_eq!(hoisted_clip(&doc, fixed), None, "released");
        assert!(hoisted_clip(&doc, later).is_some(), "keeps the clip");
        assert_eq!(hit(&doc, 110.0, 90.0), Some(later), "inside the clip");
        assert_ne!(hit(&doc, 110.0, 110.0), Some(later), "past the clip");
    }

    /// The clip is the scroll container's box, which stays put while its content scrolls.
    #[test]
    fn the_clip_of_a_scrolled_container_stays_with_its_box() {
        let (mut doc, body) = page();
        append(
            &mut doc,
            body,
            "position:fixed;left:300px;top:0;width:10px;height:10px",
        );
        let scroller = append(
            &mut doc,
            body,
            "margin:50px;width:100px;height:100px;overflow:auto",
        );
        let later = append(&mut doc, scroller, "position:relative;height:300px");
        doc.resolve(0.0);
        *doc.nodes[scroller].scroll_offset_mut() = Point { x: 0.0, y: 40.0 };
        doc.resolve(0.0);

        assert_eq!(
            hit(&doc, 65.0, 140.0),
            Some(later),
            "inside the container's box"
        );
        assert_ne!(
            hit(&doc, 65.0, 160.0),
            Some(later),
            "below the container's box"
        );
    }

    /// Tree order among positioned boxes holds across the clip: a later positioned box inside
    /// it covers an earlier fixed box outside it, and stays clipped.
    #[test]
    fn a_later_positioned_box_under_a_clip_stays_above_an_earlier_fixed_box() {
        let (mut doc, body) = page();
        append(
            &mut doc,
            body,
            "position:fixed;left:0;top:0;width:100px;height:100px",
        );
        let clipper = append(&mut doc, body, CLIPPER);
        let later = append(&mut doc, clipper, "position:relative;height:150px");
        doc.resolve(0.0);

        assert_eq!(
            hit(&doc, 20.0, 20.0),
            Some(later),
            "the overlap is the later box"
        );
        assert!(
            hoisted_clip(&doc, later).is_some(),
            "it keeps the clipper's clip"
        );
    }

    /// The other half of tree order: hoisting the fixed box must not lift it over a positioned
    /// box after it under the same clip.
    #[test]
    fn a_later_positioned_sibling_under_a_clip_stays_above_the_fixed_box() {
        let (mut doc, body) = page();
        let clipper = append(&mut doc, body, CLIPPER);
        let header = append(&mut doc, clipper, "height:20px");
        let fixed = append(&mut doc, header, FIXED);
        let later = append(&mut doc, clipper, "position:relative;height:80px");
        doc.resolve(0.0);

        assert_eq!(
            hit(&doc, 20.0, 30.0),
            Some(later),
            "the overlap is the later box"
        );
        assert_eq!(
            hit(&doc, 20.0, 15.0),
            Some(fixed),
            "the rest is the fixed box"
        );
    }

    const ROUNDED_CLIPPER: &str = "width:200px;height:100px;overflow:hidden;border-radius:20px";

    /// Whether `id` paints with `parent`, inside its clip layer, which paint shapes with the
    /// parent's corner radii, rather than hoisted with a rectangular clip.
    fn paints_with(doc: &BaseDocument, parent: NodeId, id: NodeId) -> bool {
        let context = doc.root_element().stacking_context.as_ref().unwrap();
        let is_hoisted = context.children.iter().any(|child| child.node_id == id);
        let paints = doc.nodes[parent].paint_children.borrow().clone();
        !is_hoisted && paints.unwrap_or_default().contains(&id)
    }

    /// A rounded clipper is an overflow ancestor like any other: a fixed box whose containing
    /// block is the viewport leaves it, so it is painted hoisted and hit past its corners.
    #[test]
    fn a_fixed_box_is_not_clipped_by_a_rounded_overflow_ancestor() {
        let (mut doc, body) = page();
        let clipper = append(&mut doc, body, ROUNDED_CLIPPER);
        let fixed = append(
            &mut doc,
            clipper,
            "position:fixed;left:0;top:80px;width:50px;height:50px",
        );
        doc.resolve(0.0);

        assert!(
            !paints_with(&doc, clipper, fixed),
            "hoisted out of the rounded clip"
        );
        assert_eq!(hoisted_clip(&doc, fixed), None);
        assert_eq!(hit(&doc, 10.0, 110.0), Some(fixed), "past the clipper");
    }

    /// The same for a positioned box hoisted for following a fixed box elsewhere on the page.
    #[test]
    fn a_positioned_box_under_a_rounded_clip_after_a_fixed_box_paints_inside_its_corners() {
        let (mut doc, body) = page();
        append(
            &mut doc,
            body,
            "position:fixed;left:0;top:200px;width:400px;height:20px",
        );
        let clipper = append(&mut doc, body, ROUNDED_CLIPPER);
        let badge = append(&mut doc, clipper, "position:relative;height:100px");
        doc.resolve(0.0);

        assert!(
            paints_with(&doc, clipper, badge),
            "painted inside the rounded clip"
        );
        assert_eq!(
            hit(&doc, 100.0, 50.0),
            Some(badge),
            "at the clipper's centre"
        );
    }

    const EFFECTS: [&str; 4] = [
        "filter:blur(1px)",
        "backdrop-filter:blur(1px)",
        "clip-path:inset(0)",
        "mask-image:linear-gradient(black,black)",
    ];

    /// A static stacking context sorts at CSS 2.1 Appendix E step 8, not with in-flow content.
    /// Two resolves, as a `transform` is known to `is_stacking_context_root` from the second.
    #[test]
    fn a_static_stacking_context_paints_above_its_later_in_flow_sibling() {
        for property in EFFECTS
            .iter()
            .chain(&["opacity:0.5", "transform:translate(1px,0)"])
        {
            let (mut doc, body) = page();
            let stacked = append(
                &mut doc,
                body,
                &format!("width:100px;height:100px;{property}"),
            );
            let later = append(&mut doc, body, "width:100px;height:100px;margin-top:-50px");
            doc.resolve(0.0);
            doc.resolve(0.0);

            assert_eq!(
                hit(&doc, 10.0, 75.0),
                Some(stacked),
                "{property}: above the in-flow sibling after it"
            );
            assert_eq!(
                hit(&doc, 10.0, 125.0),
                Some(later),
                "{property}: the sibling shows past it"
            );
        }
    }

    /// Step 8 is tree order, so an effect box after a fixed box paints above it, and, being
    /// hoisted with it, above in-flow content that follows the effect box's static parent.
    #[test]
    fn a_static_effect_box_after_a_fixed_box_paints_above_it_and_all_in_flow_content() {
        for effect in EFFECTS {
            let (mut doc, body) = page();
            append(
                &mut doc,
                body,
                "position:fixed;left:0;top:0;width:100px;height:100px",
            );
            let parent = append(&mut doc, body, "width:200px;height:100px");
            let effect_box = append(
                &mut doc,
                parent,
                &format!("width:100px;height:100px;{effect}"),
            );
            let later = append(&mut doc, body, "width:200px;height:100px;margin-top:-50px");
            doc.resolve(0.0);

            assert_eq!(
                hit(&doc, 10.0, 10.0),
                Some(effect_box),
                "{effect}: above the earlier fixed box"
            );
            assert_eq!(
                hit(&doc, 10.0, 75.0),
                Some(effect_box),
                "{effect}: above the in-flow content after its parent"
            );
            assert_eq!(
                hit(&doc, 150.0, 75.0),
                Some(later),
                "{effect}: the later content shows past it"
            );
        }
    }

    /// An effect box is a stacking context: a `z-index` inside it orders only within it, so
    /// the descendant stays under a later `z-index: 0` sibling of the effect box.
    #[test]
    fn a_z_index_descendant_of_a_static_effect_box_stays_inside_it() {
        for effect in EFFECTS {
            let (mut doc, body) = page();
            let effect_box = append(
                &mut doc,
                body,
                &format!("width:100px;height:100px;{effect}"),
            );
            let inner = append(
                &mut doc,
                effect_box,
                "position:absolute;z-index:1;left:0;top:0;width:50px;height:50px",
            );
            let later = append(
                &mut doc,
                body,
                "position:relative;z-index:0;width:100px;height:100px;margin-top:-100px",
            );
            doc.resolve(0.0);

            let context = doc.nodes[effect_box].stacking_context.as_ref();
            assert!(
                context.is_some_and(|context| {
                    context.children.iter().any(|child| child.node_id == inner)
                }),
                "{effect}: hoisted no further than the effect box"
            );
            assert_eq!(
                hit(&doc, 10.0, 10.0),
                Some(later),
                "{effect}: under the later z-index:0 sibling"
            );
        }
    }

    /// Whether `id` paints in the group of `ancestor`, as a child or in its stacking context.
    fn paints_inside(doc: &BaseDocument, ancestor: NodeId, id: NodeId) -> bool {
        let node = &doc.nodes[ancestor];
        let is_paint_child = node
            .paint_children
            .borrow()
            .iter()
            .flatten()
            .any(|&child| child == id);
        let is_hoisted_child = node
            .stacking_context
            .as_ref()
            .is_some_and(|context| context.children.iter().any(|child| child.node_id == id));
        is_paint_child || is_hoisted_child
    }

    /// CSS 2.1 §9.9.1: a box belongs to one stacking context, whatever its containing block,
    /// so a fixed box stays in the group of each of these ancestors and takes its effect.
    /// Two resolves, as a `transform` is known to `is_stacking_context_root` from the second.
    #[test]
    fn a_fixed_box_paints_inside_the_group_of_an_effect_ancestor() {
        for effect in EFFECTS.iter().chain(&["transform:translate(1px,0)"]) {
            let (mut doc, body) = page();
            let ancestor = append(
                &mut doc,
                body,
                &format!("width:100px;height:100px;{effect}"),
            );
            let fixed = append(&mut doc, ancestor, FIXED);
            doc.resolve(0.0);
            doc.resolve(0.0);

            assert!(
                paints_inside(&doc, ancestor, fixed),
                "{effect}: painted in the ancestor's group, not hoisted past it"
            );
        }
    }
}
