//! A read-only view of the document's CSS animations and transitions as the last
//! style resolve left them: what an embedder diffs between resolves to fire
//! `animation*` and `transition*` events.

use blitz_traits::node_id::NodeId;
use style::Atom;
use style::animation::{
    Animation, AnimationSetKey, AnimationState, ElementAnimationSet, KeyframesIterationState,
    Transition,
};
use style::selector_parser::PseudoElement;

use crate::BaseDocument;

/// What an [`AnimationSnapshot`] runs.
#[derive(Clone, Debug, PartialEq)]
pub enum AnimationSource {
    /// A CSS animation, by its `animation-name`.
    Animation(Atom),
    /// A CSS transition, by its property name (`background-color`, `--x`).
    Transition(String),
}

/// One CSS animation or transition of one element at the time of the last style
/// resolve. `(node_id, pseudo_element, source)` identifies it across resolves.
#[derive(Clone, Debug, PartialEq)]
pub struct AnimationSnapshot {
    pub node_id: NodeId,
    /// The pseudo-element of `node_id` it runs on, if any.
    pub pseudo_element: Option<PseudoElement>,
    pub source: AnimationSource,
    /// `Finished` and `Canceled` (which includes the removal of the element from
    /// the document) appear only in the snapshot of the resolve that reached them.
    pub state: AnimationState,
    /// The current iteration, counted from 0. Always 0 for a transition.
    pub iteration: f64,
    /// Seconds of active time elapsed at the resolve, excluding a positive delay
    /// and including the skipped part of a negative one, clamped to the active
    /// duration.
    pub elapsed: f64,
}

impl AnimationSnapshot {
    pub(crate) fn of_animation(key: &AnimationSetKey, animation: &Animation, now: f64) -> Self {
        let iteration = match animation.iteration_state {
            KeyframesIterationState::Finite(current, _)
            | KeyframesIterationState::Infinite(current) => current,
        };
        let iteration_duration = animation.current_iteration_duration();
        let in_iteration = match animation.state {
            AnimationState::Paused(progress) => progress * animation.duration,
            AnimationState::Finished => iteration_duration,
            AnimationState::Pending | AnimationState::Running | AnimationState::Canceled => {
                now - animation.started_at
            }
        };
        Self {
            node_id: node_id(key),
            pseudo_element: key.pseudo_element.clone(),
            source: AnimationSource::Animation(animation.name.clone()),
            state: animation.state.clone(),
            iteration,
            elapsed: iteration * animation.duration + in_iteration.clamp(0.0, iteration_duration),
        }
    }

    pub(crate) fn of_transition(key: &AnimationSetKey, transition: &Transition, now: f64) -> Self {
        let duration = transition.property_animation.duration;
        let elapsed = match transition.state {
            AnimationState::Paused(progress) => progress * duration,
            AnimationState::Finished => duration,
            AnimationState::Pending | AnimationState::Running | AnimationState::Canceled => {
                now - transition.start_time
            }
        };
        Self {
            node_id: node_id(key),
            pseudo_element: key.pseudo_element.clone(),
            source: AnimationSource::Transition(
                transition
                    .property_animation
                    .property_id()
                    .name()
                    .into_owned(),
            ),
            state: transition.state.clone(),
            iteration: 0.0,
            elapsed: elapsed.clamp(0.0, duration),
        }
    }

    /// Every animation and transition of `set`.
    pub(crate) fn of_set(
        key: &AnimationSetKey,
        set: &ElementAnimationSet,
        now: f64,
    ) -> impl Iterator<Item = Self> {
        let animations = set
            .animations
            .iter()
            .map(move |animation| Self::of_animation(key, animation, now));
        let transitions = set
            .transitions
            .iter()
            .map(move |transition| Self::of_transition(key, transition, now));
        animations.chain(transitions)
    }
}

fn node_id(key: &AnimationSetKey) -> NodeId {
    NodeId::from_u64(key.node.id() as u64)
}

impl BaseDocument {
    /// The document's CSS animations and transitions as the last style resolve
    /// left them, in no particular order. A reader that fires events reads it
    /// after every resolve: a `Finished` or `Canceled` entry is gone by the next.
    pub fn animation_snapshot(&self) -> &[AnimationSnapshot] {
        &self.animation_snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Attribute, DocumentConfig, qual_name};
    use blitz_traits::shell::{ColorScheme, Viewport};

    /// `<html><body><div style="{style}"></div></body></html>` under the `css` sheet.
    fn make_doc(css: &str, style: &str) -> (BaseDocument, NodeId) {
        let mut doc = BaseDocument::new(DocumentConfig {
            viewport: Some(Viewport::new(400, 300, 1.0, ColorScheme::Light)),
            ..Default::default()
        });
        doc.add_user_agent_stylesheet(css);
        let root_id = doc.root_node().id;
        let mut mutator = doc.mutate();
        let html = mutator.create_element(qual_name!("html"), vec![]);
        let body = mutator.create_element(qual_name!("body"), vec![]);
        let div = mutator.create_element(
            qual_name!("div"),
            vec![Attribute {
                name: qual_name!("style"),
                value: style.to_string(),
            }],
        );
        mutator.append_children(body, &[div]);
        mutator.append_children(html, &[body]);
        mutator.append_children(root_id, &[html]);
        drop(mutator);
        (doc, div)
    }

    /// `(node, source, state, iteration, elapsed)` of every entry.
    fn entries(doc: &BaseDocument) -> Vec<(NodeId, AnimationSource, AnimationState, f64, f64)> {
        doc.animation_snapshot()
            .iter()
            .map(|s| {
                assert_eq!(s.pseudo_element, None);
                (
                    s.node_id,
                    s.source.clone(),
                    s.state.clone(),
                    s.iteration,
                    s.elapsed,
                )
            })
            .collect()
    }

    const KEYFRAMES: &str = "@keyframes k { from { opacity: 0 } to { opacity: 1 } }";

    /// The states an embedder turns into `animationstart`/`animationiteration`/
    /// `animationend`: a 1s animation with 2 iterations is created pending, runs,
    /// enters its second iteration, and reports `Finished` with its whole active
    /// duration for exactly one resolve.
    #[test]
    fn a_two_iteration_animation_runs_iterates_and_finishes() {
        let (mut doc, div) = make_doc(KEYFRAMES, "animation: k 1s 2");
        let k = AnimationSource::Animation(Atom::from("k"));
        let at = |doc: &mut BaseDocument, now: f64| {
            doc.resolve(now);
            entries(doc)
        };
        use AnimationState::{Finished, Pending, Running};
        assert_eq!(at(&mut doc, 0.0), [(div, k.clone(), Pending, 0.0, 0.0)]);
        assert_eq!(at(&mut doc, 0.5), [(div, k.clone(), Running, 0.0, 0.5)]);
        assert_eq!(at(&mut doc, 1.5), [(div, k.clone(), Running, 1.0, 1.5)]);
        assert_eq!(at(&mut doc, 2.5), [(div, k, Finished, 1.0, 2.0)]);
        assert_eq!(
            at(&mut doc, 3.0),
            [],
            "a finished animation is reported once"
        );
    }

    /// Removing the element cancels its running animation (`animationcancel`),
    /// reported with the active time it reached.
    #[test]
    fn removing_the_element_cancels_its_animation() {
        let (mut doc, div) = make_doc(KEYFRAMES, "animation: k 1s infinite");
        doc.resolve(0.0);
        doc.resolve(0.5);
        doc.mutate().remove_node(div);
        doc.resolve(0.75);
        let k = AnimationSource::Animation(Atom::from("k"));
        assert_eq!(
            entries(&doc),
            [(div, k, AnimationState::Canceled, 0.0, 0.75)]
        );
        doc.resolve(1.0);
        assert_eq!(entries(&doc), []);
    }

    /// `display: none` on an ancestor cancels a running animation (stylo never
    /// restyles the element), but one that finishes in the same resolve reports
    /// only `Finished` (`animationend`), never also `Canceled`.
    #[test]
    fn a_display_none_ancestor_cancels_only_an_unfinished_animation() {
        let hide_parent_at = |now: f64| {
            let (mut doc, div) = make_doc(KEYFRAMES, "animation: k 1s");
            doc.resolve(0.0);
            doc.resolve(0.5);
            let body = doc.get_node(div).unwrap().parent.unwrap();
            doc.mutate()
                .set_attribute(body, qual_name!("style"), "display: none");
            doc.resolve(now);
            (entries(&doc), div)
        };
        let k = AnimationSource::Animation(Atom::from("k"));
        let (running, div) = hide_parent_at(0.75);
        assert_eq!(
            running,
            [(div, k.clone(), AnimationState::Canceled, 0.0, 0.75)]
        );
        let (finishing, div) = hide_parent_at(1.5);
        assert_eq!(finishing, [(div, k, AnimationState::Finished, 0.0, 1.0)]);
    }

    /// A transition is named by its property and timed from its start, for
    /// `transitionrun`/`transitionstart`/`transitionend`.
    #[test]
    fn a_transition_reports_its_property_and_progress() {
        let (mut doc, div) = make_doc("", "opacity: 0; transition: opacity 1s");
        doc.resolve(0.0);
        assert_eq!(entries(&doc), []);
        doc.mutate().set_attribute(
            div,
            qual_name!("style"),
            "opacity: 1; transition: opacity 1s",
        );
        let opacity = AnimationSource::Transition(String::from("opacity"));
        use AnimationState::{Finished, Pending, Running};
        doc.resolve(0.25);
        assert_eq!(entries(&doc), [(div, opacity.clone(), Pending, 0.0, 0.0)]);
        doc.resolve(0.75);
        assert_eq!(entries(&doc), [(div, opacity.clone(), Running, 0.0, 0.5)]);
        doc.resolve(1.5);
        assert_eq!(entries(&doc), [(div, opacity, Finished, 0.0, 1.0)]);
    }
}
