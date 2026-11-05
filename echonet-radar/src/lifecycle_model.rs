//! A bounded Stateright model over the production session lifecycle.
//!
//! Every action calls the same `RadarModel` mutator the service calls, so a
//! transition that contradicts an invariant fails here.

use std::hash::{Hash, Hasher};
use std::time::{Duration, SystemTime};

use echonet_radar_core::{Fault, RadarModel, SessionStatus};
use stateright::{Checker as _, Model, Property};

use crate::readiness::{self, Readiness};

/// Two frames cover the pre-frame and post-frame shapes the projection reads.
const MAX_FRAMES: u64 = 2;

#[derive(Clone, Debug)]
enum State {
    NotStarted,
    Session(RadarModel),
}

#[derive(Debug, PartialEq, Eq, Hash)]
struct Key {
    status: SessionStatus,
    frames_seen: u64,
    last_frame: Option<SystemTime>,
}

impl PartialEq for State {
    fn eq(
        &self,
        other: &Self,
    ) -> bool {
        key(self) == key(other)
    }
}

impl Eq for State {}

impl Hash for State {
    fn hash<H: Hasher>(
        &self,
        state: &mut H,
    ) {
        key(self).hash(state);
    }
}

/// Stateright treats the hash as the state's identity, so this key must cover
/// every field an action can change. The actions here mutate only the session
/// lifecycle, never objects, notices or the change ring, and every session is
/// constructed with id zero, so the id is not part of the identity.
fn key(state: &State) -> Option<Key> {
    let State::Session(model) = state else {
        return None;
    };
    let snapshot = model.initial_snapshot();
    Some(Key {
        status: snapshot.status,
        frames_seen: snapshot.frames_seen,
        last_frame: snapshot.last_frame,
    })
}

fn at(seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

fn fault() -> Fault {
    Fault::Service(String::from("receive failed"))
}

/// Mirrors `run_service`: bind, receive frames, then a fault or a clean stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Start,
    Bound,
    Frame,
    Fault,
    Stop,
    Restart,
}

struct LifecycleModel;

impl Model for LifecycleModel {
    type Action = Action;
    type State = State;

    fn init_states(&self) -> Vec<Self::State> {
        vec![State::NotStarted]
    }

    fn actions(
        &self,
        state: &Self::State,
        actions: &mut Vec<Self::Action>,
    ) {
        match state {
            State::NotStarted => actions.push(Action::Start),
            State::Session(_) => actions.extend([
                Action::Bound,
                Action::Frame,
                Action::Fault,
                Action::Stop,
                Action::Restart,
            ]),
        }
    }

    fn next_state(
        &self,
        state: &Self::State,
        action: Self::Action,
    ) -> Option<Self::State> {
        let next = match (state, action) {
            (State::NotStarted, Action::Start) | (State::Session(_), Action::Restart) => {
                State::Session(RadarModel::new(0))
            },
            (State::Session(model), Action::Bound) => {
                let mut model = model.clone();
                model.start(at(1));
                State::Session(model)
            },
            (State::Session(model), Action::Frame) => {
                let frames = key(state).map_or(0, |key| key.frames_seen);
                let mut model = model.clone();
                model.record_frame(at(2 + frames));
                State::Session(model)
            },
            (State::Session(model), Action::Fault) => {
                let mut model = model.clone();
                model.degrade(fault());
                State::Session(model)
            },
            (State::Session(model), Action::Stop) => {
                let mut model = model.clone();
                model.stop();
                State::Session(model)
            },
            (State::NotStarted, _) | (State::Session(_), Action::Start) => return None,
        };
        // A mutator that ignores the event leaves the key unchanged. Modelling that
        // as a disabled action keeps the successor scans below to real transitions.
        (key(&next) != key(state)).then_some(next)
    }

    fn within_boundary(
        &self,
        state: &Self::State,
    ) -> bool {
        key(state).is_none_or(|key| key.frames_seen <= MAX_FRAMES)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        vec![
            Property::always(
                "frame timeline matches the count",
                frame_timeline_matches_the_count,
            ),
            Property::always("binding holds no history", binding_holds_no_history),
            Property::always(
                "running starts at the bind time",
                running_starts_at_the_bind_time,
            ),
            Property::always("a binding session cannot degrade", binding_never_degrades),
            Property::always(
                "a stopped session only changes through a new session",
                stopped_only_restarts,
            ),
            Property::always(
                "a degraded session records no more frames",
                degraded_records_no_more_frames,
            ),
            Property::sometimes("binding is reachable", reaches_binding),
            Property::sometimes("unverified is reachable", reaches_unverified),
            Property::sometimes("listening is reachable", reaches_listening),
            Property::sometimes(
                "degraded without a frame is reachable",
                reaches_degraded_without_a_frame,
            ),
            Property::sometimes(
                "degraded after a frame is reachable",
                reaches_degraded_after_a_frame,
            ),
            Property::sometimes("stopped is reachable", reaches_stopped),
        ]
    }
}

fn readiness(state: &State) -> Readiness {
    key(state).map_or(Readiness::NotStarted, |key| {
        readiness::from_parts(&key.status, key.frames_seen, key.last_frame)
    })
}

fn frame_timeline_matches_the_count(
    _: &LifecycleModel,
    state: &State,
) -> bool {
    key(state)
        .is_none_or(|key| key.last_frame == (key.frames_seen > 0).then(|| at(1 + key.frames_seen)))
}

fn binding_holds_no_history(
    _: &LifecycleModel,
    state: &State,
) -> bool {
    key(state).is_none_or(|key| {
        !matches!(key.status, SessionStatus::Binding)
            || (key.frames_seen == 0 && key.last_frame.is_none())
    })
}

fn running_starts_at_the_bind_time(
    _: &LifecycleModel,
    state: &State,
) -> bool {
    key(state).is_none_or(|key| match key.status {
        SessionStatus::Running { since } => since == at(1),
        _ => true,
    })
}

fn binding_never_degrades(
    model: &LifecycleModel,
    state: &State,
) -> bool {
    let Some(current) = key(state) else {
        return true;
    };
    if !matches!(current.status, SessionStatus::Binding) {
        return true;
    }
    model.next_states(state).iter().all(|next| {
        key(next).is_none_or(|next| !matches!(next.status, SessionStatus::Degraded { .. }))
    })
}

fn stopped_only_restarts(
    model: &LifecycleModel,
    state: &State,
) -> bool {
    let Some(current) = key(state) else {
        return true;
    };
    if !matches!(current.status, SessionStatus::Stopped) {
        return true;
    }
    model.next_states(state).iter().all(|next| {
        key(next).is_some_and(|next| {
            matches!(next.status, SessionStatus::Binding)
                && next.frames_seen == 0
                && next.last_frame.is_none()
        })
    })
}

fn degraded_records_no_more_frames(
    model: &LifecycleModel,
    state: &State,
) -> bool {
    let Some(current) = key(state) else {
        return true;
    };
    if !matches!(current.status, SessionStatus::Degraded { .. }) {
        return true;
    }
    model.next_states(state).iter().all(|next| {
        key(next).is_some_and(|next| {
            (matches!(next.status, SessionStatus::Stopped)
                && next.frames_seen == current.frames_seen
                && next.last_frame == current.last_frame)
                || (matches!(next.status, SessionStatus::Binding)
                    && next.frames_seen == 0
                    && next.last_frame.is_none())
        })
    })
}

fn reaches_binding(
    _: &LifecycleModel,
    state: &State,
) -> bool {
    matches!(readiness(state), Readiness::Binding)
}

fn reaches_unverified(
    _: &LifecycleModel,
    state: &State,
) -> bool {
    matches!(readiness(state), Readiness::Unverified { .. })
}

fn reaches_listening(
    _: &LifecycleModel,
    state: &State,
) -> bool {
    matches!(readiness(state), Readiness::Listening { .. })
}

fn reaches_degraded_without_a_frame(
    _: &LifecycleModel,
    state: &State,
) -> bool {
    matches!(
        readiness(state),
        Readiness::Degraded {
            last_frame: None,
            ..
        }
    )
}

fn reaches_degraded_after_a_frame(
    _: &LifecycleModel,
    state: &State,
) -> bool {
    matches!(
        readiness(state),
        Readiness::Degraded {
            last_frame: Some(_),
            ..
        }
    )
}

fn reaches_stopped(
    _: &LifecycleModel,
    state: &State,
) -> bool {
    matches!(readiness(state), Readiness::Stopped)
}

#[test]
fn the_session_lifecycle_only_moves_through_guarded_transitions() {
    let checker = LifecycleModel.checker().spawn_bfs().join();
    checker.assert_properties();
}
