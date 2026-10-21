//! Enable the dom to participate in styling by servo
//!

use blitz_traits::node_id::NodeId;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use crate::StyleThreading;
use crate::layout::damage::compute_layout_damage;
use crate::node::Node;
use crate::node::NodeData;
use markup5ever::{LocalName, LocalNameStaticSet, Namespace, NamespaceStaticSet, local_name};
use selectors::bloom::BLOOM_HASH_MASK;
use selectors::{
    Element, OpaqueElement,
    attr::{AttrSelectorOperation, NamespaceConstraint},
    matching::{ElementSelectorFlags, MatchingContext, VisitedHandlingMode},
    sink::Push,
};
use style::CaseSensitivityExt;
use style::animation::AnimationSetKey;
use style::animation::AnimationState;
use style::applicable_declarations::ApplicableDeclarationBlock;
use style::bloom::each_relevant_element_hash;
use style::color::AbsoluteColor;
use style::data::{ElementDataMut, ElementDataRef};
use style::global_style_data::STYLE_THREAD_POOL;
use style::invalidation::element::restyle_hints::RestyleHint;
use style::properties::ComputedValues;
use style::properties::{Importance, PropertyDeclaration};
use style::rule_tree::CascadeLevel;
use style::rule_tree::CascadeOrigin;
use style::selector_parser::PseudoElement;
use style::selector_parser::RestyleDamage;
use style::stylesheets::layer_rule::LayerOrder;
use style::stylesheets::scope_rule::ImplicitScopeRoot;
use style::values::AtomString;
use style::values::specified::NoCalcPercentage;
use style::{
    Atom,
    context::{
        QuirksMode, RegisteredSpeculativePainter, RegisteredSpeculativePainters,
        SharedStyleContext, StyleContext,
    },
    dom::{LayoutIterator, NodeInfo, OpaqueNode, TDocument, TElement, TNode, TShadowRoot},
    global_style_data::GLOBAL_STYLE_DATA,
    properties::PropertyDeclarationBlock,
    selector_parser::{NonTSPseudoClass, SelectorImpl},
    servo_arc::{Arc, ArcBorrow},
    shared_lock::{Locked, SharedRwLock, StylesheetGuards},
    thread_state::ThreadState,
    traversal::{DomTraversal, PerLevelTraversalData},
    traversal_flags::TraversalFlags,
    values::{AtomIdent, GenericAtomIdent},
};
use style_dom::ElementState;

use style::values::computed::text::TextAlign as StyloTextAlign;

impl crate::document::BaseDocument {
    pub fn resolve_stylist(&mut self, now: f64) {
        style::thread_state::enter(ThreadState::LAYOUT);

        self.flush_shadow_styles();

        let guard = &self.guard;
        let guards = StylesheetGuards {
            author: &guard.read(),
            ua_or_user: &guard.read(),
        };

        let root = TDocument::as_node(&&self.nodes[self.root_node_id])
            .first_element_child()
            .unwrap()
            .as_element()
            .unwrap();

        self.stylist
            .flush(&guards)
            .process_style(root, Some(&self.snapshots));

        // Mark actively animating nodes as dirty
        let mut sets = self.animations.sets.write();
        for (key, set) in sets.iter_mut() {
            let node_id = NodeId::from_u64(key.node.id() as u64);

            // Drop animations belonging to nodes that are no longer in the
            // document. A removed element is never restyled, so it would never
            // get a chance to cancel its own animations; an infinite animation
            // would then keep `has_active_animations` set forever and force a
            // redraw every frame. Emptying the set here lets the `retain` below
            // discard it so the flag can clear on this same pass.
            let in_document = self
                .nodes
                .get(node_id)
                .is_some_and(|node| node.flags.is_in_document());
            if !in_document {
                set.animations.clear();
                set.transitions.clear();
                continue;
            }

            self.nodes[node_id].set_restyle_hint(RestyleHint::RESTYLE_SELF);

            for animation in set.animations.iter_mut() {
                if animation.state == AnimationState::Pending && animation.started_at <= now {
                    animation.state = AnimationState::Running;
                }
                animation.iterate_if_necessary(now);

                if animation.state == AnimationState::Running && animation.has_ended(now) {
                    animation.state = AnimationState::Finished;
                }
            }

            for transition in set.transitions.iter_mut() {
                if transition.state == AnimationState::Pending && transition.start_time <= now {
                    transition.state = AnimationState::Running;
                }
                if transition.state == AnimationState::Running && transition.has_ended(now) {
                    transition.state = AnimationState::Finished;
                }
            }
        }
        drop(sets);

        // Build the style context used by the style traversal
        let context = SharedStyleContext {
            traversal_flags: TraversalFlags::empty(),
            stylist: &self.stylist,
            options: GLOBAL_STYLE_DATA.options.clone(),
            guards,
            visited_styles_enabled: false,
            animations: self.animations.clone(),
            current_time_for_animations: now,
            snapshot_map: &self.snapshots,
            registered_speculative_painters: &RegisteredPaintersImpl,
        };

        // components/layout_2020/lib.rs:983
        let root = self.root_element();
        // dbg!(root);
        let token = RecalcStyle::pre_traverse(root, &context);

        let mut nodes_needing_style_image_flush = Vec::new();
        let mut svg_nodes_to_reparse = Vec::new();
        if token.should_traverse() {
            // Style the elements, resolving their data
            let mut traverser = RecalcStyle::new(context);
            // `Sequential` bypasses Stylo's global pool. See `StyleThreading`.
            let pool_guard = matches!(self.style_threading, StyleThreading::Parallel)
                .then(|| STYLE_THREAD_POOL.pool());
            let rayon_pool = pool_guard.as_ref().and_then(|g| g.as_ref());
            style::driver::traverse_dom(&traverser, token, rayon_pool);
            nodes_needing_style_image_flush =
                std::mem::take(traverser.nodes_needing_style_image_flush.get_mut().unwrap());
            svg_nodes_to_reparse =
                std::mem::take(traverser.svg_nodes_to_reparse.get_mut().unwrap());
        }
        self.pending_style_image_nodes
            .extend(nodes_needing_style_image_flush);
        for id in svg_nodes_to_reparse {
            if !self.pending_svg_reparse_nodes.contains(&id) {
                self.pending_svg_reparse_nodes.push(id);
            }
        }

        for opaque in self.snapshots.keys() {
            let id = NodeId::from_u64(opaque.id() as u64);
            if let Some(node) = self.nodes.get_mut(id) {
                node.set_has_snapshot(false);
            }
        }
        self.snapshots.clear();

        let mut sets = self.animations.sets.write();
        for set in sets.values_mut() {
            set.clear_canceled_animations();
            for animation in set.animations.iter_mut() {
                animation.is_new = false;
            }
            for transition in set.transitions.iter_mut() {
                transition.is_new = false;
            }
        }
        sets.retain(|_, state| !state.is_empty());
        self.has_active_animations = sets.values().any(|state| state.needs_animation_ticks());

        // Maybe run garbage collection. Stylo has internal to determine whether to run or not.
        self.stylist.rule_tree().maybe_gc();

        style::thread_state::exit(ThreadState::LAYOUT);
    }
}

/// The `(inner, outer)` pairs of an `exportparts` value: `ident` means
/// `ident:ident`, and entries that are not one or two idents are dropped.
fn exported_part_entries(value: &str) -> impl Iterator<Item = (&str, &str)> {
    let is_ident =
        |s: &str| !s.is_empty() && !s.contains(|c: char| c.is_ascii_whitespace() || c == ':');
    value.split(',').filter_map(move |entry| {
        let entry = entry.trim_ascii();
        let (inner, outer) = entry
            .split_once(':')
            .map_or((entry, entry), |(i, o)| (i.trim_ascii(), o.trim_ascii()));
        (is_ident(inner) && is_ident(outer)).then_some((inner, outer))
    })
}

/// A handle to a node that Servo's style traits are implemented against
///
/// Since BlitzNodes are not persistent (IE we don't keep the pointers around between frames), we choose to just implement
/// the tree structure in the nodes themselves, and temporarily give out pointers during the layout phase.
type BlitzNode<'a> = &'a Node;

impl<'a> TDocument for BlitzNode<'a> {
    type ConcreteNode = BlitzNode<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        self
    }

    fn is_html_document(&self) -> bool {
        true
    }

    fn quirks_mode(&self) -> QuirksMode {
        QuirksMode::NoQuirks
    }

    fn shared_lock(&self) -> &SharedRwLock {
        self.guard()
    }
}

impl NodeInfo for BlitzNode<'_> {
    fn is_element(&self) -> bool {
        Node::is_element(self)
    }

    fn is_text_node(&self) -> bool {
        Node::is_text_node(self)
    }
}

impl<'a> TShadowRoot for BlitzNode<'a> {
    type ConcreteNode = BlitzNode<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        self
    }

    fn host(&self) -> <Self::ConcreteNode as TNode>::ConcreteElement {
        // A shadow root is created with its host and dropped with it, so the
        // lookup cannot fail; stylo's signature has no way to say so, and
        // panicking is not an option, so a dangling link falls back to the root itself.
        self.shadow_host_id()
            .and_then(|id| self.tree().get(id))
            .unwrap_or(*self)
    }

    fn style_data<'b>(&self) -> Option<&'b style::stylist::CascadeData>
    where
        Self: 'b,
    {
        self.element_data()
            .and_then(|element| element.shadow_styles.as_deref())
            .map(|styles| &*styles.data)
    }
}

// components/styleaapper.rs:
impl<'a> TNode for BlitzNode<'a> {
    type ConcreteElement = BlitzNode<'a>;
    type ConcreteDocument = BlitzNode<'a>;
    type ConcreteShadowRoot = BlitzNode<'a>;

    fn parent_node(&self) -> Option<Self> {
        self.parent.map(|id| self.with(id))
    }

    fn first_child(&self) -> Option<Self> {
        self.children.first().map(|id| self.with(*id))
    }

    fn last_child(&self) -> Option<Self> {
        self.children.last().map(|id| self.with(*id))
    }

    fn prev_sibling(&self) -> Option<Self> {
        self.backward(1)
    }

    fn next_sibling(&self) -> Option<Self> {
        self.forward(1)
    }

    fn owner_doc(&self) -> Self::ConcreteDocument {
        // Walk up the parent chain (crossing shadow roots to their hosts) to the
        // root Document node.
        let mut node = *self;
        while let Some(parent_id) = node.parent_or_host_id() {
            node = node.with(parent_id);
        }
        node
    }

    fn is_in_document(&self) -> bool {
        true
    }

    // The parent in the flat tree: the host for children of a shadow root, the
    // slot for slotted nodes, the DOM parent otherwise.
    fn traversal_parent(&self) -> Option<Self::ConcreteElement> {
        self.flat_parent_id()
            .and_then(|id| self.tree().get(id))
            .and_then(|node| node.as_element())
    }

    fn opaque(&self) -> OpaqueNode {
        OpaqueNode(self.id.as_u64() as usize)
    }

    fn debug_id(self) -> usize {
        self.id.as_u64() as usize
    }

    fn as_element(&self) -> Option<Self::ConcreteElement> {
        match self.data {
            // A shadow root is stored as an element node but is not an element
            NodeData::Element { .. } if !self.is_shadow_root() => Some(self),
            _ => None,
        }
    }

    fn as_document(&self) -> Option<Self::ConcreteDocument> {
        match self.data {
            NodeData::Document(_) => Some(self),
            _ => None,
        }
    }

    fn as_shadow_root(&self) -> Option<Self::ConcreteShadowRoot> {
        self.is_shadow_root().then_some(*self)
    }
}

impl selectors::Element for BlitzNode<'_> {
    type Impl = SelectorImpl;

    fn opaque(&self) -> selectors::OpaqueElement {
        // This correctly uses a unique id for the OpaqueElement (unlike using a pointer to the "slot")
        // However, it makes it impossible for us to "rehydrate" the OpaqueElement back into an actual Element
        // which is required to implement the `implicit_scope_for_sheet_in_shadow_root` method below
        //
        // We should see if selectors will accept a PR that allows us to use 128bits for the OpaqueElement. Or
        // find some other solution that will enable "rehydration". This is required to enable and use the
        // Shadow DOM functionality in Stylo.
        let non_null =
            NonNull::new((self.id.as_u64() as usize).wrapping_add(1) as *mut ()).unwrap();
        OpaqueElement::from_non_null_ptr(non_null)
    }

    // The DOM parent: selectors must not see across a shadow boundary, so a
    // child of a shadow root has no parent element.
    fn parent_element(&self) -> Option<Self> {
        self.parent_node().and_then(|node| node.as_element())
    }

    fn parent_node_is_shadow_root(&self) -> bool {
        self.parent
            .and_then(|id| self.tree().get(id))
            .is_some_and(Node::is_shadow_root)
    }

    fn containing_shadow_host(&self) -> Option<Self> {
        let root = self.tree().get(self.containing_shadow_root_id()?)?;
        self.tree().get(root.shadow_host_id()?)
    }

    fn is_pseudo_element(&self) -> bool {
        matches!(self.data, NodeData::AnonymousBlock(_))
    }

    // These methods are implemented naively since we only threaded real nodes and not fake nodes
    // we should try and use `find` instead of this foward/backward stuff since its ugly and slow
    fn prev_sibling_element(&self) -> Option<Self> {
        let mut n = 1;
        while let Some(node) = self.backward(n) {
            if node.is_element() {
                return Some(node);
            }
            n += 1;
        }

        None
    }

    fn next_sibling_element(&self) -> Option<Self> {
        let mut n = 1;
        while let Some(node) = self.forward(n) {
            if node.is_element() {
                return Some(node);
            }
            n += 1;
        }

        None
    }

    fn first_element_child(&self) -> Option<Self> {
        let mut children = self.dom_children();
        children.find(|child| child.is_element())
    }

    fn is_html_element_in_html_document(&self) -> bool {
        true // self.has_namespace(ns!(html))
    }

    fn has_local_name(&self, local_name: &LocalName) -> bool {
        self.data.is_element_with_tag_name(local_name)
    }

    fn has_namespace(&self, ns: &Namespace) -> bool {
        self.element_data().expect("Not an element").name.ns == *ns
    }

    fn is_same_type(&self, other: &Self) -> bool {
        self.local_name() == other.local_name() && self.namespace() == other.namespace()
    }

    fn attr_matches(
        &self,
        _ns: &NamespaceConstraint<&GenericAtomIdent<NamespaceStaticSet>>,
        local_name: &GenericAtomIdent<LocalNameStaticSet>,
        operation: &AttrSelectorOperation<&AtomString>,
    ) -> bool {
        match self.data.attr(local_name.0.clone()) {
            None => false,
            Some(attr_value) => operation.eval_str(attr_value),
        }
    }

    fn match_non_ts_pseudo_class(
        &self,
        pseudo_class: &<Self::Impl as selectors::SelectorImpl>::NonTSPseudoClass,
        _context: &mut MatchingContext<Self::Impl>,
    ) -> bool {
        match *pseudo_class {
            NonTSPseudoClass::Active => self.element_state().contains(ElementState::ACTIVE),
            NonTSPseudoClass::AnyLink => self
                .element_state()
                .intersects(ElementState::VISITED_OR_UNVISITED),
            NonTSPseudoClass::Checked => self.element_state().contains(ElementState::CHECKED),
            NonTSPseudoClass::Valid => false,
            NonTSPseudoClass::Invalid => false,
            NonTSPseudoClass::Defined => false,
            NonTSPseudoClass::Disabled => self.element_state().contains(ElementState::DISABLED),
            NonTSPseudoClass::Enabled => self.element_state().contains(ElementState::ENABLED),
            NonTSPseudoClass::Focus => self.element_state().contains(ElementState::FOCUS),
            NonTSPseudoClass::FocusWithin => false,
            NonTSPseudoClass::FocusVisible => false,
            NonTSPseudoClass::Fullscreen => false,
            NonTSPseudoClass::Hover => self.element_state().contains(ElementState::HOVER),
            NonTSPseudoClass::Indeterminate => false,
            NonTSPseudoClass::Lang(_) => false,
            NonTSPseudoClass::CustomState(_) => false,
            NonTSPseudoClass::Link => self.element_state().contains(ElementState::UNVISITED),
            NonTSPseudoClass::PlaceholderShown => false,
            NonTSPseudoClass::ReadWrite => false,
            NonTSPseudoClass::ReadOnly => false,
            NonTSPseudoClass::ServoNonZeroBorder => false,
            NonTSPseudoClass::Target => false,
            NonTSPseudoClass::Visited => false,
            NonTSPseudoClass::Autofill => false,
            NonTSPseudoClass::Default => false,

            NonTSPseudoClass::InRange => false,
            NonTSPseudoClass::Modal => false,
            NonTSPseudoClass::Open => false,
            NonTSPseudoClass::Optional => false,
            NonTSPseudoClass::OutOfRange => false,
            NonTSPseudoClass::PopoverOpen => false,
            NonTSPseudoClass::Required => false,
            NonTSPseudoClass::UserInvalid => false,
            NonTSPseudoClass::UserValid => false,
            NonTSPseudoClass::MozMeterOptimum => false,
            NonTSPseudoClass::MozMeterSubOptimum => false,
            NonTSPseudoClass::MozMeterSubSubOptimum => false,
        }
    }

    fn match_pseudo_element(
        &self,
        pe: &PseudoElement,
        _context: &mut MatchingContext<Self::Impl>,
    ) -> bool {
        let pseudo = match self.stylo_element_data_opt().and_then(|s| s.get()) {
            Some(el) => el
                .styles
                .get_primary()
                .and_then(|s| s.pseudo())
                .or(match &self.data {
                    NodeData::AnonymousBlock(_) => Some(PseudoElement::ServoAnonymousBox),
                    _ => None,
                }),
            None => None,
        };

        pseudo.is_some_and(|psuedo| psuedo == *pe)
    }

    fn apply_selector_flags(&self, flags: ElementSelectorFlags) {
        // Handle flags that apply to the element.
        let self_flags = flags.for_self();
        if !self_flags.is_empty() {
            self.selector_flags()
                .set(self.selector_flags().get() | self_flags);
        }

        // Handle flags that apply to the parent.
        let parent_flags = flags.for_parent();
        if !parent_flags.is_empty() {
            if let Some(parent) = self.parent_node() {
                parent
                    .selector_flags()
                    .set(parent.selector_flags().get() | parent_flags);
            }
        }
    }

    fn is_link(&self) -> bool {
        self.data.is_element_with_tag_name(&local_name!("a"))
    }

    fn is_html_slot_element(&self) -> bool {
        self.is_slot_element()
    }

    fn assigned_slot(&self) -> Option<Self> {
        self.assigned_slot_id().and_then(|id| self.tree().get(id))
    }

    fn has_id(
        &self,
        id: &<Self::Impl as selectors::SelectorImpl>::Identifier,
        case_sensitivity: selectors::attr::CaseSensitivity,
    ) -> bool {
        self.element_data()
            .and_then(|data| data.id.as_ref())
            .map(|id_attr| case_sensitivity.eq_atom(id_attr, id))
            .unwrap_or(false)
    }

    fn has_class(
        &self,
        search_name: &<Self::Impl as selectors::SelectorImpl>::Identifier,
        case_sensitivity: selectors::attr::CaseSensitivity,
    ) -> bool {
        let class_attr = self.data.attr(local_name!("class"));
        if let Some(class_attr) = class_attr {
            // split the class attribute
            for pheme in class_attr.split_ascii_whitespace() {
                let atom = Atom::from(pheme);
                if case_sensitivity.eq_atom(&atom, search_name) {
                    return true;
                }
            }
        }

        false
    }

    // `name` is the outer name this host exports a part under; the result is the
    // part's name inside the host's shadow tree.
    fn imported_part(
        &self,
        name: &<Self::Impl as selectors::SelectorImpl>::Identifier,
    ) -> Option<<Self::Impl as selectors::SelectorImpl>::Identifier> {
        let value = self.data.attr(LocalName::from("exportparts"))?;
        exported_part_entries(value)
            .find(|(_, outer)| *outer == &*name.0)
            .map(|(inner, _)| GenericAtomIdent(Atom::from(inner)))
    }

    fn is_part(&self, name: &<Self::Impl as selectors::SelectorImpl>::Identifier) -> bool {
        self.data
            .attr(local_name!("part"))
            .is_some_and(|parts| parts.split_ascii_whitespace().any(|part| part == &*name.0))
    }

    fn is_empty(&self) -> bool {
        self.dom_children().next().is_none()
    }

    fn is_root(&self) -> bool {
        // The root element is the child of the document node; a child of a
        // shadow root is not, even though its parent has no parent either.
        self.parent_node()
            .is_none_or(|parent| matches!(parent.data, NodeData::Document(_)))
    }

    fn has_custom_state(
        &self,
        _name: &<Self::Impl as selectors::SelectorImpl>::Identifier,
    ) -> bool {
        false
    }

    fn add_element_unique_hashes(&self, filter: &mut selectors::bloom::BloomFilter) -> bool {
        each_relevant_element_hash(*self, |hash| filter.insert_hash(hash & BLOOM_HASH_MASK));
        true
    }
}

impl<'a> TElement for BlitzNode<'a> {
    type ConcreteNode = BlitzNode<'a>;

    type TraversalChildrenIterator = Traverser<'a>;

    fn as_node(&self) -> Self::ConcreteNode {
        self
    }

    fn implicit_scope_for_sheet_in_shadow_root(
        _opaque_host: OpaqueElement,
        _sheet_index: usize,
    ) -> Option<ImplicitScopeRoot> {
        // We cannot implement this as we are using the NodeId as the OpaqueElement,
        // and need a reference to the Slab to convert it back into an Element, so
        // `@scope` rules in shadow-tree stylesheets get no implicit scope root.
        None
    }

    fn traversal_children(&self) -> style::dom::LayoutIterator<Self::TraversalChildrenIterator> {
        let node: &'a Node = self;
        LayoutIterator(Traverser {
            children: node.flat_children(),
            parent: node,
            child_index: 0,
        })
    }

    // Inheritance follows the flat tree: shadow children inherit from the host
    // and slotted nodes from their slot.
    fn inheritance_parent(&self) -> Option<Self> {
        TElement::traversal_parent(self)
    }

    fn is_html_element(&self) -> bool {
        self.is_element()
    }

    // not implemented.....
    fn is_mathml_element(&self) -> bool {
        false
    }

    // need to check the namespace
    fn is_svg_element(&self) -> bool {
        false
    }

    fn style_attribute(&self) -> Option<ArcBorrow<'_, Locked<PropertyDeclarationBlock>>> {
        self.element_data()
            .expect("Not an element")
            .style_attribute
            .as_ref()
            .map(|f| f.borrow_arc())
    }

    fn state(&self) -> ElementState {
        *self.element_state()
    }

    fn has_part_attr(&self) -> bool {
        self.data.attr(local_name!("part")).is_some()
    }

    fn exports_any_part(&self) -> bool {
        self.data.attr(LocalName::from("exportparts")).is_some()
    }

    fn each_part<F>(&self, mut callback: F)
    where
        F: FnMut(&AtomIdent),
    {
        if let Some(parts) = self.data.attr(local_name!("part")) {
            for part in parts.split_ascii_whitespace() {
                callback(&GenericAtomIdent(Atom::from(part)));
            }
        }
    }

    // Calls back with every outer name this host exports the part `name` under.
    fn each_exported_part<F>(&self, name: &AtomIdent, mut callback: F)
    where
        F: FnMut(&AtomIdent),
    {
        let Some(value) = self.data.attr(LocalName::from("exportparts")) else {
            return;
        };
        for (_, outer) in exported_part_entries(value).filter(|(inner, _)| *inner == &*name.0) {
            callback(&GenericAtomIdent(Atom::from(outer)));
        }
    }

    fn id(&self) -> Option<&style::Atom> {
        self.element_data().and_then(|data| data.id.as_ref())
    }

    fn each_class<F>(&self, mut callback: F)
    where
        F: FnMut(&style::values::AtomIdent),
    {
        let class_attr = self.data.attr(local_name!("class"));
        if let Some(class_attr) = class_attr {
            // split the class attribute
            for pheme in class_attr.split_ascii_whitespace() {
                let atom = Atom::from(pheme); // interns the string
                callback(AtomIdent::cast(&atom));
            }
        }
    }

    fn each_attr_name<F>(&self, mut callback: F)
    where
        F: FnMut(&style::LocalName),
    {
        if let Some(attrs) = self.data.attrs() {
            for attr in attrs.iter() {
                callback(&GenericAtomIdent(attr.name.local.clone()));
            }
        }
    }

    fn has_dirty_descendants(&self) -> bool {
        Node::has_dirty_descendants(self)
    }

    fn has_snapshot(&self) -> bool {
        Node::has_snapshot(self)
    }

    fn handled_snapshot(&self) -> bool {
        self.snapshot_handled().load(Ordering::SeqCst)
    }

    unsafe fn set_handled_snapshot(&self) {
        self.snapshot_handled().store(true, Ordering::SeqCst);
    }

    unsafe fn set_dirty_descendants(&self) {
        Node::set_dirty_descendants(self);
        Node::mark_ancestors_dirty(self);
    }

    unsafe fn unset_dirty_descendants(&self) {
        Node::unset_dirty_descendants(self);
    }

    fn store_children_to_process(&self, _n: isize) {
        unimplemented!()
    }

    fn did_process_child(&self) -> isize {
        unimplemented!()
    }

    unsafe fn ensure_data(&self) -> ElementDataMut<'_> {
        // SAFETY: stylo traversal has exclusive access to nodes
        unsafe { self.stylo_element_data().ensure_init() }
    }

    unsafe fn clear_data(&self) {
        // SAFETY: stylo traversal has exclusive access to nodes
        unsafe { self.stylo_element_data().clear() }
    }

    fn has_data(&self) -> bool {
        self.stylo_element_data_opt().is_some_and(|s| s.has_data())
    }

    fn borrow_data(&self) -> Option<ElementDataRef<'_>> {
        self.stylo_element_data_opt().and_then(|s| s.get())
    }

    fn mutate_data(&self) -> Option<ElementDataMut<'_>> {
        unsafe { self.stylo_element_data().unsafe_stylo_only_mut() }
    }

    fn skip_item_display_fixup(&self) -> bool {
        false
    }

    fn may_have_animations(&self) -> bool {
        true
    }

    fn has_animations(&self, context: &SharedStyleContext) -> bool {
        self.has_css_animations(context, None) || self.has_css_transitions(context, None)
    }

    fn has_css_animations(
        &self,
        context: &SharedStyleContext,
        pseudo_element: Option<PseudoElement>,
    ) -> bool {
        let key = AnimationSetKey::new(TNode::opaque(&TElement::as_node(self)), pseudo_element);
        context.animations.has_active_animations(&key)
    }

    fn has_css_transitions(
        &self,
        context: &SharedStyleContext,
        pseudo_element: Option<PseudoElement>,
    ) -> bool {
        let key = AnimationSetKey::new(TNode::opaque(&TElement::as_node(self)), pseudo_element);
        context.animations.has_active_transitions(&key)
    }

    fn animation_rule(
        &self,
        context: &SharedStyleContext,
    ) -> Option<Arc<Locked<PropertyDeclarationBlock>>> {
        let opaque = TNode::opaque(&TElement::as_node(self));
        context.animations.get_animation_declarations(
            &AnimationSetKey::new_for_non_pseudo(opaque),
            context.current_time_for_animations,
            self.guard(),
        )
    }

    fn transition_rule(
        &self,
        context: &SharedStyleContext,
    ) -> Option<Arc<Locked<PropertyDeclarationBlock>>> {
        let opaque = TNode::opaque(&TElement::as_node(self));
        context.animations.get_transition_declarations(
            &AnimationSetKey::new_for_non_pseudo(opaque),
            context.current_time_for_animations,
            self.guard(),
        )
    }

    fn shadow_root(&self) -> Option<<Self::ConcreteNode as TNode>::ConcreteShadowRoot> {
        self.shadow_root_id().and_then(|id| self.tree().get(id))
    }

    fn containing_shadow(&self) -> Option<<Self::ConcreteNode as TNode>::ConcreteShadowRoot> {
        self.containing_shadow_root_id()
            .and_then(|id| self.tree().get(id))
    }

    fn get_attr(&self, attr: &style::LocalName, _ns: &style::Namespace) -> Option<String> {
        // TODO: filter by namespace
        // TODO: case-insensitive matching for HTML-ns attrs
        self.attr(attr.0.clone()).map(|s| s.to_string())
    }

    fn lang_attr(&self) -> Option<style::selector_parser::AttrValue> {
        None
    }

    fn match_element_lang(
        &self,
        _override_lang: Option<Option<style::selector_parser::AttrValue>>,
        _value: &style::selector_parser::Lang,
    ) -> bool {
        false
    }

    fn is_html_document_body_element(&self) -> bool {
        // Check node is a <body> element
        let is_body_element = self.data.is_element_with_tag_name(&local_name!("body"));

        // If it isn't then return early
        if !is_body_element {
            return false;
        }

        // If it is then check if it is a child of the root (<html>) element
        let root_node = TNode::owner_doc(self);
        let root_element = TDocument::as_node(&root_node)
            .first_element_child()
            .unwrap();
        root_element.children.contains(&self.id)
    }

    fn synthesize_presentational_hints_for_legacy_attributes<V>(
        &self,
        _visited_handling: VisitedHandlingMode,
        hints: &mut V,
    ) where
        V: Push<style::applicable_declarations::ApplicableDeclarationBlock>,
    {
        let Some(elem) = self.data.downcast_element() else {
            return;
        };

        let tag = &elem.name.local;

        let mut push_style = |decl: PropertyDeclaration| {
            hints.push(ApplicableDeclarationBlock::from_declarations(
                Arc::new(
                    self.guard()
                        .wrap(PropertyDeclarationBlock::with_one(decl, Importance::Normal)),
                ),
                CascadeLevel::new(CascadeOrigin::PresHints),
                LayerOrder::root(),
            ));
        };

        fn parse_color_attr(value: &str) -> Option<(u8, u8, u8, f32)> {
            if !value.starts_with('#') {
                return None;
            }

            let value = &value[1..];
            if value.len() == 3 {
                let r = u8::from_str_radix(&value[0..1], 16).ok()?;
                let g = u8::from_str_radix(&value[1..2], 16).ok()?;
                let b = u8::from_str_radix(&value[2..3], 16).ok()?;
                return Some((r, g, b, 1.0));
            }

            if value.len() == 6 {
                let r = u8::from_str_radix(&value[0..2], 16).ok()?;
                let g = u8::from_str_radix(&value[2..4], 16).ok()?;
                let b = u8::from_str_radix(&value[4..6], 16).ok()?;
                return Some((r, g, b, 1.0));
            }

            None
        }

        /// The HTML "rules for parsing dimension values" -- Stylo's
        /// implementation of them -- packaged as a specified
        /// `<length-percentage>`. `ignoring_zero` selects the spec's separate
        /// "maps to the dimension property (ignoring zero)" mapping, where a
        /// zero value is dropped rather than honoured.
        ///
        /// https://html.spec.whatwg.org/multipage/common-microsyntaxes.html#rules-for-parsing-dimension-values
        fn parse_size_attr(
            value: &str,
            ignoring_zero: bool,
        ) -> Option<style::values::specified::LengthPercentage> {
            use style::servo::attr::{
                LengthOrPercentageOrAuto, parse_length, parse_nonzero_length,
            };
            use style::values::specified::{LengthPercentage, NoCalcLength};
            let parsed = if ignoring_zero {
                parse_nonzero_length(value)
            } else {
                parse_length(value)
            };
            match parsed {
                LengthOrPercentageOrAuto::Length(length) => Some(LengthPercentage::Length(
                    NoCalcLength::from_px(length.to_f32_px()),
                )),
                LengthOrPercentageOrAuto::Percentage(fraction) => Some(
                    LengthPercentage::Percentage(NoCalcPercentage::new(fraction)),
                ),
                LengthOrPercentageOrAuto::Auto => None,
            }
        }

        /// Parse the value of an SVG `width`/`height` presentation attribute.
        /// Unlike the legacy HTML dimension attributes, these accept any CSS
        /// <length-percentage> (e.g. `1em`), and a unitless number means user
        /// units, which map to CSS px.
        fn parse_svg_size_attr(value: &str) -> Option<style::values::specified::LengthPercentage> {
            use style::values::specified::{LengthPercentage, NoCalcLength};
            use style_traits::ParsingMode;

            let value = value.trim();
            if let Some(number) = value.strip_suffix('%') {
                let val: f32 = number.trim().parse().ok()?;
                return (val >= 0.0)
                    .then(|| LengthPercentage::Percentage(NoCalcPercentage::new(val / 100.0)));
            }

            // Split into number and unit: the unit is the trailing run of
            // ASCII alphabetic characters (this never eats into a scientific
            // exponent such as `1e3`, which ends in a digit).
            let number_len = value
                .trim_end_matches(|c: char| c.is_ascii_alphabetic())
                .len();
            let (number, unit) = value.split_at(number_len);
            let val: f32 = number.trim().parse().ok().filter(|v| *v >= 0.0)?;
            let length = if unit.is_empty() {
                NoCalcLength::from_px(val)
            } else {
                NoCalcLength::parse_dimension_with_flags(ParsingMode::DEFAULT, false, val, unit)
                    .ok()?
            };
            Some(LengthPercentage::Length(length))
        }

        // `<input type=image>` is the only input type that is replaced
        // content, and it is the only one that takes the embedded-content
        // presentational attributes. The type attribute is matched ASCII
        // case-insensitively, as attribute keywords always are.
        let is_image_input = *tag == local_name!("input")
            && elem.attrs().iter().any(|attr| {
                attr.name.local == local_name!("type") && attr.value.eq_ignore_ascii_case("image")
            });

        for attr in elem.attrs() {
            let name = &attr.name.local;
            let value = attr.value.as_str();

            if *name == local_name!("align") {
                use style::values::specified::TextAlign;
                let keyword = match value {
                    "left" => Some(StyloTextAlign::MozLeft),
                    "right" => Some(StyloTextAlign::MozRight),
                    "center" => Some(StyloTextAlign::MozCenter),
                    _ => None,
                };

                if let Some(keyword) = keyword {
                    push_style(PropertyDeclaration::TextAlign(TextAlign::Keyword(keyword)));
                }
            }

            // The width/height attributes on these elements map to the
            // corresponding dimension properties (percentages allowed):
            // https://html.spec.whatwg.org/multipage/rendering.html#dimRendering
            // https://html.spec.whatwg.org/multipage/rendering.html#attributes-for-embedded-content-and-images
            let is_width = *name == local_name!("width");
            let is_height = *name == local_name!("height");
            // The elements whose width/height attributes map to the dimension
            // properties. `input` only joins them as type=image, which is the
            // one replaced input type.
            let is_embedded = *tag == local_name!("iframe")
                || *tag == local_name!("embed")
                || *tag == local_name!("video")
                || *tag == local_name!("object")
                || *tag == local_name!("img")
                || *tag == local_name!("marquee")
                || is_image_input;
            let maps_to_dimension = if is_width {
                is_embedded
                    || *tag == local_name!("table")
                    || *tag == local_name!("col")
                    || *tag == local_name!("colgroup")
                    || *tag == local_name!("tr")
                    || *tag == local_name!("td")
                    || *tag == local_name!("th")
                    || *tag == local_name!("hr")
            } else if is_height {
                is_embedded
                    || *tag == local_name!("table")
                    || *tag == local_name!("thead")
                    || *tag == local_name!("tbody")
                    || *tag == local_name!("tfoot")
                    || *tag == local_name!("tr")
                    || *tag == local_name!("td")
                    || *tag == local_name!("th")
            } else {
                false
            };
            if maps_to_dimension {
                // Three of these use the "(ignoring zero)" variant of the
                // mapping, where a zero is dropped instead of honoured:
                // `table width`, and `td`/`th` in both axes.
                let is_cell = *tag == local_name!("td") || *tag == local_name!("th");
                let ignoring_zero = is_cell || (is_width && *tag == local_name!("table"));
                if let Some(size) = parse_size_attr(value, ignoring_zero) {
                    use style::values::generics::{NonNegative, length::Size};
                    let size = Size::LengthPercentage(NonNegative(size));
                    push_style(if is_width {
                        PropertyDeclaration::Width(size)
                    } else {
                        PropertyDeclaration::Height(size)
                    });
                }
            }

            // hspace/vspace map to the horizontal and vertical margins as
            // dimension properties. This is a *smaller* set than the one that
            // takes width/height: `iframe` and `video` take width and height
            // but not hspace/vspace, and html/rendering/unmapped-attributes
            // checks exactly that -- browsers have got it wrong before.
            let takes_spacing = *tag == local_name!("embed")
                || *tag == local_name!("img")
                || *tag == local_name!("object")
                || *tag == local_name!("marquee")
                || is_image_input;
            if takes_spacing {
                let is_hspace = *name == local_name!("hspace");
                let is_vspace = *name == local_name!("vspace");
                if is_hspace || is_vspace {
                    if let Some(size) = parse_size_attr(value, false) {
                        use style::values::generics::length::GenericMargin;
                        let margin = GenericMargin::LengthPercentage(size);
                        if is_hspace {
                            push_style(PropertyDeclaration::MarginLeft(margin.clone()));
                            push_style(PropertyDeclaration::MarginRight(margin));
                        } else {
                            push_style(PropertyDeclaration::MarginTop(margin.clone()));
                            push_style(PropertyDeclaration::MarginBottom(margin));
                        }
                    }
                }
            }

            // https://svgwg.org/svg2-draft/geometry.html#Sizing
            // The `width` and `height` attributes on an `<svg>` element are
            // presentation attributes that map to the CSS `width`/`height`
            // properties, so e.g. `width="1em"` must resolve against the
            // element's font-size like any other CSS length.
            if *tag == local_name!("svg")
                && (*name == local_name!("width") || *name == local_name!("height"))
            {
                if let Some(size) = parse_svg_size_attr(value) {
                    use style::values::generics::{NonNegative, length::Size};
                    let size = Size::LengthPercentage(NonNegative(size));
                    push_style(if *name == local_name!("width") {
                        PropertyDeclaration::Width(size)
                    } else {
                        PropertyDeclaration::Height(size)
                    });
                }
            }

            // https://svgwg.org/svg2-draft/painting.html#SpecifyingPaint
            // The `color` attribute on SVG elements is a presentation attribute for the
            // CSS `color` property. Mapping it at the PresHints level keeps it below all
            // author CSS, and gives descendants the inherited value for `currentColor`.
            if *name == local_name!("color") && elem.name.ns == markup5ever::ns!(svg) {
                use style::parser::{Parse, ParserContext};
                use style::stylesheets::{CssRuleType, Origin};
                use style::values::specified::ColorPropertyValue;
                use style_traits::ParsingMode;

                let url_extra_data = crate::url::DocumentUrl::default().url_extra_data();
                let context = ParserContext::new(
                    Origin::Author,
                    &url_extra_data,
                    Some(CssRuleType::Style),
                    ParsingMode::DEFAULT,
                    QuirksMode::NoQuirks,
                    Default::default(),
                    None,
                    None,
                    Default::default(),
                );
                let mut input = cssparser::ParserInput::new(value);
                let mut parser = style::values::Parser::new(&mut input);
                if let Ok(color) = ColorPropertyValue::parse(&context, &mut parser)
                    && parser.expect_exhausted().is_ok()
                {
                    push_style(PropertyDeclaration::Color(color));
                }
            }

            // The `border` attribute maps to the four border widths as a
            // pixel length, plus the four border styles as `solid` -- width
            // alone would compute back to zero against the default
            // border-style of `none`. It is only these three elements:
            // `embed`, `iframe`, `marquee` and non-image `input` all have a
            // `border` attribute that must stay unmapped.
            if *name == local_name!("border")
                && (*tag == local_name!("img") || *tag == local_name!("object") || is_image_input)
            {
                if let Ok(px) = style::servo::attr::parse_unsigned_integer(value.chars()) {
                    use style::values::specified::{BorderSideWidth, BorderStyle};
                    let width = BorderSideWidth::from_px(px as f32);
                    push_style(PropertyDeclaration::BorderTopWidth(width.clone()));
                    push_style(PropertyDeclaration::BorderRightWidth(width.clone()));
                    push_style(PropertyDeclaration::BorderBottomWidth(width.clone()));
                    push_style(PropertyDeclaration::BorderLeftWidth(width));
                    push_style(PropertyDeclaration::BorderTopStyle(BorderStyle::Solid));
                    push_style(PropertyDeclaration::BorderRightStyle(BorderStyle::Solid));
                    push_style(PropertyDeclaration::BorderBottomStyle(BorderStyle::Solid));
                    push_style(PropertyDeclaration::BorderLeftStyle(BorderStyle::Solid));
                }
            }

            // `body` carries four legacy margin attributes, as pixel lengths:
            // marginwidth and marginheight set both sides of an axis, and
            // leftmargin and topmargin set one side each.
            //
            // There is deliberately no `rightmargin` or `bottommargin`. They
            // look like the obvious counterparts to the two that exist, but
            // the spec does not define them and browsers ignore them in both
            // standards and quirks mode -- body-margin-3a/3b assert exactly
            // that.
            if *tag == local_name!("body") {
                // Matched as strings: these are not in the static atom set,
                // so `local_name!` will not compile for them.
                let sides: &[u8] = match &**name {
                    "marginwidth" => b"lr",
                    "marginheight" => b"tb",
                    "leftmargin" => b"l",
                    "topmargin" => b"t",
                    _ => b"",
                };
                if !sides.is_empty() {
                    if let Ok(px) = style::servo::attr::parse_unsigned_integer(value.chars()) {
                        use style::values::generics::length::GenericMargin;
                        use style::values::specified::{LengthPercentage, NoCalcLength};
                        let margin = GenericMargin::LengthPercentage(LengthPercentage::Length(
                            NoCalcLength::from_px(px as f32),
                        ));
                        for side in sides {
                            push_style(match side {
                                b'l' => PropertyDeclaration::MarginLeft(margin.clone()),
                                b'r' => PropertyDeclaration::MarginRight(margin.clone()),
                                b't' => PropertyDeclaration::MarginTop(margin.clone()),
                                b'b' => PropertyDeclaration::MarginBottom(margin.clone()),
                                _ => unreachable!("side table above only yields lrtb"),
                            });
                        }
                    }
                }
            }

            if *name == local_name!("bgcolor") {
                use style::values::specified::Color;
                if let Some((r, g, b, a)) = parse_color_attr(value) {
                    push_style(PropertyDeclaration::BackgroundColor(
                        Color::from_absolute_color(AbsoluteColor::srgb_legacy(r, g, b, a)),
                    ));
                }
            }

            if *name == local_name!("hidden") {
                use style::values::specified::Display;
                push_style(PropertyDeclaration::Display(Display::None));
            }
        }
    }

    fn local_name(&self) -> &LocalName {
        &self.element_data().expect("Not an element").name.local
    }

    fn namespace(&self) -> &Namespace {
        &self.element_data().expect("Not an element").name.ns
    }

    fn query_container_size(
        &self,
        _display: &style::values::specified::Display,
    ) -> euclid::default::Size2D<Option<app_units::Au>> {
        // FIXME: Implement container queries. For now this effectively disables them without panicking.
        Default::default()
    }

    fn each_custom_state<F>(&self, _callback: F)
    where
        F: FnMut(&AtomIdent),
    {
        todo!()
    }

    fn has_selector_flags(&self, flags: ElementSelectorFlags) -> bool {
        self.selector_flags().get().contains(flags)
    }

    fn relative_selector_search_direction(&self) -> ElementSelectorFlags {
        let flags = self.selector_flags().get();
        if flags.contains(ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING)
        {
            ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR_SIBLING
        } else if flags.contains(ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR)
        {
            ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_ANCESTOR
        } else if flags.contains(ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_SIBLING) {
            ElementSelectorFlags::RELATIVE_SELECTOR_SEARCH_DIRECTION_SIBLING
        } else {
            ElementSelectorFlags::empty()
        }
    }

    fn compute_layout_damage(old: &ComputedValues, new: &ComputedValues) -> RestyleDamage {
        compute_layout_damage(old, new)
        // ALL_DAMAGE
    }

    // fn update_animations(
    //     &self,
    //     before_change_style: Option<Arc<ComputedValues>>,
    //     tasks: style::context::UpdateAnimationsTasks,
    // ) {
    //     todo!()
    // }

    // fn process_post_animation(&self, tasks: style::context::PostAnimationTasks) {
    //     todo!()
    // }

    // fn needs_transitions_update(
    //     &self,
    //     before_change_style: &ComputedValues,
    //     after_change_style: &ComputedValues,
    // ) -> bool {
    //     todo!()
    // }
}

/// Iterates the children of a node in the flat tree.
pub struct Traverser<'a> {
    parent: BlitzNode<'a>,
    children: std::borrow::Cow<'a, [NodeId]>,
    child_index: usize,
}

impl<'a> Iterator for Traverser<'a> {
    type Item = BlitzNode<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let node_id = self.children.get(self.child_index)?;
        let node = self.parent.with(*node_id);

        self.child_index += 1;

        Some(node)
    }
}

impl std::hash::Hash for BlitzNode<'_> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        state.write_u64(self.id.as_u64())
    }
}

/// Handle custom painters like images for layouting
///
/// todo: actually implement this
pub struct RegisteredPaintersImpl;
impl RegisteredSpeculativePainters for RegisteredPaintersImpl {
    fn get(&self, _name: &Atom) -> Option<&dyn RegisteredSpeculativePainter> {
        None
    }
}

use style::traversal::recalc_style_at;

pub struct RecalcStyle<'a> {
    context: SharedStyleContext<'a>,
    /// Nodes whose `background-image`/`mask-image` layers need flushing to
    /// dedicated storage on the node (see `flush_image_layers_from_style`)
    /// because their style changed during this traversal.
    nodes_needing_style_image_flush: Mutex<Vec<NodeId>>,
    /// Inline `<svg>` elements whose computed `color` changed during this traversal.
    svg_nodes_to_reparse: Mutex<Vec<NodeId>>,
}

impl<'a> RecalcStyle<'a> {
    pub fn new(context: SharedStyleContext<'a>) -> Self {
        RecalcStyle {
            context,
            nodes_needing_style_image_flush: Mutex::new(Vec::new()),
            svg_nodes_to_reparse: Mutex::new(Vec::new()),
        }
    }
}

#[allow(unsafe_code)]
impl<'dom> DomTraversal<BlitzNode<'dom>> for RecalcStyle<'_> {
    fn process_preorder<F: FnMut(BlitzNode<'dom>)>(
        &self,
        traversal_data: &PerLevelTraversalData,
        context: &mut StyleContext<BlitzNode<'dom>>,
        node: BlitzNode<'dom>,
        note_child: F,
    ) {
        if let Some(el) = node.as_element() {
            // let mut data = el.mutate_data().unwrap();
            let mut data = unsafe { el.ensure_data() };
            #[cfg(feature = "svg")]
            let in_svg = is_svg_element(el);
            #[cfg(not(feature = "svg"))]
            let in_svg = false;
            // Only elements whose anonymous blocks or inline SVG depend on the primary style
            // need the pre-restyle style to detect a change.
            let old_primary = (in_svg || !el.anonymous_blocks.is_empty())
                .then(|| data.styles.get_primary().cloned())
                .flatten();
            recalc_style_at(self, traversal_data, context, el, &mut data, note_child);

            sync_pseudo_element_styles(el, &data, &self.nodes_needing_style_image_flush);

            if let (Some(old), Some(new)) = (old_primary, data.styles.get_primary().cloned())
                && !std::ptr::eq(&*old, &*new)
            {
                sync_anonymous_block_styles(el, &new, context.shared);
                // Inline SVG is rasterised from a tree built with every element's own
                // computed `color` baked in (for `currentColor`), so a colour change on
                // the root or any descendant needs a re-parse of the whole tree. The
                // box tree is unaffected, so this avoids construction damage.
                if in_svg && old.clone_color() != new.clone_color() {
                    let root = outermost_svg_ancestor(el);
                    let mut queued = self.svg_nodes_to_reparse.lock().unwrap();
                    if !queued.contains(&root) {
                        queued.push(root);
                    }
                }
            }

            if !data.damage.is_empty() {
                // Mark the ancestor chain so that the damage propagation pass
                // visits this element.
                el.mark_damaged();

                if data
                    .styles
                    .get_primary()
                    .is_some_and(|style| needs_style_image_flush(el, style))
                {
                    self.nodes_needing_style_image_flush
                        .lock()
                        .unwrap()
                        .push(el.id);
                }
            }

            // Gets set later on
            el.unset_dirty_descendants();
        }
    }

    #[inline]
    fn needs_postorder_traversal() -> bool {
        false
    }

    fn process_postorder(
        &self,
        _style_context: &mut StyleContext<BlitzNode<'dom>>,
        _node: BlitzNode<'dom>,
    ) {
        panic!("this should never be called")
    }

    #[inline]
    fn shared_context(&self) -> &SharedStyleContext<'_> {
        &self.context
    }
}

/// Flush updated pseudo-element (`::before`/`::after`) styles from the owning
/// element's freshly-computed stylo data to the pseudo-element's anonymous node.
///
/// Pseudo-element styles are normally flushed to the pseudo-element's node
/// during box construction (see `flush_pseudo_elements`), but in incremental
/// mode box construction only runs for nodes with construction damage.
/// Pseudo-element style changes which don't require reconstruction (e.g.
/// animations/transitions of repaint- or relayout-only properties) must still
/// be flushed to the pseudo-element's node - along with the damage they imply -
/// so that layout and paint see the new style.
///
/// This runs during the style traversal, immediately after the owning element
/// has been restyled, because that is where the old and new pseudo styles can
/// be diffed precisely (the pseudo node's stored primary style is the "old"
/// style; the owner's just-computed pseudo styles are the "new" ones).
///
/// SAFETY: the style traversal has exclusive access to the tree, and each
/// pseudo-element node is only ever accessed from its owning element's
/// `process_preorder` (pseudo nodes are not part of the DOM child list and are
/// not themselves visited by the traversal), so mutating the pseudo node's
/// stylo data here cannot race with any other traversal thread.
#[allow(unsafe_code)]
fn sync_pseudo_element_styles(
    el: &Node,
    data: &style::data::ElementData,
    nodes_needing_style_image_flush: &Mutex<Vec<NodeId>>,
) {
    let before_node_id = el.before();
    let after_node_id = el.after();
    if before_node_id.is_none() && after_node_id.is_none() {
        return;
    }

    // Note: yes these are kinda backwards (see `flush_pseudo_elements`)
    let pseudos = data.styles.pseudos.as_array();
    let before_style = pseudos[1].clone();
    let after_style = pseudos[0].clone();

    // Creation and removal of pseudo-elements is handled during box construction
    // (Stylo generates construction damage for those cases), so only the case
    // where the pseudo-element both was and remains present is handled here.
    for (pe_node_id, pe_style) in [(before_node_id, before_style), (after_node_id, after_style)] {
        let (Some(pe_node_id), Some(pe_style)) = (pe_node_id, pe_style) else {
            continue;
        };
        let pe_node = el.with(pe_node_id);
        let Some(stylo_data) = pe_node.stylo_element_data_opt() else {
            continue;
        };
        let mut pe_data = match unsafe { stylo_data.unsafe_stylo_only_mut() } {
            Some(data) => data,
            None => continue,
        };
        let Some(old_style) = pe_data.styles.primary.clone() else {
            continue;
        };
        if std::ptr::eq(&*old_style, &*pe_style) {
            continue;
        }

        let diff = RestyleDamage::compute_style_difference::<&Node>(&old_style, &pe_style);
        if !diff.damage.is_empty() {
            pe_data.damage.insert(diff.damage);
            pe_node.mark_damaged();

            if needs_style_image_flush(pe_node, &pe_style) {
                nodes_needing_style_image_flush
                    .lock()
                    .unwrap()
                    .push(pe_node_id);
            }
        }
        pe_data.styles.primary = Some(pe_style);
        pe_data.set_restyled();
    }
}

/// The outermost SVG-namespace ancestor (or self) of an element: the inline `<svg>` root,
/// which is what box construction rasterises.
#[cfg(feature = "svg")]
fn outermost_svg_ancestor(el: &Node) -> NodeId {
    let mut root = el;
    while let Some(parent) = root.parent.map(|id| el.with(id)) {
        if !is_svg_element(parent) {
            break;
        }
        root = parent;
    }
    root.id
}

#[cfg(feature = "svg")]
fn is_svg_element(node: &Node) -> bool {
    node.data
        .downcast_element()
        .is_some_and(|e| e.name.ns == markup5ever::ns!(svg))
}

#[cfg(not(feature = "svg"))]
fn outermost_svg_ancestor(el: &Node) -> NodeId {
    el.id
}

/// Recompute the styles of an element's anonymous blocks from its freshly-computed
/// primary style.
///
/// Anonymous boxes inherit every inherited property from their parent and are
/// otherwise initial (CSS 2.1 section 9.2.1.1), so they must follow the parent's
/// style. Box construction derives their style once, and a restyle that needs no
/// reconstruction (e.g. a `color` transition tick) would leave them stale: text
/// inside takes its glyph colour from the anonymous block.
///
/// SAFETY: as with `sync_pseudo_element_styles`, anonymous blocks are not part of
/// the DOM child list, so they are only reached from their owner's
/// `process_preorder` and no other traversal thread can touch their stylo data.
#[allow(unsafe_code)]
fn sync_anonymous_block_styles(
    el: &Node,
    parent_style: &style::servo_arc::Arc<ComputedValues>,
    shared: &SharedStyleContext<'_>,
) {
    for &anon_id in &el.anonymous_blocks {
        let anon_node = el.with(anon_id);
        let Some(stylo_data) = anon_node.stylo_element_data_opt() else {
            continue;
        };
        let Some(mut anon_data) = (unsafe { stylo_data.unsafe_stylo_only_mut() }) else {
            continue;
        };
        let Some(old_style) = anon_data.styles.primary.clone() else {
            continue;
        };

        let new_style = shared.stylist.style_for_anonymous::<&Node>(
            &shared.guards,
            &PseudoElement::ServoAnonymousBox,
            parent_style,
        );
        let diff = RestyleDamage::compute_style_difference::<&Node>(&old_style, &new_style);
        if !diff.damage.is_empty() {
            anon_data.damage.insert(diff.damage);
            anon_node.mark_damaged();
        }
        anon_data.styles.primary = Some(new_style.clone());
        anon_data.set_restyled();
        drop(anon_data);

        sync_anonymous_block_styles(anon_node, &new_style, shared);
    }
}

/// Whether a node's `background-image`/`mask-image` layers need flushing to
/// dedicated storage on the node: the URLs referenced by the new style differ
/// from the image data currently stored on the node.
fn needs_style_image_flush(node: &Node, style: &ComputedValues) -> bool {
    use crate::node::ImageResourceData;
    use style::url::ComputedUrl;
    use style::values::computed::image::Image;

    fn out_of_sync(style_images: &[Image], stored: &[Option<ImageResourceData>]) -> bool {
        if style_images.len() != stored.len() {
            // Differing lengths only matter if either side references an image
            // (a style list of `none` layers and empty storage are in sync).
            return style_images
                .iter()
                .any(|image| matches!(image, Image::Url(_)))
                || stored.iter().any(Option::is_some);
        }
        std::iter::zip(style_images, stored).any(|(style_image, stored)| {
            match (style_image, stored) {
                (Image::Url(ComputedUrl::Valid(url)), Some(data)) => **url != *data.url,
                (Image::Url(ComputedUrl::Valid(_)), None) => true,
                (_, Some(_)) => true,
                (_, None) => false,
            }
        })
    }

    node.data.downcast_element().is_some_and(|el| {
        out_of_sync(
            &style.get_background().background_image.0,
            &el.background_images,
        ) || out_of_sync(&style.get_svg().mask_image.0, &el.mask_images)
    })
}

#[test]
fn assert_size_of_equals() {
    // use std::mem;

    // fn assert_layout<E>() {
    //     assert_eq!(
    //         mem::size_of::<SharingCache<E>>(),
    //         mem::size_of::<TypelessSharingCache>()
    //     );
    //     assert_eq!(
    //         mem::align_of::<SharingCache<E>>(),
    //         mem::align_of::<TypelessSharingCache>()
    //     );
    // }

    // let size = mem::size_of::<StyleSharingCandidate<BlitzNode>>();
    // dbg!(size);
}

#[test]
fn parse_inline() {
    // let attrs = style::attr::AttrValue::from_serialized_tokenlist(
    //     r#"visibility: hidden; left: 1306.5px; top: 50px; display: none;"#.to_string(),
    // );

    // let val = CSSInlineStyleDeclaration();
}
