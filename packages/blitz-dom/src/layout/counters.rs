//! CSS counters (css-lists-3 §4): a tree-order pass that gives every counter its
//! value, so `::before`/`::after` `content` can show `counter()`/`counters()` and
//! list markers can show the `list-item` counter.
//!
//! Each element, then its `::before`, children and `::after`, applies
//! `counter-reset`, then `counter-increment` (plus the implicit `list-item`
//! increment of list items, §4.6), then `counter-set`. A counter created by
//! `counter-reset` is in scope for its element, the element's descendants and its
//! following siblings with theirs (§4.4).
//!
//! The pass runs between styling and box construction, and also renders the text of
//! each counter in the counter styles the document's `@counter-style` rules define.
//! Where a pseudo-element's text or a list item's marker changed, it damages the
//! element so construction rebuilds that text.

use std::collections::HashMap;

use blitz_traits::node_id::NodeId;
use style::Atom;
use style::counter_style::{CounterStyle, CounterStyleRule};
use style::properties::ComputedValues;
use style::servo_arc::Arc;
use style::shared_lock::ToCssWithGuard;
use style::values::computed::{Content, ContentItem};

use super::counter_style::CounterStyles;
use super::damage::ALL_DAMAGE;
use crate::BaseDocument;
use crate::node::Marker;

/// The pseudo-element slots of `ElementStyles::pseudos`, as `flush_pseudo_elements`
/// indexes them.
pub(crate) const AFTER: usize = 0;
pub(crate) const BEFORE: usize = 1;

/// What the last counters pass resolved.
#[derive(Default, PartialEq)]
pub(crate) struct CounterValues {
    /// The text of each `::before`/`::after` with `content` items, by originating
    /// element and pseudo slot. `None` where no item generates text.
    pseudo_text: HashMap<(NodeId, usize), Option<String>>,
    /// The marker of each list item: its `list-item` counter in its
    /// `list-style-type`, `None` for `none`.
    list_markers: HashMap<NodeId, Option<Marker>>,
    /// The CSS of the document's effective `@counter-style` rules, so the pass runs
    /// again when one changes while no element is damaged.
    counter_style_rules: Vec<String>,
}

impl CounterValues {
    /// The generated text of `node_id`'s pseudo-element in `slot`.
    pub(crate) fn pseudo_text(&self, node_id: NodeId, slot: usize) -> Option<&str> {
        self.pseudo_text
            .get(&(node_id, slot))
            .and_then(|text| text.as_deref())
    }

    /// The marker of the list item `node_id`.
    pub(crate) fn list_marker(&self, node_id: NodeId) -> Option<&Marker> {
        self.list_markers.get(&node_id)?.as_ref()
    }
}

struct Counter {
    value: i32,
    is_reversed: bool,
    /// Index into `Walk::reversed` of a reversed counter whose initial value is
    /// computed from its scope.
    tally: Option<usize>,
}

/// The increments and sets one element applies to a reversed counter with no
/// initial value, in tree order.
#[derive(Default)]
struct Tally {
    steps: Vec<Step>,
}

struct Step {
    element: usize,
    increment: i32,
    set: Option<i32>,
}

impl Tally {
    fn record(&mut self, element: usize, increment: i32, set: Option<i32>) {
        if let Some(step) = self.steps.last_mut().filter(|step| step.element == element) {
            step.increment = step.increment.saturating_add(increment);
            step.set = set.or(step.set);
            return;
        }
        self.steps.push(Step {
            element,
            increment,
            set,
        });
    }

    /// The initial value of a reversed counter (css-lists-3 §4.4.2).
    fn initial(&self) -> i32 {
        let mut num = 0i32;
        let mut first = true;
        for step in &self.steps {
            let negated = step.increment.saturating_neg();
            if first {
                num = num.saturating_add(negated);
                first = false;
            }
            if let Some(set) = step.set {
                return num.saturating_add(set);
            }
            num = num.saturating_add(negated);
        }
        num
    }
}

struct Walk<'a> {
    doc: &'a BaseDocument,
    styles: &'a CounterStyles<'a>,
    /// The counters in scope, innermost last, by name.
    counters: HashMap<Atom, Vec<Counter>>,
    /// Per open element, the names its children (and pseudo-elements) instantiated.
    scopes: Vec<Vec<Atom>>,
    /// Reversed counters with no initial value, in creation order.
    reversed: Vec<Tally>,
    /// Their initial values, from a previous pass that tallied them.
    reversed_initials: Option<Vec<i32>>,
    /// The tree-order index of the element or pseudo-element being applied.
    element: usize,
    /// Interned once per pass rather than per element.
    list_item: Atom,
    out: CounterValues,
}

impl<'a> Walk<'a> {
    fn new(
        doc: &'a BaseDocument,
        styles: &'a CounterStyles<'a>,
        reversed_initials: Option<Vec<i32>>,
    ) -> Self {
        Self {
            doc,
            styles,
            counters: HashMap::new(),
            scopes: vec![Vec::new()],
            reversed: Vec::new(),
            reversed_initials,
            element: 0,
            list_item: Atom::from("list-item"),
            out: CounterValues::default(),
        }
    }

    fn visit_children(&mut self, node_id: NodeId) {
        let doc = self.doc;
        for &child_id in doc.nodes[node_id].flat_children().iter() {
            if doc.nodes[child_id].is_element() {
                self.visit_element(child_id);
            }
        }
    }

    fn visit_element(&mut self, node_id: NodeId) {
        let node = &self.doc.nodes[node_id];
        // An element with no box sets no counters, nor do its descendants.
        let Some(style) = node.primary_styles().map(|style| Arc::clone(&style)) else {
            return;
        };
        if style.get_box().display.is_none() {
            return;
        }
        let pseudos = node
            .stylo_element_data_opt()
            .and_then(|data| data.get())
            .map(|data| {
                let pseudos = data.styles.pseudos.as_array();
                [pseudos[AFTER].clone(), pseudos[BEFORE].clone()]
            })
            .unwrap_or_default();

        self.apply(&style);
        if style.get_box().display.is_list_item() {
            let list_item = self.list_item.clone();
            let value = self.innermost(&list_item).value;
            let marker = self.styles.marker(&style.clone_list_style_type().0, value);
            self.out.list_markers.insert(node_id, marker);
        }

        self.scopes.push(Vec::new());
        if let Some(before) = &pseudos[BEFORE] {
            self.visit_pseudo(node_id, BEFORE, before);
        }
        self.visit_children(node_id);
        if let Some(after) = &pseudos[AFTER] {
            self.visit_pseudo(node_id, AFTER, after);
        }
        for name in self.scopes.pop().expect("pushed above") {
            self.counters.get_mut(&name).and_then(Vec::pop);
        }
    }

    /// A pseudo-element counts as a box (and so sets counters) only with `content`
    /// items, as `flush_pseudo_elements` builds it.
    fn visit_pseudo(&mut self, node_id: NodeId, slot: usize, style: &ComputedValues) {
        let Content::Items(items) = &style.get_counters().content else {
            return;
        };
        if style.get_box().display.is_none() {
            return;
        }
        self.apply(style);

        let mut text = String::new();
        let mut generates_text = false;
        for item in &items.items[..items.alt_start] {
            match item {
                ContentItem::String(string) => {
                    text.push_str(string);
                    generates_text = true;
                }
                ContentItem::Counter(name, counter_style) => {
                    text.push_str(&self.format(&name.0, counter_style, None));
                    generates_text = true;
                }
                ContentItem::Counters(name, separator, counter_style) => {
                    text.push_str(&self.format(&name.0, counter_style, Some(&**separator)));
                    generates_text = true;
                }
                // Quotes, attr() and images generate no counter text, and box
                // construction does not render them yet.
                ContentItem::OpenQuote
                | ContentItem::CloseQuote
                | ContentItem::NoOpenQuote
                | ContentItem::NoCloseQuote
                | ContentItem::Attr(_)
                | ContentItem::Image(_) => {}
            }
        }
        self.out
            .pseudo_text
            .insert((node_id, slot), generates_text.then_some(text));
    }

    /// `counter()` (no separator) or `counters()` of `name`, which the current element
    /// instantiates at 0 if no counter of that name is in scope (css-lists-3 §4.5).
    fn format(&mut self, name: &Atom, style: &CounterStyle, separator: Option<&str>) -> String {
        self.innermost(name);
        let stack = &self.counters[name];
        let styles = self.styles;
        match separator {
            None => styles.counter_text(style, stack.last().expect("instantiated").value),
            Some(separator) => stack
                .iter()
                .map(|counter| styles.counter_text(style, counter.value))
                .collect::<Vec<_>>()
                .join(separator),
        }
    }

    /// `counter-reset`, `counter-increment` and `counter-set` of one box.
    fn apply(&mut self, style: &ComputedValues) {
        self.element += 1;
        let counters = style.get_counters();

        for pair in counters.counter_reset.iter() {
            // stylo's i32::MIN is `reversed(name)` with no initial value.
            let tally = (pair.is_reversed && pair.value == i32::MIN).then(|| {
                self.reversed.push(Tally::default());
                self.reversed.len() - 1
            });
            let value = match tally {
                Some(tally) => self
                    .reversed_initials
                    .as_ref()
                    .map_or(0, |initials| initials[tally]),
                None => pair.value,
            };
            self.instantiate(&pair.name.0, value, pair.is_reversed, tally);
        }

        let list_item = self.list_item.clone();
        let mut increments: Vec<(Atom, i32)> = counters
            .counter_increment
            .iter()
            .map(|pair| (pair.name.0.clone(), pair.value))
            .collect();
        if style.get_box().display.is_list_item()
            && !increments.iter().any(|(name, _)| *name == list_item)
        {
            // A reversed list counts down (css-lists-3 §4.6).
            let step = if self.innermost(&list_item).is_reversed {
                -1
            } else {
                1
            };
            increments.push((list_item, step));
        }
        for (name, increment) in increments {
            let element = self.element;
            let counter = self.innermost(&name);
            counter.value = counter.value.saturating_add(increment);
            if let Some(tally) = counter.tally {
                self.reversed[tally].record(element, increment, None);
            }
        }

        for pair in counters.counter_set.iter() {
            let element = self.element;
            let counter = self.innermost(&pair.name.0);
            counter.value = pair.value;
            if let Some(tally) = counter.tally {
                self.reversed[tally].record(element, 0, Some(pair.value));
            }
        }
    }

    /// The innermost counter `name` in scope, instantiated at 0 if there is none.
    fn innermost(&mut self, name: &Atom) -> &mut Counter {
        if self.counters.get(name).is_none_or(Vec::is_empty) {
            self.instantiate(name, 0, false, None);
        }
        self.counters
            .get_mut(name)
            .and_then(|stack| stack.last_mut())
            .expect("instantiated above")
    }

    /// "Instantiate a counter" (css-lists-3 §4.4): a counter the same element or a
    /// previous sibling created is replaced, any other one is nested.
    fn instantiate(&mut self, name: &Atom, value: i32, is_reversed: bool, tally: Option<usize>) {
        let siblings = self
            .scopes
            .last_mut()
            .expect("the root scope is never popped");
        let stack = self.counters.entry(name.clone()).or_default();
        if siblings.contains(name) {
            stack.pop();
        } else {
            siblings.push(name.clone());
        }
        stack.push(Counter {
            value,
            is_reversed,
            tally,
        });
    }
}

impl BaseDocument {
    /// Resolve every counter of the document and damage the elements whose generated
    /// text or list marker changed, so box construction picks the change up.
    pub(crate) fn resolve_counters(&mut self) {
        let values = {
            let guard = self.guard.read();
            let stylist = &self.stylist;
            let mut rules_css: Vec<String> = stylist
                .iter_extra_data_origins()
                .flat_map(|(data, _)| data.counter_styles.values())
                .filter_map(|rules| rules.last())
                .map(|(rule, _)| rule.read_with(&guard).to_css_string(&guard))
                .collect();
            rules_css.sort();
            if self.incremental_layout
                && !self.root_node().has_damaged_descendants()
                && rules_css == self.counter_values.counter_style_rules
            {
                return;
            }

            // An author rule wins over a user or user-agent one of the same name.
            let rule = |name: &Atom| -> Option<&CounterStyleRule> {
                stylist
                    .iter_extra_data_origins_rev()
                    .find_map(|(data, _)| data.counter_styles.get(name))
                    .map(|rule| rule.read_with(&guard))
            };
            let styles = CounterStyles::new(&rule);
            let root_id = self.root_node().id;
            let mut walk = Walk::new(self, &styles, None);
            walk.visit_children(root_id);
            let mut values = walk.out;
            if !walk.reversed.is_empty() {
                let initials = walk.reversed.iter().map(Tally::initial).collect();
                let mut walk = Walk::new(self, &styles, Some(initials));
                walk.visit_children(root_id);
                values = walk.out;
            }
            values.counter_style_rules = rules_css;
            values
        };
        if values == self.counter_values {
            return;
        }

        let old = std::mem::replace(&mut self.counter_values, values);
        let new = &self.counter_values;
        let mut changed: Vec<NodeId> = Vec::new();
        for (key, text) in &new.pseudo_text {
            if old.pseudo_text.get(key) != Some(text) {
                changed.push(key.0);
            }
        }
        for key in old.pseudo_text.keys() {
            if !new.pseudo_text.contains_key(key) {
                changed.push(key.0);
            }
        }
        for (node_id, marker) in &new.list_markers {
            if old.list_markers.get(node_id) != Some(marker) {
                changed.push(*node_id);
            }
        }
        for node_id in old.list_markers.keys() {
            if !new.list_markers.contains_key(node_id) {
                changed.push(*node_id);
            }
        }
        for node_id in changed {
            if let Some(node) = self.nodes.get_mut(node_id) {
                node.insert_damage(ALL_DAMAGE);
            }
        }
    }
}
