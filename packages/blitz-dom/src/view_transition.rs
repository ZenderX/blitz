//! The view transition pseudo-element tree (css-view-transitions-1 §3.2).
//!
//! Given the named elements of a transition ([`ViewTransitionCapture`]), the document
//! writes the dynamic view transition style sheet of §7.3.3 and "update pseudo-element
//! styles" (§7.3.5), and every resolve styles the tree
//! `::view-transition` > `::view-transition-group(name)` > `::view-transition-image-pair(name)`
//! > `::view-transition-old(name)`, `::view-transition-new(name)` from the cascade, each
//! pseudo-element inheriting from its parent in the tree.
//!
//! [`BaseDocument::animate_view_transition`] runs the tree's CSS animations (the UA
//! crossfade and group keyframes, and any author ones) through stylo's own animation
//! state, and writes their values into the tree's styles. The embedder captures the
//! images: it hands each name its old snapshot and new element
//! ([`BaseDocument::set_view_transition_content`]), which a painter reads back.

use blitz_traits::node_id::NodeId;
use cssparser::{Parser, ParserInput, serialize_identifier};
use kurbo::Affine;
use style::Atom;
use style::animation::{AnimationState, ElementAnimationSet};
use style::context::{SharedStyleContext, StyleContext, ThreadLocalStyleContext};
use style::global_style_data::GLOBAL_STYLE_DATA;
use style::properties::ComputedValues;
use style::selector_parser::PseudoElement;
use style::servo_arc::Arc;
use style::shared_lock::StylesheetGuards;
use style::style_resolver::{PseudoElementResolution, StyleResolverForElement};
use style::stylist::RuleInclusion;
use style::thread_state::{self, ThreadState};
use style::traversal_flags::TraversalFlags;
use style_traits::ToCss;

use crate::BaseDocument;
use crate::node::RasterImageData;

/// The geometry of a captured element (css-view-transitions-1 §7.3): the size of its
/// border box, and the transform that maps that box from the snapshot containing block
/// origin to its visual position.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CapturedGeometry {
    pub width: f32,
    pub height: f32,
    pub transform: Affine,
}

/// One entry of a transition's named elements. At least one of `old` and `new` is set.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewTransitionCapture {
    /// The `view-transition-name`.
    pub name: Atom,
    /// The old element's geometry, when an old image was captured.
    pub old: Option<CapturedGeometry>,
    /// The new element's current geometry, when an element has the name after the update.
    pub new: Option<CapturedGeometry>,
}

/// The computed styles of one name's branch of the tree.
#[derive(Clone, Debug)]
pub struct ViewTransitionGroupStyles {
    pub name: Atom,
    pub group: Arc<ComputedValues>,
    pub image_pair: Arc<ComputedValues>,
    /// `None` when the capture has no old image.
    pub old: Option<Arc<ComputedValues>>,
    /// `None` when the capture has no new element.
    pub new: Option<Arc<ComputedValues>>,
}

/// The computed styles of the pseudo-element tree, groups in capture order.
#[derive(Clone, Debug)]
pub struct ViewTransitionTree {
    /// `::view-transition`.
    pub root: Arc<ComputedValues>,
    pub groups: Vec<ViewTransitionGroupStyles>,
}

impl ViewTransitionTree {
    /// The style of the pseudo-element of this tree that `pseudo` names exactly.
    pub fn style(&self, pseudo: &PseudoElement) -> Option<&Arc<ComputedValues>> {
        if *pseudo == PseudoElement::ViewTransition {
            return Some(&self.root);
        }
        let group = self
            .groups
            .iter()
            .find(|group| Some(&group.name) == pseudo.view_transition_name())?;
        match pseudo {
            PseudoElement::ViewTransitionGroup(_) => Some(&group.group),
            PseudoElement::ViewTransitionImagePair(_) => Some(&group.image_pair),
            PseudoElement::ViewTransitionOld(_) => group.old.as_ref(),
            PseudoElement::ViewTransitionNew(_) => group.new.as_ref(),
            _ => None,
        }
    }

    /// Every pseudo-element of the tree with its style, parents first.
    fn styles_mut(&mut self) -> impl Iterator<Item = (PseudoElement, &mut Arc<ComputedValues>)> {
        let root = std::iter::once((PseudoElement::ViewTransition, &mut self.root));
        root.chain(self.groups.iter_mut().flat_map(|group| {
            let name = &group.name;
            [
                Some((
                    PseudoElement::ViewTransitionGroup(name.clone()),
                    &mut group.group,
                )),
                Some((
                    PseudoElement::ViewTransitionImagePair(name.clone()),
                    &mut group.image_pair,
                )),
                group
                    .old
                    .as_mut()
                    .map(|style| (PseudoElement::ViewTransitionOld(name.clone()), style)),
                group
                    .new
                    .as_mut()
                    .map(|style| (PseudoElement::ViewTransitionNew(name.clone()), style)),
            ]
            .into_iter()
            .flatten()
        }))
    }
}

/// What a painter shows for one name: the old snapshot and the live new element.
#[derive(Clone, Debug, Default)]
struct CapturedContent {
    old_image: Option<RasterImageData>,
    new_element: Option<NodeId>,
}

/// An active transition: its captures, the dynamic style sheet written for them, the
/// tree as the last resolve styled it, and its animations.
pub(crate) struct ViewTransitionState {
    captures: Vec<ViewTransitionCapture>,
    dynamic_css: String,
    tree: Option<ViewTransitionTree>,
    /// Per capture, in capture order.
    content: Vec<CapturedContent>,
    /// The animations of each pseudo-element, started by the first
    /// [`BaseDocument::animate_view_transition`] after the tree was styled.
    animations: Option<Vec<(PseudoElement, ElementAnimationSet)>>,
}

impl BaseDocument {
    /// Show the pseudo-element tree for `captures`, replacing any previous one, styled
    /// from the next [`BaseDocument::resolve`]. Call it again whenever a geometry
    /// changes: the group sizes and transforms are written into the style sheet.
    pub fn set_view_transition(&mut self, captures: Vec<ViewTransitionCapture>) {
        for capture in &captures {
            assert!(
                capture.old.is_some() || capture.new.is_some(),
                "view transition capture {:?} has neither an old image nor a new element",
                capture.name
            );
        }
        self.clear_view_transition();
        let dynamic_css = dynamic_style_sheet(&captures);
        self.add_user_agent_stylesheet(&dynamic_css);
        let content = vec![CapturedContent::default(); captures.len()];
        self.view_transition = Some(ViewTransitionState {
            captures,
            dynamic_css,
            tree: None,
            content,
            animations: None,
        });
    }

    /// Give the capture named `name` its old snapshot and its new element.
    /// `old_image` holds premultiplied RGBA8 at device scale, covering the old
    /// element's border box (the viewport for the root element). `new_element` is
    /// painted live as `::view-transition-new(name)`, and not in place.
    ///
    /// Panics if the shown transition has no capture named `name`.
    pub fn set_view_transition_content(
        &mut self,
        name: &Atom,
        old_image: Option<RasterImageData>,
        new_element: Option<NodeId>,
    ) {
        let state = self
            .view_transition
            .as_mut()
            .expect("set_view_transition_content needs a shown view transition");
        let index = state
            .captures
            .iter()
            .position(|capture| capture.name == *name)
            .unwrap_or_else(|| panic!("the view transition has no capture named {name:?}"));
        state.content[index] = CapturedContent {
            old_image,
            new_element,
        };
    }

    /// The captures of the shown transition, in capture order.
    pub fn view_transition_captures(&self) -> &[ViewTransitionCapture] {
        self.view_transition
            .as_ref()
            .map_or(&[], |state| &state.captures)
    }

    /// The old snapshot given to the capture named `name`
    /// ([`Self::set_view_transition_content`]).
    pub fn view_transition_old_image(&self, name: &Atom) -> Option<&RasterImageData> {
        self.view_transition_content(name)?.old_image.as_ref()
    }

    /// The new element given to the capture named `name`
    /// ([`Self::set_view_transition_content`]).
    pub fn view_transition_new_element(&self, name: &Atom) -> Option<NodeId> {
        self.view_transition_content(name)?.new_element
    }

    /// The new elements of the shown transition: each paints as its
    /// `::view-transition-new()` image only.
    pub fn view_transition_new_elements(&self) -> Vec<NodeId> {
        self.view_transition
            .as_ref()
            .map_or_else(Vec::new, |state| {
                state
                    .content
                    .iter()
                    .filter_map(|content| content.new_element)
                    .collect()
            })
    }

    fn view_transition_content(&self, name: &Atom) -> Option<&CapturedContent> {
        let state = self.view_transition.as_ref()?;
        let index = state
            .captures
            .iter()
            .position(|capture| capture.name == *name)?;
        Some(&state.content[index])
    }

    /// The rendered elements with a `view-transition-name` (css-view-transitions-1 §7.3
    /// "capture the old state", "capture the new state"), in tree order, or the first
    /// name two of them share, which fails the capture.
    ///
    /// A `display: none` element and its subtree are not rendered. `match-element`
    /// (css-view-transitions-2) names no element here.
    pub fn view_transition_named_elements(&self) -> Result<Vec<(Atom, NodeId)>, Atom> {
        let mut named = Vec::new();
        let Some(root) = self.try_root_element() else {
            return Ok(named);
        };
        let mut stack = vec![root.id];
        while let Some(id) = stack.pop() {
            let node = &self.nodes[id];
            if !node.is_element() {
                continue;
            }
            let Some(style) = node.primary_styles() else {
                continue;
            };
            if style.get_box().clone_display().is_none() {
                continue;
            }
            if let Some(name) = view_transition_name(&style) {
                if named.iter().any(|(other, _)| *other == name) {
                    return Err(name);
                }
                named.push((name, id));
            }
            stack.extend(node.children.iter().rev());
        }
        Ok(named)
    }

    /// Start (at the first call after the tree was styled) and advance the tree's CSS
    /// animations to `now` (seconds, the clock of [`BaseDocument::resolve`]), and write
    /// their values into [`Self::view_transition_tree`]'s styles. Call it after each
    /// resolve: a resolve restyles the tree from the cascade.
    pub fn animate_view_transition(&mut self, now: f64) {
        let Some(mut state) = self.view_transition.take() else {
            return;
        };
        if let Some(tree) = state.tree.as_mut() {
            thread_state::enter(ThreadState::LAYOUT);
            let read_guard = self.guard.read();
            let context = SharedStyleContext {
                traversal_flags: TraversalFlags::empty(),
                stylist: &self.stylist,
                options: GLOBAL_STYLE_DATA.options.clone(),
                guards: StylesheetGuards::same(&read_guard),
                visited_styles_enabled: false,
                animations: Default::default(),
                current_time_for_animations: now,
                snapshot_map: &self.snapshots,
                registered_speculative_painters: &crate::stylo::RegisteredPaintersImpl,
            };
            let root_element = self.root_element();
            let animations = state.animations.get_or_insert_with(|| {
                let mut thread_local = ThreadLocalStyleContext::new();
                let mut style_context = StyleContext {
                    shared: &context,
                    thread_local: &mut thread_local,
                };
                // Keyframes resolve against the originating element, the root.
                let mut resolver = StyleResolverForElement::new(
                    root_element,
                    &mut style_context,
                    RuleInclusion::All,
                    PseudoElementResolution::IfApplicable,
                );
                tree.styles_mut()
                    .map(|(pseudo, style)| {
                        let mut set = ElementAnimationSet::default();
                        set.update_animations_for_new_style(
                            root_element,
                            &context,
                            style,
                            &mut resolver,
                        );
                        (pseudo, set)
                    })
                    .collect()
            });
            for ((pseudo, set), (styled, style)) in animations.iter_mut().zip(tree.styles_mut()) {
                debug_assert_eq!(*pseudo, styled, "the tree keeps its shape while animating");
                tick_animations(set, now);
                set.apply_active_animations(&context, style);
            }
            drop(read_guard);
            thread_state::exit(ThreadState::LAYOUT);
        }
        self.view_transition = Some(state);
    }

    /// Whether the shown transition's animations still run: true until the tree has
    /// been styled and animated once, then while one of its animations is pending,
    /// running or paused (css-view-transitions-1 §7.8 "handle transition frame").
    pub fn is_view_transition_animating(&self) -> bool {
        let Some(state) = self.view_transition.as_ref() else {
            return false;
        };
        let Some(animations) = state.animations.as_ref() else {
            return true;
        };
        animations.iter().any(|(_, set)| {
            set.animations.iter().any(|animation| {
                matches!(
                    animation.state,
                    AnimationState::Pending | AnimationState::Running | AnimationState::Paused(_)
                )
            })
        })
    }

    /// Remove the pseudo-element tree and its dynamic style sheet.
    pub fn clear_view_transition(&mut self) {
        if let Some(state) = self.view_transition.take() {
            self.remove_user_agent_stylesheet(&state.dynamic_css);
        }
    }

    /// The pseudo-element tree as of the last resolve, if a transition is shown.
    pub fn view_transition_tree(&self) -> Option<&ViewTransitionTree> {
        self.view_transition.as_ref()?.tree.as_ref()
    }

    /// Restyle the tree of the shown transition. Runs after the style traversal, which
    /// flushes the stylist the dynamic style sheet was added to.
    pub(crate) fn resolve_view_transition(&mut self) {
        let Some(mut state) = self.view_transition.take() else {
            return;
        };
        state.tree = Some(self.style_view_transition_tree(&state.captures));
        self.view_transition = Some(state);
    }

    fn style_view_transition_tree(&self, captures: &[ViewTransitionCapture]) -> ViewTransitionTree {
        let read_guard = self.guard.read();
        let guards = StylesheetGuards::same(&read_guard);
        let root_element = self.root_element();
        let root_style = root_element
            .primary_styles()
            .expect("the root element is styled after the style traversal");
        let style = |pseudo: PseudoElement, parent: &ComputedValues| {
            let matches_selector =
                |selector: &PseudoElement| pseudo.matches_view_transition_selector(selector);
            self.stylist
                .lazily_compute_pseudo_element_style(
                    &guards,
                    root_element,
                    &pseudo,
                    RuleInclusion::All,
                    parent,
                    false,
                    Some(&matches_selector),
                )
                .expect("a lazy pseudo-element style that is not a probe always computes")
        };

        let root = style(PseudoElement::ViewTransition, &root_style);
        let groups = captures
            .iter()
            .map(|capture| {
                let name = &capture.name;
                let group = style(PseudoElement::ViewTransitionGroup(name.clone()), &root);
                let image_pair =
                    style(PseudoElement::ViewTransitionImagePair(name.clone()), &group);
                let old = capture
                    .old
                    .map(|_| style(PseudoElement::ViewTransitionOld(name.clone()), &image_pair));
                let new = capture
                    .new
                    .map(|_| style(PseudoElement::ViewTransitionNew(name.clone()), &image_pair));
                ViewTransitionGroupStyles {
                    name: name.clone(),
                    group,
                    image_pair,
                    old,
                    new,
                }
            })
            .collect();
        ViewTransitionTree { root, groups }
    }
}

/// The dynamic view transition style sheet of css-view-transitions-1 §7.3.3, with the
/// group size and transform of "update pseudo-element styles" (§7.3.5).
fn dynamic_style_sheet(captures: &[ViewTransitionCapture]) -> String {
    let mut css = String::new();
    for capture in captures {
        let name = css_ident(&capture.name);
        let current = capture
            .new
            .or(capture.old)
            .expect("set_view_transition checked that a capture has a geometry");
        css += &format!(
            ":root::view-transition-group({name}) {{ {} }}\n",
            geometry_declarations(&current)
        );
        match (capture.old, capture.new) {
            (None, _) => {
                css += &format!(
                    ":root::view-transition-new({name}) {{ animation-name: -ua-view-transition-fade-in; }}\n"
                );
            }
            (Some(_), None) => {
                css += &format!(
                    ":root::view-transition-old({name}) {{ animation-name: -ua-view-transition-fade-out; }}\n"
                );
            }
            (Some(old), Some(_)) => {
                let keyframes = css_ident(&format!(
                    "-ua-view-transition-group-anim-{}",
                    &*capture.name
                ));
                css += &format!(
                    "@keyframes {keyframes} {{ from {{ {} }} }}\n\
                     :root::view-transition-group({name}) {{ animation-name: {keyframes}; }}\n\
                     :root::view-transition-image-pair({name}) {{ isolation: isolate; }}\n\
                     :root::view-transition-old({name}) {{ animation-name: -ua-view-transition-fade-out, -ua-mix-blend-mode-plus-lighter; }}\n\
                     :root::view-transition-new({name}) {{ animation-name: -ua-view-transition-fade-in, -ua-mix-blend-mode-plus-lighter; }}\n",
                    geometry_declarations(&old)
                );
            }
        }
    }
    css
}

fn geometry_declarations(geometry: &CapturedGeometry) -> String {
    let [a, b, c, d, e, f] = geometry.transform.as_coeffs();
    format!(
        "width: {}px; height: {}px; transform: matrix({a}, {b}, {c}, {d}, {e}, {f});",
        geometry.width, geometry.height
    )
}

/// Advance `set`'s animations to `now`, as `resolve_stylist` does for the document's.
fn tick_animations(set: &mut ElementAnimationSet, now: f64) {
    for animation in set.animations.iter_mut() {
        if animation.state == AnimationState::Pending && animation.started_at <= now {
            animation.state = AnimationState::Running;
        }
        animation.iterate_if_necessary(now);
        if animation.state == AnimationState::Running && animation.has_ended(now) {
            animation.state = AnimationState::Finished;
        }
    }
}

/// The `view-transition-name` of `style` as a name, `None` for `none` and for
/// css-view-transitions-2's `match-element`.
pub(crate) fn view_transition_name(style: &ComputedValues) -> Option<Atom> {
    // The keyword's ident is private to stylo; its serialization is a (possibly
    // escaped) CSS ident, which the tokenizer unescapes.
    let css = style.get_ui().view_transition_name.value.to_css_string();
    let mut input = ParserInput::new(&css);
    let mut parser = Parser::new(&mut input);
    let ident = parser.expect_ident().ok()?;
    if ident.eq_ignore_ascii_case("none") || ident.eq_ignore_ascii_case("match-element") {
        return None;
    }
    Some(Atom::from(&**ident))
}

fn css_ident(name: &str) -> String {
    let mut ident = String::new();
    serialize_identifier(name, &mut ident).expect("writing to a String cannot fail");
    ident
}
