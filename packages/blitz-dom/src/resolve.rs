//! Resolve style and layout

use blitz_traits::node_id::NodeId;
use std::cell::RefCell;

use debug_timer::debug_timer;
use kurbo::{Affine, Rect};
use parley::LayoutContext;
use selectors::Element as _;
use style::dom::TDocument;

#[cfg(feature = "parallel-construct")]
use rayon::prelude::*;

// FIXME: static thread_local FontCtx isn't necessarily correct in multi-document context.
// Should use thread_local crate with ThreadLocal value store in the Document.
thread_local! {
    pub(crate) static LAYOUT_CTX: RefCell<Option<Box<LayoutContext<TextBrush>>>> = const { RefCell::new(None) };
}

use app_units::Au;
use std::collections::HashMap;
use style::invalidation::element::restyle_hints::RestyleHint;
use style::selector_parser::RestyleDamage;
use style::values::computed::ContainerType;
use taffy::AvailableSpace;

use crate::{
    BaseDocument,
    layout::{
        construct::{
            ConstructionTask, ConstructionTaskData, ConstructionTaskResult,
            ConstructionTaskResultData, LayoutChildren, build_inline_layout_into,
            collect_layout_children,
        },
        damage::{ALL_DAMAGE, CONSTRUCT_BOX, CONSTRUCT_DESCENDENT, CONSTRUCT_FC},
    },
    node::TextBrush,
};

/// The content-box width and height of a size container. An axis the container does not
/// contain is `None`.
pub(crate) type ContainerExtent = euclid::default::Size2D<Option<Au>>;

pub(crate) fn content_extent(layout: &taffy::Layout) -> ContainerExtent {
    ContainerExtent::new(
        Some(Au::from_f32_px(layout.content_box_width())),
        Some(Au::from_f32_px(layout.content_box_height())),
    )
}

impl BaseDocument {
    /// Restyle the tree and then relayout it
    pub fn resolve(&mut self, current_time_for_animations: f64) {
        if TDocument::as_node(&self.root_node())
            .first_element_child()
            .is_none()
        {
            #[cfg(feature = "tracing")]
            tracing::warn!("No DOM - not resolving");
            return;
        }

        // Process messages that have been sent to our message channel (e.g. loaded resource)
        self.handle_messages();

        // While render-blocking resources (e.g. stylesheets linked from the `<head>`) are
        // still loading, don't resolve styles or layout (matching how browsers block
        // rendering). Resolving styles before the document's stylesheets have loaded would
        // give elements computed styles based on an incomplete cascade, and a later restyle
        // (once the stylesheet loads) would treat those as genuine "before-change styles",
        // spuriously starting CSS transitions from unstyled values. See issue #689.
        //
        // `handle_messages` above must still run so that loaded resources are ingested and
        // this state can clear.
        if self.has_pending_critical_resources() {
            return;
        }

        self.resolve_scroll_animation();

        // Drop scrollbar-activity entries whose fade-out has finished (also
        // sheds entries for removed nodes).
        {
            use crate::node::scrollbar::{FADE_DELAY, FADE_DURATION};
            self.scrollbar_activity
                .retain(|_, last| last.elapsed() < FADE_DELAY + FADE_DURATION);
        }

        let root_node_id = self.root_element().id;
        debug_timer!(timer, feature = "log-phase-times");

        // The second pass exists only for `@container` size queries, which read the layout
        // the first pass produced. It runs at most once so a query that keeps resizing its
        // own container cannot loop; the next resolve picks up whatever is still unsettled.
        for is_container_pass in [false, true] {
            // we need to resolve stylist first since it will need to drive our layout bits
            self.resolve_stylist(current_time_for_animations);
            self.resolve_view_transition();
            timer.record_time("style");

            // Before damage propagation, which carries the damage of changed counter text
            self.resolve_counters();
            timer.record_time("counters");

            // Propagate damage flags (from mutation and restyles) up and down the tree
            if self.incremental_layout {
                self.propagate_damage_flags(root_node_id, RestyleDamage::empty());
                timer.record_time("damage");
            }

            // Fix up tree for layout (insert anonymous blocks as necessary, etc)
            self.resolve_layout_children();
            timer.record_time("construct");

            self.resolve_deferred_tasks();
            // Flush background/mask images from style to dedicated storage on the
            // nodes whose style changed (queued by the style traversal and by
            // pseudo-element box construction), fetching any not-yet-loaded images.
            self.flush_pending_style_images();
            #[cfg(feature = "svg")]
            for node_id in std::mem::take(&mut self.pending_svg_reparse_nodes) {
                // The node may have been removed between queueing and now
                if self.nodes.contains_key(node_id) {
                    crate::layout::construct::parse_inline_svg(self, node_id);
                }
            }
            #[cfg(not(feature = "svg"))]
            self.pending_svg_reparse_nodes.clear();
            timer.record_time("pconstruct");

            // Merge stylo into taffy
            self.flush_styles_to_layout(root_node_id);
            timer.record_time("flush");

            // Next we resolve layout with the data resolved by stlist
            self.resolve_layout();
            timer.record_time("layout");

            if is_container_pass || !self.restyle_resized_size_containers() {
                break;
            }
        }

        // Resolve transforms
        self.resolve_transforms(root_node_id);
        timer.record_time("transform");

        // Clear all damage and dirty flags, walking only subtrees which are
        // marked as (potentially) containing damage.
        if self.incremental_layout {
            let doc_node_id = self.root_node().id;
            self.clear_damage_and_dirty_flags(doc_node_id);
            timer.record_time("c_damage");
        }

        // Re-resolve the hover node from the pointer position against the fresh
        // layout. This must run *after* the damage/dirty flags are cleared
        // above, so that the restyle hint and ancestor `dirty_descendants`
        // flags set by any resulting hover change survive into the next resolve
        // pass (the clearing loop would otherwise wipe them). Any resulting
        // restyle is picked up on the next resolve pass; a redraw is requested
        // if the hovered node actually changes.
        self.refresh_hover();

        let mut subdoc_is_animating = false;
        for &node_id in &self.sub_document_nodes {
            let node = &mut self.nodes[node_id];
            let size = node.final_layout().size;
            if let Some(mut sub_doc) = node.subdoc_mut().map(|doc| doc.inner_mut()) {
                // Set viewport
                // viewport_mut handles change detection. So we just unconditionally set the values;
                let mut sub_viewport = sub_doc.viewport_mut();
                sub_viewport.hidpi_scale = self.viewport.hidpi_scale;
                sub_viewport.zoom = self.viewport.zoom;
                sub_viewport.color_scheme = self.viewport.color_scheme;

                let viewport_scale = self.viewport.scale();
                sub_viewport.window_size = (
                    (size.width * viewport_scale) as u32,
                    (size.height * viewport_scale) as u32,
                );
                drop(sub_viewport);

                sub_doc.resolve(current_time_for_animations);

                subdoc_is_animating |= sub_doc.is_animating();
            }
        }
        self.subdoc_is_animating = subdoc_is_animating;
        timer.record_time("subdocs");

        timer.print_times(&format!("Resolve({}): ", self.id()));
    }

    fn resolve_transforms(&mut self, node_id: NodeId) -> Rect {
        if !self.nodes.contains_key(node_id) {
            return Rect::ZERO;
        }

        if !self.nodes[node_id]
            .damage()
            .map(|d| d.contains(style::selector_parser::RestyleDamage::RECALCULATE_OVERFLOW))
            .unwrap_or(false)
        {
            return self.overflow_in_parent_frame(node_id);
        }

        let scale = self.viewport.scale_f64();

        self.nodes[node_id].set_transform(scale as f32);

        let w = self.nodes[node_id].final_layout().size.width as f64 * scale;
        let h = self.nodes[node_id].final_layout().size.height as f64 * scale;
        let mut overflow = Rect::new(0.0, 0.0, w, h);

        let layout_children = std::mem::take(self.nodes[node_id].layout_children.get_mut());

        if let Some(ref children) = layout_children {
            for &child_id in children {
                let child_rect_in_self = self.resolve_transforms(child_id);
                overflow = overflow.union(child_rect_in_self);
            }
        }
        if let Some(before) = self.nodes[node_id].before() {
            let child_rect_in_self = self.resolve_transforms(before);
            overflow = overflow.union(child_rect_in_self);
        }
        if let Some(after) = self.nodes[node_id].after() {
            let child_rect_in_self = self.resolve_transforms(after);
            overflow = overflow.union(child_rect_in_self);
        }

        *self.nodes[node_id].scrollable_overflow_mut() = overflow;
        *self.nodes[node_id].layout_children.get_mut() = layout_children;

        self.overflow_in_parent_frame(node_id)
    }

    /// The node's stored overflow, which is in its own frame, in its layout parent's.
    fn overflow_in_parent_frame(&self, node_id: NodeId) -> Rect {
        let scale = self.viewport.scale_f64();
        let node = &self.nodes[node_id];
        let location = node.final_layout().location;
        let translate = Affine::translate((location.x as f64 * scale, location.y as f64 * scale));
        let full = match *node.transform() {
            Some(t) => translate * t,
            None => translate,
        };
        full.transform_rect_bbox(*node.scrollable_overflow())
    }

    /// Ensure that the layout_children field is populated for all nodes
    pub fn resolve_layout_children(&mut self) {
        resolve_layout_children_recursive(self, self.root_node().id);

        fn resolve_layout_children_recursive(doc: &mut BaseDocument, node_id: NodeId) {
            // Anonymous blocks and pseudo-elements can be removed from the slab
            // between render passes. Bail out rather than panicking on a stale key.
            if doc.nodes.get(node_id).is_none() {
                return;
            }

            let mut damage = doc.nodes[node_id].damage().unwrap_or(ALL_DAMAGE);
            let _flags = doc.nodes[node_id].flags;

            if !doc.incremental_layout || damage.intersects(CONSTRUCT_FC | CONSTRUCT_BOX) {
                //} || flags.contains(NodeFlags::IS_INLINE_ROOT) {

                // Deallocate the anonymous blocks created for this node in the
                // previous construction round. They live only in the slab, so
                // reconstructing without freeing them would leak a slab entry per
                // anonymous block per reconstruction.
                let old_anonymous_blocks = std::mem::take(&mut doc.nodes[node_id].anonymous_blocks);
                for anon_id in old_anonymous_blocks {
                    doc.deallocate_anonymous_block(anon_id);
                }

                let mut collected = LayoutChildren::default();
                collect_layout_children(doc, node_id, &mut collected);
                let layout_children = collected.children;
                doc.nodes[node_id].anonymous_blocks = collected.anonymous_blocks;

                // Recurse into newly collected layout children
                for child_id in layout_children.iter().copied() {
                    resolve_layout_children_recursive(doc, child_id);
                    doc.nodes[child_id].layout_parent.set(Some(node_id));
                    if let Some(mut data) = doc.nodes[child_id]
                        .stylo_element_data_opt_mut()
                        .and_then(|s| s.get_mut())
                    {
                        data.damage
                            .remove(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                    }
                }

                *doc.nodes[node_id].layout_children.borrow_mut() = Some(layout_children.clone());
                // *doc.nodes[node_id].paint_children.borrow_mut() = Some(layout_children);

                damage.remove(CONSTRUCT_DESCENDENT | CONSTRUCT_FC | CONSTRUCT_BOX);
                // damage.insert(RestyleDamage::RELAYOUT | RestyleDamage::REPAINT);
            } else {
                //if damage.contains(CONSTRUCT_DESCENDENT) {
                let layout_children = doc.nodes[node_id].layout_children.borrow_mut().take();
                if let Some(layout_children) = layout_children {
                    for child_id in layout_children.iter().copied() {
                        // Anonymous blocks and pseudo-elements can be removed from the
                        // slab between render passes; skip stale IDs.
                        if !doc.nodes.contains_key(child_id) {
                            continue;
                        }
                        resolve_layout_children_recursive(doc, child_id);
                        doc.nodes[child_id].layout_parent.set(Some(node_id));
                    }

                    *doc.nodes[node_id].layout_children.borrow_mut() = Some(layout_children);
                }

                // damage.remove(CONSTRUCT_DESCENDENT);
                // damage.insert(RestyleDamage::RELAYOUT | RestyleDamage::REPAINT);
            }

            doc.nodes[node_id].set_damage(damage);
        }
    }

    pub fn resolve_deferred_tasks(&mut self) {
        let mut deferred_construction_nodes = std::mem::take(&mut self.deferred_construction_nodes);

        // Deduplicate deferred tasks by node_id to avoid redundant work
        deferred_construction_nodes.sort_unstable_by_key(|task| task.node_id);
        deferred_construction_nodes.dedup_by_key(|task| task.node_id);

        #[cfg(feature = "parallel-construct")]
        let iter = deferred_construction_nodes.into_par_iter();
        #[cfg(not(feature = "parallel-construct"))]
        let iter = deferred_construction_nodes.into_iter();

        let results: Vec<ConstructionTaskResult> = iter
            .map(|task: ConstructionTask| match task.data {
                ConstructionTaskData::InlineLayout(mut layout) => {
                    #[cfg(feature = "parallel-construct")]
                    let mut layout_ctx = LAYOUT_CTX
                        .take()
                        .unwrap_or_else(|| Box::new(LayoutContext::new()));
                    #[cfg(feature = "parallel-construct")]
                    let layout_ctx_mut = &mut layout_ctx;

                    #[cfg(feature = "parallel-construct")]
                    let mut font_ctx = self
                        .thread_font_contexts
                        .get_or(|| RefCell::new(Box::new(self.font_ctx.lock().unwrap().clone())))
                        .borrow_mut();
                    #[cfg(feature = "parallel-construct")]
                    let font_ctx_mut = &mut *font_ctx;

                    #[cfg(not(feature = "parallel-construct"))]
                    let layout_ctx_mut = &mut self.layout_ctx;
                    #[cfg(not(feature = "parallel-construct"))]
                    let font_ctx_mut = &mut *self.font_ctx.lock().unwrap();

                    layout.content_widths = None;
                    build_inline_layout_into(
                        &self.nodes,
                        layout_ctx_mut,
                        font_ctx_mut,
                        &mut layout,
                        self.viewport.scale(),
                        task.node_id,
                        None,
                    );

                    #[cfg(feature = "parallel-construct")]
                    {
                        LAYOUT_CTX.set(Some(layout_ctx));
                    }

                    // If layout doesn't contain any inline boxes, then it is safe to populate the content_widths
                    // cache during this parallelized stage.
                    // if layout.layout.inline_boxes().is_empty() {
                    //     layout.content_widths();
                    // }

                    ConstructionTaskResult {
                        node_id: task.node_id,
                        data: ConstructionTaskResultData::InlineLayout(layout),
                    }
                }
            })
            .collect();

        for result in results {
            match result.data {
                ConstructionTaskResultData::InlineLayout(layout) => {
                    self.nodes[result.node_id].cache_mut().clear();
                    self.nodes[result.node_id]
                        .element_data_mut()
                        .unwrap()
                        .inline_layout_data = Some(layout);
                }
            }
        }

        self.deferred_construction_nodes.clear();
    }

    /// Walk the nodes now that they're properly styled and transfer their styles to the taffy style system
    ///
    /// TODO: update taffy to use an associated type instead of slab key
    /// TODO: update taffy to support traited styles so we don't even need to rely on taffy for storage
    pub fn resolve_layout(&mut self) {
        let size = self.stylist.device().au_viewport_size();

        let available_space = taffy::Size {
            width: AvailableSpace::Definite(size.width.to_f32_px()),
            height: AvailableSpace::Definite(size.height.to_f32_px()),
        };

        let root_element_id = crate::taffy_node_id(self.root_element().id);

        // println!("\n\nRESOLVE LAYOUT\n===========\n");

        taffy::compute_root_layout(self, root_element_id, available_space);
        self.layout_fixed_boxes();
        taffy::round_layout(self, root_element_id);
        self.map_vertical_layouts(self.root_element().id);
        self.layout_sticky_boxes();

        // println!("\n\n");
        // taffy::print_tree(self, root_node_id)
    }

    /// Restyle the descendants of every size container whose contained axes changed since
    /// the style pass last read them: stylo does not track container sizes, and the
    /// `@container` rules of those descendants were matched against the old size. Returns
    /// whether any container was marked.
    fn restyle_resized_size_containers(&mut self) -> bool {
        let mut sizes = HashMap::new();
        let mut resized = Vec::new();
        for (node_id, node) in self.nodes.iter() {
            let (Some(element), Some(style)) = (node.element_data(), node.primary_styles()) else {
                continue;
            };
            let container_type = style.get_box().clone_container_type();
            let extent = content_extent(&element.unrounded_layout);
            let contained = if container_type.intersects(ContainerType::SIZE) {
                extent
            } else if container_type.intersects(ContainerType::INLINE_SIZE) {
                if style.writing_mode.is_horizontal() {
                    ContainerExtent::new(extent.width, None)
                } else {
                    ContainerExtent::new(None, extent.height)
                }
            } else {
                continue;
            };
            // A container the style pass has not seen was styled against an unlaid-out box
            let unseen = ContainerExtent::new(
                contained.width.map(|_| Au(0)),
                contained.height.map(|_| Au(0)),
            );
            let seen = self
                .container_query_sizes
                .get(&node_id)
                .copied()
                .unwrap_or(unseen);
            if contained != seen {
                resized.push(node_id);
            }
            sizes.insert(node_id, contained);
        }
        self.container_query_sizes = sizes;
        for &node_id in &resized {
            self.nodes[node_id].set_restyle_hint(RestyleHint::RESTYLE_DESCENDANTS);
        }
        !resized.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Attribute, DocumentConfig};
    use kurbo::Point;
    use markup5ever::{LocalName, QualName, ns};

    fn name(local: &str) -> QualName {
        QualName::new(None, ns!(html), LocalName::from(local))
    }

    fn block(doc: &mut BaseDocument, style: &str) -> NodeId {
        let attrs = vec![Attribute {
            name: name("style"),
            value: format!("display:block;{style}"),
        }];
        doc.mutate().create_element(name("div"), attrs)
    }

    /// An attribute write damages only its node and that node's ancestors, so the
    /// parent recomputes its overflow from a sibling that kept its old one. A parent
    /// that lost the sibling's real position prunes it from every later hit test.
    #[test]
    fn a_write_beside_an_out_of_box_sibling_keeps_it_in_the_parent_overflow() {
        const SIBLING: &str = "position:absolute;left:200px;top:300px;width:40px;height:40px;";
        for (extra, inside) in [
            ("", Point::new(220.0, 320.0)),
            ("transform:translateX(100px);", Point::new(320.0, 320.0)),
        ] {
            let mut doc = BaseDocument::new(DocumentConfig::default());
            let html = block(&mut doc, "");
            let body = block(&mut doc, "");
            let parent = block(&mut doc, "");
            let written = block(&mut doc, "height:10px;");
            let sibling = block(&mut doc, &format!("{SIBLING}{extra}"));
            {
                let mut m = doc.mutate();
                let root = m.doc.root_node().id;
                m.append_children(root, &[html]);
                m.append_children(html, &[body]);
                m.append_children(body, &[parent]);
                m.append_children(parent, &[written, sibling]);
            }
            doc.resolve(0.0);
            let before = *doc.nodes[parent].scrollable_overflow();
            assert!(before.contains(inside), "{extra:?}: {before:?}");

            doc.mutate().set_attribute(written, name("value"), "x");
            doc.resolve(0.0);

            assert_eq!(
                *doc.nodes[parent].scrollable_overflow(),
                before,
                "{extra:?}: the write moves nothing"
            );
        }
    }
}
