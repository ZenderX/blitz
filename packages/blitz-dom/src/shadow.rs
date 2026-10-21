//! Shadow DOM: shadow roots, slot assignment and the flat tree.
//!
//! A shadow root is an ordinary element node (named `#shadow-root`, which no
//! parsed document can produce) with no parent and a link to its host. It is
//! never part of a DOM child list: it is reached from its host through
//! [`Node::shadow_root_id`], and its children point back at it through
//! `parent`.
//!
//! The *flat tree* is what is laid out and styled: a host's flat children are
//! its shadow root's children, a `<slot>`'s flat children are the nodes
//! assigned to it (or its own children when nothing is assigned), and every
//! other node's flat children are its DOM children. Light children of a host
//! that no slot picks up are not part of the flat tree and generate no boxes.
//!
//! Slot assignment is computed on demand from the current DOM, so it can never
//! be stale; the cost is one walk of the shadow tree per query.

use std::borrow::Cow;
use std::fmt;

use blitz_traits::node_id::NodeId;
use markup5ever::{LocalName, QualName, local_name, ns};
use thin_vec::ThinVec;

use crate::layout::damage::ALL_DAMAGE;
use crate::node::NodeFlags;
use crate::{BaseDocument, ElementData, Node, NodeData, RestyleHint};

/// Local name given to the node that represents a shadow root.
const SHADOW_ROOT_NAME: &str = "#shadow-root";

/// How an element relates to shadow DOM.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ShadowLink {
    /// An ordinary element.
    #[default]
    None,
    /// A shadow host; `root` is its shadow root.
    Host { root: NodeId },
    /// A shadow root; `host` is its host.
    Root { host: NodeId },
}

/// The children of a node, taken out for iteration by
/// [`Node::take_children_for_iteration`].
pub(crate) struct TakenChildren {
    flat: Option<Vec<NodeId>>,
    dom: ThinVec<NodeId>,
}

impl TakenChildren {
    pub(crate) fn ids(&self) -> &[NodeId] {
        self.flat.as_deref().unwrap_or(&self.dom[..])
    }
}

/// Why [`BaseDocument::attach_shadow_root`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachShadowError {
    /// The id does not name a live node.
    NoSuchNode,
    /// Only elements can host a shadow root.
    NotAnElement,
    /// The host already has a shadow root.
    AlreadyAttached,
}

impl fmt::Display for AttachShadowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoSuchNode => "no such node",
            Self::NotAnElement => "only elements can host a shadow root",
            Self::AlreadyAttached => "the host already has a shadow root",
        })
    }
}

impl std::error::Error for AttachShadowError {}

impl Node {
    fn shadow_link(&self) -> ShadowLink {
        self.element_data().map_or(ShadowLink::None, |el| el.shadow)
    }

    /// Whether this node is a shadow root.
    pub fn is_shadow_root(&self) -> bool {
        matches!(self.shadow_link(), ShadowLink::Root { .. })
    }

    /// The shadow root attached to this node, if it is a shadow host.
    pub fn shadow_root_id(&self) -> Option<NodeId> {
        match self.shadow_link() {
            // A root dropped on its own leaves a stale link behind
            ShadowLink::Host { root } => self.tree().get(root).map(|_| root),
            _ => None,
        }
    }

    /// The host of this node, if it is a shadow root.
    pub fn shadow_host_id(&self) -> Option<NodeId> {
        match self.shadow_link() {
            ShadowLink::Root { host } => Some(host),
            _ => None,
        }
    }

    /// Whether this is an HTML `<slot>` element.
    pub fn is_slot_element(&self) -> bool {
        self.data.downcast_element().is_some_and(|el| {
            matches!(self.data, NodeData::Element(_))
                && el.name.ns == ns!(html)
                && &*el.name.local == "slot"
        })
    }

    /// The shadow root whose tree contains this node (this node itself if it is
    /// the root), if any.
    pub fn containing_shadow_root_id(&self) -> Option<NodeId> {
        let mut node = self;
        while let Some(parent_id) = node.parent {
            node = self.tree().get(parent_id)?;
        }
        node.is_shadow_root().then_some(node.id)
    }

    /// Whether this node lives inside a shadow tree (the root counts).
    pub fn is_in_shadow_tree(&self) -> bool {
        self.containing_shadow_root_id().is_some()
    }

    /// Elements and text nodes can be assigned to a slot.
    fn is_slottable(&self) -> bool {
        match &self.data {
            NodeData::Element(_) => !self.is_shadow_root(),
            NodeData::Text(_) => true,
            _ => false,
        }
    }

    /// The `name` of a `<slot>` (empty for the default slot).
    fn slot_name(&self) -> &str {
        self.data.attr(local_name!("name")).unwrap_or_default()
    }

    /// The `slot` attribute of a slottable (always empty for text).
    fn slottable_name(&self) -> &str {
        self.data.attr(LocalName::from("slot")).unwrap_or_default()
    }

    /// The first `<slot>` named `name` in the tree below `root`, in tree order.
    fn first_slot_named(&self, name: &str) -> Option<&Node> {
        let mut stack: Vec<NodeId> = self.children.iter().rev().copied().collect();
        while let Some(id) = stack.pop() {
            let node = self.tree().get(id)?;
            if node.is_slot_element() && node.slot_name() == name {
                return Some(node);
            }
            stack.extend(node.children.iter().rev().copied());
        }
        None
    }

    /// The slot this node is assigned to, if any.
    pub fn assigned_slot_id(&self) -> Option<NodeId> {
        if !self.is_slottable() {
            return None;
        }
        let parent = self.tree().get(self.parent?)?;
        let root = self.tree().get(parent.shadow_root_id()?)?;
        root.first_slot_named(self.slottable_name())
            .map(|slot| slot.id)
    }

    /// The nodes assigned to this `<slot>`. With `flatten`, assigned `<slot>`s
    /// (and the fallback content of an empty slot) are replaced by what they
    /// themselves resolve to, as in `assignedNodes({ flatten: true })`.
    ///
    /// Empty for anything that is not a `<slot>` inside a shadow tree.
    pub fn assigned_nodes(&self, flatten: bool) -> Vec<NodeId> {
        let mut out = Vec::new();
        self.collect_assigned_nodes(flatten, &mut out);
        out
    }

    fn collect_assigned_nodes(&self, flatten: bool, out: &mut Vec<NodeId>) {
        let Some(assigned) = self.direct_assigned_nodes() else {
            return;
        };
        if !flatten {
            out.extend(assigned);
            return;
        }
        let candidates: Vec<NodeId> = if assigned.is_empty() {
            self.children
                .iter()
                .copied()
                .filter(|id| self.tree().get(*id).is_some_and(Node::is_slottable))
                .collect()
        } else {
            assigned
        };
        for id in candidates {
            match self.tree().get(id) {
                Some(node) if node.is_slot_element() && node.is_in_shadow_tree() => {
                    node.collect_assigned_nodes(true, out);
                }
                Some(_) => out.push(id),
                None => {}
            }
        }
    }

    /// `None` if this is not a `<slot>` in a shadow tree; otherwise the host
    /// children that name this slot (empty if a same-named slot comes first).
    fn direct_assigned_nodes(&self) -> Option<Vec<NodeId>> {
        if !self.is_slot_element() {
            return None;
        }
        let root = self.tree().get(self.containing_shadow_root_id()?)?;
        let host = self.tree().get(root.shadow_host_id()?)?;
        let name = self.slot_name();
        if root.first_slot_named(name).map(|slot| slot.id) != Some(self.id) {
            return Some(Vec::new());
        }
        Some(
            host.children
                .iter()
                .copied()
                .filter(|id| {
                    self.tree()
                        .get(*id)
                        .is_some_and(|node| node.is_slottable() && node.slottable_name() == name)
                })
                .collect(),
        )
    }

    /// The children this node has in the flat tree.
    pub fn flat_children(&self) -> Cow<'_, [NodeId]> {
        if let Some(root) = self.shadow_root_id().and_then(|id| self.tree().get(id)) {
            return Cow::Borrowed(&root.children);
        }
        if self.is_slot_element() {
            if let Some(assigned) = self.direct_assigned_nodes() {
                if !assigned.is_empty() {
                    return Cow::Owned(assigned);
                }
            }
        }
        Cow::Borrowed(&self.children)
    }

    /// The flat children, but only for nodes where they differ from `children`.
    pub(crate) fn flat_children_override(&self) -> Option<Vec<NodeId>> {
        (self.shadow_root_id().is_some() || self.is_slot_element())
            .then(|| self.flat_children().into_owned())
    }

    /// Take the flat children out of the node so the tree can be mutated while
    /// they are visited; give them back with [`Node::restore_children`].
    ///
    /// Hosts and slots are not emptied: their descendants resolve slot
    /// assignment by reading the host's `children`, which must stay in place.
    pub(crate) fn take_children_for_iteration(&mut self) -> TakenChildren {
        let flat = self.flat_children_override();
        let dom = if flat.is_some() {
            ThinVec::new()
        } else {
            std::mem::take(&mut self.children)
        };
        TakenChildren { flat, dom }
    }

    /// Put back what [`Node::take_children_for_iteration`] took.
    pub(crate) fn restore_children(&mut self, taken: TakenChildren) {
        if taken.flat.is_none() {
            self.children = taken.dom;
        }
    }

    /// The parent of this node in the flat tree. A shadow root reports its host
    /// so that dirty-bit propagation can cross the shadow boundary; a light
    /// child of a host that no slot picks up has no flat parent.
    pub fn flat_parent_id(&self) -> Option<NodeId> {
        if let Some(host) = self.shadow_host_id() {
            return Some(host);
        }
        let parent_id = self.parent?;
        if self.is_anonymous() {
            return Some(parent_id);
        }
        let parent = self.tree().get(parent_id)?;
        if let Some(host) = parent.shadow_host_id() {
            return Some(host);
        }
        if parent.shadow_root_id().is_some() {
            return self.assigned_slot_id();
        }
        Some(parent_id)
    }

    /// The parent used when walking up to flag ancestors: the flat parent,
    /// falling back to the DOM parent.
    pub(crate) fn marking_parent_id(&self) -> Option<NodeId> {
        self.flat_parent_id().or(self.parent)
    }

    /// The DOM parent, or the host for a shadow root.
    pub(crate) fn parent_or_host_id(&self) -> Option<NodeId> {
        self.parent.or_else(|| self.shadow_host_id())
    }
}

impl BaseDocument {
    /// The shadow root attached to `host`, if any.
    pub fn shadow_root_of(&self, host: NodeId) -> Option<NodeId> {
        self.get_node(host)?.shadow_root_id()
    }

    /// The host of the shadow root `root`, if it is one.
    pub fn shadow_host_of(&self, root: NodeId) -> Option<NodeId> {
        self.get_node(root)?.shadow_host_id()
    }

    /// The slot `node` is assigned to, if any.
    pub fn assigned_slot(&self, node: NodeId) -> Option<NodeId> {
        self.get_node(node)?.assigned_slot_id()
    }

    /// The nodes assigned to `slot` (see [`Node::assigned_nodes`]).
    pub fn assigned_nodes(&self, slot: NodeId, flatten: bool) -> Vec<NodeId> {
        self.get_node(slot)
            .map(|node| node.assigned_nodes(flatten))
            .unwrap_or_default()
    }

    /// Attach a shadow root to `host` and return its id. Children appended to
    /// the returned node are rendered in place of the host's own children.
    pub fn attach_shadow_root(&mut self, host: NodeId) -> Result<NodeId, AttachShadowError> {
        let host_node = self.get_node(host).ok_or(AttachShadowError::NoSuchNode)?;
        if !matches!(host_node.data, NodeData::Element(_)) {
            return Err(AttachShadowError::NotAnElement);
        }
        if host_node.shadow_root_id().is_some() {
            return Err(AttachShadowError::AlreadyAttached);
        }
        let host_is_in_document = host_node.flags.is_in_document();

        let name = QualName::new(None, ns!(html), LocalName::from(SHADOW_ROOT_NAME));
        let mut data = ElementData::new(name, Vec::new());
        data.shadow = ShadowLink::Root { host };
        let root = self.create_node(NodeData::Element(Box::new(data)));
        self.nodes[root]
            .flags
            .set(NodeFlags::IS_IN_DOCUMENT, host_is_in_document);

        if let Some(host_data) = self.nodes[host].element_data_mut() {
            host_data.shadow = ShadowLink::Host { root };
        }
        self.invalidate_flat_tree(host);
        Ok(root)
    }

    /// Schedule restyle and box reconstruction for the flat tree of `host`.
    ///
    /// Every ancestor of a `<slot>` up to the host is damaged directly: slots are
    /// `display: contents`, so their hoisted content belongs to the nearest
    /// ancestor that generates a box, and damage on a `display: contents` node
    /// does not bubble up by itself.
    pub(crate) fn invalidate_flat_tree(&mut self, host: NodeId) {
        let Some(root_id) = self.shadow_root_of(host) else {
            return;
        };

        let mut slots = Vec::new();
        let mut stack = vec![root_id];
        while let Some(id) = stack.pop() {
            let Some(node) = self.get_node(id) else {
                continue;
            };
            if node.is_slot_element() {
                slots.push(id);
            }
            stack.extend(node.children.iter().copied());
        }
        for slot in slots {
            let mut current = Some(slot);
            while let Some(id) = current.filter(|id| *id != root_id) {
                let node = &mut self.nodes[id];
                node.insert_damage(ALL_DAMAGE);
                current = node.parent;
            }
        }

        let host_node = &mut self.nodes[host];
        if host_node.flags.is_in_document() {
            if let Some(mut data) = host_node
                .stylo_element_data_opt_mut()
                .and_then(|s| s.get_mut())
            {
                data.hint |= RestyleHint::restyle_subtree();
            }
        }
        host_node.insert_damage(ALL_DAMAGE);
        host_node.mark_ancestors_dirty();
    }

    /// Call after the child list of `parent` (or a text node directly below
    /// it) changed, to refresh the flat tree where that matters: below a shadow
    /// root, below a shadow host, and below a `<slot>`'s fallback content.
    pub(crate) fn invalidate_flat_tree_for_child_change(&mut self, parent: NodeId) {
        let Some(node) = self.get_node(parent) else {
            return;
        };
        let host = if let Some(host) = node.shadow_host_id() {
            Some(host)
        } else if node.shadow_root_id().is_some() {
            Some(parent)
        } else if node.is_slot_element() {
            node.containing_shadow_root_id()
                .and_then(|root| self.shadow_host_of(root))
        } else {
            None
        };
        if let Some(host) = host {
            self.invalidate_flat_tree(host);
        }
    }

    /// Call after the `slot` attribute of `node` or the `name` attribute of a
    /// `<slot>` changed.
    pub(crate) fn invalidate_flat_tree_for_attr_change(&mut self, node: NodeId, attr: &LocalName) {
        let Some(node_ref) = self.get_node(node) else {
            return;
        };
        if &**attr == "slot" {
            if let Some(parent) = node_ref.parent {
                self.invalidate_flat_tree_for_child_change(parent);
            }
        } else if *attr == local_name!("name") && node_ref.is_slot_element() {
            self.invalidate_flat_tree_for_child_change(node);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DocumentConfig;
    use markup5ever::{QualName, ns};

    fn el(name: &str) -> QualName {
        QualName::new(None, ns!(html), LocalName::from(name))
    }

    /// A document whose body is empty, with helpers to build the host tree.
    struct Fixture {
        doc: BaseDocument,
        body: NodeId,
    }

    impl Fixture {
        fn new() -> Self {
            let mut doc = BaseDocument::new(DocumentConfig::default());
            let body = {
                let mut m = doc.mutate();
                let html = m.create_element(el("html"), Vec::new());
                let body = m.create_element(el("body"), Vec::new());
                let root = m.doc.root_node().id;
                m.append_children(root, &[html]);
                m.append_children(html, &[body]);
                body
            };
            Self { doc, body }
        }

        /// Every element is `display: block` (so each generates its own layout
        /// box) except `<slot>`, which keeps its `display: contents` default.
        fn element(&mut self, tag: &str, attrs: &[(&str, &str)]) -> NodeId {
            let extra_style = attrs
                .iter()
                .find(|(name, _)| *name == "style")
                .map_or("", |(_, value)| *value);
            let style = if tag == "slot" {
                extra_style.to_string()
            } else {
                format!("display:block;{extra_style}")
            };
            let attrs = attrs
                .iter()
                .filter(|(name, _)| *name != "style")
                .map(|(name, value)| (*name, (*value).to_string()))
                .chain((!style.is_empty()).then_some(("style", style)))
                .map(|(name, value)| crate::Attribute {
                    name: el(name),
                    value,
                })
                .collect();
            self.doc.mutate().create_element(el(tag), attrs)
        }

        fn append(&mut self, parent: NodeId, children: &[NodeId]) {
            self.doc.mutate().append_children(parent, children);
        }

        /// A host in the body, its shadow tree `<div><slot a/><slot/></div>` and the
        /// light children `<b slot=a>`, `<i>`, `<u slot=zz>`.
        fn host_with_slots(&mut self) -> Hosted {
            let host = self.element("x-host", &[]);
            self.append(self.body, &[host]);
            let root = self.doc.attach_shadow_root(host).unwrap();
            let div = self.element("div", &[]);
            let slot_a = self.element("slot", &[("name", "a")]);
            let slot_default = self.element("slot", &[]);
            self.append(root, &[div, slot_a, slot_default]);
            let b = self.element("b", &[("slot", "a")]);
            let i = self.element("i", &[]);
            let u = self.element("u", &[("slot", "zz")]);
            self.append(host, &[b, i, u]);
            Hosted {
                host,
                root,
                div,
                slot_a,
                slot_default,
                b,
                i,
                u,
            }
        }

        fn layout_children(&mut self, node: NodeId) -> Vec<NodeId> {
            self.doc.resolve(0.0);
            self.doc.nodes[node]
                .layout_children
                .borrow()
                .as_ref()
                .map(|c| c.iter().copied().collect())
                .unwrap_or_default()
        }
    }

    struct Hosted {
        host: NodeId,
        root: NodeId,
        div: NodeId,
        slot_a: NodeId,
        slot_default: NodeId,
        b: NodeId,
        i: NodeId,
        u: NodeId,
    }

    #[test]
    fn attach_twice_is_an_error_value() {
        let mut f = Fixture::new();
        let host = f.element("x-host", &[]);
        f.append(f.body, &[host]);
        let root = f.doc.attach_shadow_root(host).unwrap();
        assert_eq!(f.doc.shadow_root_of(host), Some(root));
        assert_eq!(f.doc.shadow_host_of(root), Some(host));
        assert!(f.doc.nodes[root].is_shadow_root());
        assert_eq!(f.doc.nodes[root].parent, None);
        assert_eq!(
            f.doc.attach_shadow_root(host),
            Err(AttachShadowError::AlreadyAttached)
        );
        let text = f.doc.create_text_node("t");
        assert_eq!(
            f.doc.attach_shadow_root(text),
            Err(AttachShadowError::NotAnElement)
        );
    }

    #[test]
    fn flat_children_are_shadow_content_with_slotted_light_children() {
        let mut f = Fixture::new();
        let h = f.host_with_slots();

        // Named assignment: b -> slot[name=a], i -> default slot, u unassigned.
        assert_eq!(f.doc.assigned_slot(h.b), Some(h.slot_a));
        assert_eq!(f.doc.assigned_slot(h.i), Some(h.slot_default));
        assert_eq!(
            f.doc.assigned_slot(h.u),
            None,
            "a slot attribute naming no slot leaves the node unassigned"
        );
        assert_eq!(f.doc.assigned_nodes(h.slot_a, false), vec![h.b]);
        assert_eq!(f.doc.assigned_nodes(h.slot_default, false), vec![h.i]);

        // The host lays out [div, b, i]: the slots are display:contents and hoist
        // their assigned nodes; the unassigned <u> generates no box.
        assert_eq!(f.layout_children(h.host), vec![h.div, h.b, h.i]);
        assert!(f.doc.nodes[h.u].layout_parent.get().is_none());
        assert_eq!(f.doc.nodes[h.b].layout_parent.get(), Some(h.host));
    }

    #[test]
    fn empty_slot_renders_its_fallback_content() {
        let mut f = Fixture::new();
        let host = f.element("x-host", &[]);
        f.append(f.body, &[host]);
        let root = f.doc.attach_shadow_root(host).unwrap();
        let slot = f.element("slot", &[]);
        let fallback = f.element("span", &[]);
        f.append(root, &[slot]);
        f.append(slot, &[fallback]);

        assert_eq!(f.doc.assigned_nodes(slot, false), Vec::<NodeId>::new());
        assert_eq!(f.doc.assigned_nodes(slot, true), vec![fallback]);
        assert_eq!(f.layout_children(host), vec![fallback]);

        // Once something is assigned, the fallback is no longer rendered.
        let light = f.element("em", &[]);
        f.append(host, &[light]);
        assert_eq!(f.layout_children(host), vec![light]);
    }

    #[test]
    fn changing_a_slot_attribute_reassigns() {
        let mut f = Fixture::new();
        let h = f.host_with_slots();
        assert_eq!(f.layout_children(h.host), vec![h.div, h.b, h.i]);

        // Pointing <u> at slot "a" assigns it there; the old layout must not stick.
        f.doc.mutate().set_attribute(h.u, el("slot"), "a");
        assert_eq!(f.doc.assigned_nodes(h.slot_a, false), vec![h.b, h.u]);
        assert_eq!(f.layout_children(h.host), vec![h.div, h.b, h.u, h.i]);
    }

    #[test]
    fn duplicate_slot_names_assign_to_the_first_only() {
        let mut f = Fixture::new();
        let h = f.host_with_slots();
        let second_default = f.element("slot", &[]);
        f.append(h.root, &[second_default]);
        assert_eq!(f.doc.assigned_nodes(h.slot_default, false), vec![h.i]);
        assert_eq!(
            f.doc.assigned_nodes(second_default, false),
            Vec::<NodeId>::new()
        );
    }

    #[test]
    fn shadow_descendants_follow_the_host_connectivity() {
        let mut f = Fixture::new();
        let host = f.element("x-host", &[]);
        let root = f.doc.attach_shadow_root(host).unwrap();
        let inner = f.element("p", &[("id", "inner")]);
        f.append(root, &[inner]);
        assert!(!f.doc.nodes[inner].flags.is_in_document());

        f.append(f.body, &[host]);
        assert!(f.doc.nodes[root].flags.is_in_document());
        assert!(f.doc.nodes[inner].flags.is_in_document());
        assert!(
            f.doc.get_element_by_id("inner").is_none(),
            "ids inside a shadow tree are not visible to the document"
        );

        f.doc.mutate().remove_node(host);
        assert!(!f.doc.nodes[inner].flags.is_in_document());
    }

    #[test]
    fn host_height_is_the_sum_of_its_flat_children() {
        let mut f = Fixture::new();
        let host = f.element("x-host", &[]);
        f.append(f.body, &[host]);
        let root = f.doc.attach_shadow_root(host).unwrap();
        let inner = f.element("div", &[("style", "height:40px")]);
        let slot_a = f.element("slot", &[("name", "a")]);
        let slot = f.element("slot", &[]);
        f.append(root, &[inner, slot_a, slot]);
        let b = f.element("b", &[("slot", "a"), ("style", "height:10px")]);
        let i = f.element("i", &[("style", "height:10px")]);
        let u = f.element("u", &[("slot", "zz"), ("style", "height:10px")]);
        f.append(host, &[b, i, u]);
        f.doc.resolve(0.0);

        let height = |f: &Fixture, id: NodeId| f.doc.nodes[id].final_layout().size.height;
        assert_eq!(height(&f, host), 60.0, "40 + b + i; u is unslotted");
        assert_eq!(height(&f, u), 0.0, "an unslotted light child has no box");

        // Slotting <u> too must grow the already laid-out host.
        f.doc.mutate().set_attribute(u, el("slot"), "a");
        f.doc.resolve(0.0);
        assert_eq!(height(&f, host), 70.0);

        // And removing a light child must shrink it again.
        f.doc.mutate().remove_node(b);
        f.doc.resolve(0.0);
        assert_eq!(height(&f, host), 60.0);
    }

    #[test]
    fn style_inherits_through_the_flat_tree() {
        let mut f = Fixture::new();
        let host = f.element("x-host", &[("style", "color:rgb(1,2,3)")]);
        f.append(f.body, &[host]);
        let root = f.doc.attach_shadow_root(host).unwrap();
        let inner = f.element("div", &[]);
        let slot = f.element("slot", &[("style", "color:rgb(4,5,6)")]);
        f.append(root, &[inner, slot]);
        let light = f.element("i", &[]);
        f.append(host, &[light]);
        f.doc.resolve(0.0);

        let color = |id: NodeId| {
            format!(
                "{:?}",
                f.doc.nodes[id].primary_styles().unwrap().clone_color()
            )
        };
        assert_eq!(
            color(inner),
            color(host),
            "shadow children inherit from the host"
        );
        assert_ne!(
            color(light),
            color(host),
            "slotted nodes inherit from the slot"
        );
        assert_eq!(color(light), color(slot));
    }

    #[test]
    fn dropping_a_host_drops_its_shadow_tree() {
        let mut f = Fixture::new();
        let h = f.host_with_slots();
        f.doc.mutate().remove_and_drop_node(h.host);
        assert!(f.doc.get_node(h.root).is_none());
        assert!(f.doc.get_node(h.div).is_none());
    }

    /// Append a `<style>` with `css` below `parent`.
    fn add_style(f: &mut Fixture, parent: NodeId, css: &str) {
        let mut m = f.doc.mutate();
        let style = m.create_element(el("style"), Vec::new());
        let text = m.create_text_node(css);
        m.append_children(style, &[text]);
        m.append_children(parent, &[style]);
    }

    fn height(f: &Fixture, id: NodeId) -> f32 {
        f.doc.nodes[id].final_layout().size.height
    }

    #[test]
    fn host_rule_in_the_shadow_tree_styles_the_host() {
        let mut f = Fixture::new();
        let host = f.element("x-host", &[("class", "on")]);
        let plain = f.element("x-host", &[]);
        f.append(f.body, &[host, plain]);
        for h in [host, plain] {
            let root = f.doc.attach_shadow_root(h).unwrap();
            add_style(&mut f, root, ":host{height:50px} :host(.on){height:30px}");
        }
        f.doc.resolve(0.0);
        assert_eq!(height(&f, host), 30.0, ":host(.on) is more specific");
        assert_eq!(
            height(&f, plain),
            50.0,
            ":host(.on) must not match a plain host"
        );
    }

    #[test]
    fn document_rules_do_not_reach_shadow_internals() {
        let mut f = Fixture::new();
        let body = f.body;
        add_style(&mut f, body, ".inner{height:99px}");
        let light = f.element("div", &[("class", "inner")]);
        let host = f.element("x-host", &[]);
        f.append(f.body, &[light, host]);
        let root = f.doc.attach_shadow_root(host).unwrap();
        let inner = f.element("div", &[("class", "inner")]);
        f.append(root, &[inner]);
        f.doc.resolve(0.0);
        assert_eq!(
            height(&f, light),
            99.0,
            "the document rule matches in the document"
        );
        assert_eq!(height(&f, inner), 0.0, "but not inside the shadow tree");
    }

    #[test]
    fn slotted_rule_styles_assigned_elements_only() {
        let mut f = Fixture::new();
        let host = f.element("x-host", &[]);
        f.append(f.body, &[host]);
        let root = f.doc.attach_shadow_root(host).unwrap();
        add_style(&mut f, root, "::slotted(b){height:20px}");
        let slot = f.element("slot", &[]);
        f.append(root, &[slot]);
        let b = f.element("b", &[]);
        let i = f.element("i", &[]);
        f.append(host, &[b, i]);
        f.doc.resolve(0.0);
        assert_eq!(height(&f, b), 20.0);
        assert_eq!(height(&f, i), 0.0, "::slotted(b) does not match <i>");
    }

    #[test]
    fn outer_rule_beats_host_rule() {
        let mut f = Fixture::new();
        let body = f.body;
        add_style(&mut f, body, "x-host{height:70px}");
        let host = f.element("x-host", &[]);
        f.append(f.body, &[host]);
        let root = f.doc.attach_shadow_root(host).unwrap();
        add_style(&mut f, root, ":host{height:50px}");
        f.doc.resolve(0.0);
        assert_eq!(height(&f, host), 70.0);
    }

    #[test]
    fn changing_a_shadow_sheet_restyles_the_host() {
        let mut f = Fixture::new();
        let host = f.element("x-host", &[]);
        f.append(f.body, &[host]);
        let root = f.doc.attach_shadow_root(host).unwrap();
        add_style(&mut f, root, ":host{height:50px}");
        f.doc.resolve(0.0);
        assert_eq!(height(&f, host), 50.0);

        add_style(&mut f, root, ":host{height:80px}");
        f.doc.resolve(0.0);
        assert_eq!(height(&f, host), 80.0, "a later sheet wins and restyles");

        let style = f.doc.nodes[root].children[1];
        f.doc.mutate().remove_and_drop_node(style);
        f.doc.resolve(0.0);
        assert_eq!(
            height(&f, host),
            50.0,
            "removing the sheet restores the first"
        );
    }

    /// A host in the body whose shadow tree holds `<div part=…>` for each of `parts`.
    fn host_with_parts(f: &mut Fixture, host_tag: &str, parts: &[&str]) -> (NodeId, NodeId) {
        let host = f.element(host_tag, &[]);
        f.append(f.body, &[host]);
        let root = f.doc.attach_shadow_root(host).unwrap();
        for part in parts {
            let div = f.element("div", &[("part", part)]);
            f.append(root, &[div]);
        }
        (host, root)
    }

    #[test]
    fn part_rule_from_the_outer_tree_styles_the_part() {
        let mut f = Fixture::new();
        let body = f.body;
        add_style(
            &mut f,
            body,
            "x-host::part(label){height:33px} x-host::part(a b){height:44px}",
        );
        let (_, root) = host_with_parts(&mut f, "x-host", &["label", "plain", "a b c"]);
        let bare = f.element("div", &[]);
        f.append(root, &[bare]);
        f.doc.resolve(0.0);

        let divs = f.doc.nodes[root].children.clone();
        assert_eq!(height(&f, divs[0]), 33.0, "part=label matches");
        assert_eq!(height(&f, divs[1]), 0.0, "another part name does not");
        assert_eq!(height(&f, divs[2]), 44.0, "::part(a b) needs both names");
        assert_eq!(
            height(&f, bare),
            0.0,
            "an element without part never matches"
        );

        f.doc.mutate().set_attribute(divs[1], el("part"), "label");
        f.doc.resolve(0.0);
        assert_eq!(height(&f, divs[1]), 33.0, "changing `part` restyles");
    }

    #[test]
    fn exportparts_forwards_parts_through_nested_hosts() {
        let mut f = Fixture::new();
        let body = f.body;
        add_style(
            &mut f,
            body,
            "x-outer::part(outer){height:21px} x-outer::part(label){height:22px} \
             x-outer::part(hidden){height:23px} x-outer::part(both){height:24px}",
        );
        let (outer, outer_root) = host_with_parts(&mut f, "x-outer", &[]);
        let inner = f.element("x-inner", &[]);
        f.append(outer_root, &[inner]);
        let inner_root = f.doc.attach_shadow_root(inner).unwrap();
        let mut parts = Vec::new();
        for part in ["label", "hidden", "kept"] {
            let div = f.element("div", &[("part", part)]);
            f.append(inner_root, &[div]);
            parts.push(div);
        }
        // `label` is renamed, `kept` is exported twice, `:` and empty entries are dropped
        f.doc.mutate().set_attribute(
            inner,
            el("exportparts"),
            "label:outer, kept:both, kept:label2, :, bad:",
        );
        f.doc
            .mutate()
            .set_attribute(outer, el("exportparts"), "outer, both, label2:label");
        f.doc.resolve(0.0);

        assert_eq!(
            height(&f, parts[0]),
            21.0,
            "label:outer forwarded as `outer`"
        );
        assert_eq!(height(&f, parts[1]), 0.0, "an unexported part stays hidden");
        assert_eq!(height(&f, parts[2]), 24.0, "kept -> both -> both");
    }

    #[test]
    fn adopted_sheets_follow_the_trees_own_sheets() {
        let mut f = Fixture::new();
        let host = f.element("x-host", &[]);
        let other = f.element("x-host", &[]);
        f.append(f.body, &[host, other]);
        let roots: Vec<NodeId> = [host, other]
            .iter()
            .map(|h| f.doc.attach_shadow_root(*h).unwrap())
            .collect();
        add_style(&mut f, roots[0], ":host{height:50px}");
        let sheet = f.doc.create_adopted_stylesheet(":host{height:80px}");
        f.doc.set_adopted_stylesheets(Some(roots[0]), &[sheet]);
        f.doc.set_adopted_stylesheets(Some(roots[1]), &[sheet]);
        f.doc.resolve(0.0);
        assert_eq!(
            height(&f, host),
            80.0,
            "adopted sheet beats the earlier <style>"
        );
        assert_eq!(height(&f, other), 80.0, "one sheet is shared by two roots");

        // A <style> added later still sorts before the adopted sheet
        add_style(&mut f, roots[0], ":host{height:60px}");
        f.doc.resolve(0.0);
        assert_eq!(height(&f, host), 80.0);

        f.doc
            .replace_adopted_stylesheet(sheet, ":host{height:90px}");
        f.doc.resolve(0.0);
        assert_eq!(height(&f, host), 90.0, "replace updates every adopter");
        assert_eq!(height(&f, other), 90.0);

        f.doc.set_adopted_stylesheets(Some(roots[0]), &[]);
        f.doc.resolve(0.0);
        assert_eq!(
            height(&f, host),
            60.0,
            "dropping the list restores the <style>"
        );
        assert_eq!(height(&f, other), 90.0);

        f.doc.release_adopted_stylesheet(sheet);
        f.doc.resolve(0.0);
        assert_eq!(height(&f, other), 0.0, "releasing removes it everywhere");
    }

    #[test]
    fn adopted_document_sheets_apply_and_can_be_removed() {
        let mut f = Fixture::new();
        let body = f.body;
        add_style(&mut f, body, "x-host{height:70px}");
        let host = f.element("x-host", &[]);
        f.append(f.body, &[host]);
        let sheet = f.doc.create_adopted_stylesheet("x-host{height:77px}");
        f.doc.set_adopted_stylesheets(None, &[sheet]);
        f.doc.resolve(0.0);
        assert_eq!(height(&f, host), 77.0);

        f.doc
            .replace_adopted_stylesheet(sheet, "x-host{height:78px}");
        f.doc.resolve(0.0);
        assert_eq!(height(&f, host), 78.0);

        f.doc.set_adopted_stylesheets(None, &[]);
        f.doc.resolve(0.0);
        assert_eq!(height(&f, host), 70.0);
    }
}
