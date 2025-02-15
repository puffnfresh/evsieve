use crate::error::Context;
use crate::range::Range;
use crate::key::Key;
use crate::event::{Event, Channel, EventFlag};
use crate::state::State;
use crate::subprocess;
use crate::loopback;
use crate::loopback::LoopbackHandle;
use crate::capability::{Capability, CapMatch};
use crate::time::Duration;
use std::collections::HashSet;

// TODO: Add a unittest for a hook with multiple send-keys.
// TODO: Check whether the ordering behaviour of --withhold is consistent with --hook send-key.

pub type Effect = Box<dyn Fn(&mut State)>;

/// Represents the point at time after which a pressed tracker is no longer valid.
/// Usually determined by the --hook period= clause.
pub enum ExpirationTime {
    Never,
    Until(loopback::Token),
}

enum TrackerState {
    /// This tracker's corresponding key is held down. Also keeps track of how much time is
    /// left until this tracker expires due to a period= clause. If no period= clause was
    /// specified, then its expiration time shall be ExpirationTime::Never.
    Active(ExpirationTime),
    /// This tracker's corresponding key is not held down.
    Inactive,
    /// Based on the events that were received by this Tracker, the state should be active,
    /// but it is counted as inactive due to some circumstances, e.g. because the period
    /// in which the hook must be triggered expired, or because this tracker was activated
    /// before its predecessors in a sequential hook.
    /// 
    /// To activate this tracker, it first needs to return to Inactive and then activate.
    Invalid,
}

impl TrackerState {
    fn is_active(&self) -> bool {
        match self {
            TrackerState::Active (_) => true,
            TrackerState::Inactive | TrackerState::Invalid => false,
        }
    }
}

/// A tracker is used to track whether a certain key is held down. This is useful for --hook type
/// arguments.
struct Tracker {
    key: Key,
    range: Range,

    /// The state is mutable at runtime. It reflects whether the key tracked by this tracker
    /// is currently pressed or not, as well as which event triggered it and when.
    state: TrackerState,
}

impl Tracker {
    fn new(mut key: Key) -> Tracker {
        let range = key.pop_value().unwrap_or_else(|| Range::new(Some(1), None));
        Tracker {
            key,
            range,
            state: TrackerState::Inactive,
        }
    }

    /// Returns true if this event might interact with this tracker in some way.
    fn matches(&self, event: &Event) -> bool {
        self.key.matches(event)
    }

    /// Returns true if any event with the given channel might interact with this
    /// tracker in some way.
    fn matches_channel(&self, channel: Channel) -> bool {
        self.key.matches_channel(channel)
    }

    /// Returns whether this event would turn this tracker on or off.
    /// Only returns sensible values if self.matches(event) is true.
    fn activates_by(&self, event: Event) -> bool {
        self.range.contains(event.value)
    }

    fn is_active(&self) -> bool {
        match self.state {
            TrackerState::Active(_) => true,
            TrackerState::Invalid | TrackerState::Inactive => false,
        }
    }

    /// Like Clone::clone, but does not clone the runtime state of the Tracker.
    fn clone_empty(&self) -> Tracker {
        Tracker {
            key: self.key.clone(),
            range: self.range,
            state: TrackerState::Inactive,
        }
    }
}

/// The Trigger is the inner part of the hook that keeps track of when the hook is supposed to
/// activate.
pub struct Trigger {
    /// If Some, then all trackers must be activated within a certain duration from the first
    /// tracker to activate in order to trigger the hook.
    period: Option<Duration>,
    /// If true, then all trackers belonging to this Trigger must be triggered in sequential
    /// order. If a tracker is activated while its previous tracker is still inactive, then
    /// that tracker becomes invalid.
    sequential: bool,

    trackers: Vec<Tracker>,
    state: TriggerState,
}

/// Returned by Trigger::apply to inform the caller what effect the provided event had on
/// the hook.
#[derive(Clone, Copy)]
pub enum TriggerResponse {
    /// This event does not interact with this hook in any way.
    None,
    /// This event matches the key one of the trackers. Does not guarantee that the actual
    /// state of the tracker was changed.
    Matches,
    /// The hook has activated because of this event. Its effects should be triggered.
    Activates,
    /// The hook has released because of this event. Its on-release effects should be triggered.
    Releases,
}

#[derive(Clone, Copy)]
enum TriggerState {
    /// All trackers are currently pressed.
    Active,
    /// Not all trackers are currently pressed.
    Inactive,
}

impl Trigger {
    pub fn new(keys: Vec<Key>, period: Option<Duration>, sequential: bool) -> Trigger {
        let trackers = keys.into_iter().map(Tracker::new).collect();
        Trigger {
            period, trackers, sequential,
            state: TriggerState::Inactive,
        }
    }

    pub fn apply(&mut self, event: Event, loopback: &mut LoopbackHandle) -> TriggerResponse {
        let mut any_tracker_matched: bool = false;

        for tracker in self.trackers.iter_mut()
            .filter(|tracker| tracker.matches(&event))
        {
            any_tracker_matched = true;

            if tracker.activates_by(event) {
                match tracker.state {
                    // If this tracker was inactive, activate it.
                    TrackerState::Inactive => {
                        // Note: if this hook is sequential, this activation may get invalidated
                        // later in this function.
                        tracker.state = TrackerState::Active(
                            acquire_expiration_token(self.period, loopback)
                        );
                    },
                    TrackerState::Active(..) | TrackerState::Invalid => {},
                }
            } else {
                tracker.state = TrackerState::Inactive;
            };
        }
        
        if ! any_tracker_matched {
            // No trackers care about this event.
            return TriggerResponse::None;
        }

        if self.sequential {
            // Invalidate all trackers that activated out of order.
            self.trackers.iter_mut()
                // Skip all trackers that are consecutively active from the start.
                .skip_while(|tracker| tracker.is_active())
                // ... then find all trackers that are active but not consecutively so.
                .filter(|tracker| tracker.is_active())
                // ... and invalidate them.
                // TODO: Consider canceling the activation token.
                .for_each(|tracker| tracker.state = TrackerState::Invalid);
        }

        // Check if we transitioned between active and inactive.
        let all_trackers_active = self.trackers.iter().all(|tracker| tracker.state.is_active());

        match (self.state, all_trackers_active) {
            (TriggerState::Inactive, true) => {
                self.state = TriggerState::Active;
                // TODO: Cancel tokens?
                for tracker in &mut self.trackers {
                    tracker.state = TrackerState::Active(ExpirationTime::Never);
                }
                TriggerResponse::Activates
            },
            (TriggerState::Active, false) => {
                self.state = TriggerState::Inactive;
                TriggerResponse::Releases
            },
            (TriggerState::Active {..}, true) | (TriggerState::Inactive, false)
                => TriggerResponse::Matches,
        }
    }

    /// Release a tracker that has expired. If a tracker expired, returns the associated key.
    /// It is important that the Tokens are unique for this function to work correctly.
    /// 
    /// Returns true if at least one tracker expired. Returns false otherwise.
    pub fn wakeup(&mut self, token: &loopback::Token) -> bool {
        let mut result = false;
        for tracker in &mut self.trackers {
            match tracker.state {
                TrackerState::Inactive => {},
                TrackerState::Invalid => {},
                TrackerState::Active(ExpirationTime::Never) => {},
                TrackerState::Active(ExpirationTime::Until(ref other_token)) => {
                    if token == other_token {
                        // This tracker expired.
                        tracker.state = TrackerState::Invalid;
                        result = true;
                    }
                }
            }
        }
        result
    }

    /// Returns true if any of the active trackers might have been activated by an event
    /// with the provided channel, regardless of whether that channel actually activated them.
    pub fn has_active_tracker_matching_channel(&self, channel: Channel) -> bool {
        self.trackers.iter()
            .filter(|tracker| tracker.is_active())
            .any(   |tracker| tracker.matches_channel(channel))
    }

    /// Returns true if any of the might be activated by an event with the provided channel.
    pub fn has_tracker_matching_channel(&self, channel: Channel) -> bool {
        self.trackers.iter()
            .any(|tracker| tracker.matches_channel(channel))
    }

    /// Like Clone::clone, but does not clone the runtime state of the Trigger.
    pub fn clone_empty(&self) -> Trigger {
        Trigger {
            sequential: self.sequential,
            period: self.period,
            trackers: self.trackers.iter().map(Tracker::clone_empty).collect(),
            state: TriggerState::Inactive,
        }
    }
}

pub struct Hook {
    /// Effects that shall be triggered if this hook activates, i.e. all keys are held down simultaneously.
    effects: Vec<Effect>,
    /// Effects that shall be released after one of the keys has been released after activating.
    release_effects: Vec<Effect>,
    /// If true, this Hook is associated with a --withhold argument and we need to mark all
    /// events that matched one of our keys.
    mark_withholdable: bool,

    /// The current state mutable at runtime.
    trigger: Trigger,

    /// The substructure responsible for generating additinal events for the send-key clause.
    event_dispatcher: EventDispatcher,
}

impl Hook {
    pub fn new(trigger: Trigger, event_dispatcher: EventDispatcher, mark_withholdable: bool) -> Hook {
        Hook {
            trigger,
            mark_withholdable,
            effects: Vec::new(),
            release_effects: Vec::new(),
            event_dispatcher,
        }
    }

    fn apply(&mut self, mut event: Event, events_out: &mut Vec<Event>, state: &mut State, loopback: &mut LoopbackHandle) {
        let response = self.trigger.apply(event, loopback);

        if self.mark_withholdable {
            match response {
                TriggerResponse::Matches | TriggerResponse::Activates | TriggerResponse::Releases
                    => event.flags.set(EventFlag::Withholdable),
                TriggerResponse::None => (),
            }
        }

        self.event_dispatcher.map_event(event, response, events_out);

        match response {
            TriggerResponse::Activates => {
                self.apply_effects(state);
            },
            TriggerResponse::Releases => {
                self.apply_release_effects(state);
            },
            TriggerResponse::Matches | TriggerResponse::None => (),
        }
    }

    pub fn apply_to_all(
        &mut self,
        events: &[Event],
        events_out: &mut Vec<Event>,
        state: &mut State,
        loopback: &mut LoopbackHandle,
    ) {
        for event in events {
            self.apply(*event, events_out, state, loopback);
        }
    }

    pub fn apply_to_all_caps(
        &self,
        caps: &[Capability],
        caps_out: &mut Vec<Capability>,
    ) {
        caps_out.extend(caps);
        self.event_dispatcher.generate_additional_caps(&self.trigger, caps, caps_out);
    }

    pub fn wakeup(&mut self, token: &loopback::Token) {
        self.trigger.wakeup(token);
    }

    /// Runs all effects that should be ran when this hook triggers.
    fn apply_effects(&self, state: &mut State) {
        for effect in &self.effects {
            effect(state);
        }
    }

    /// Runs all effects that should be ran when this hook has triggered and
    /// a tracked key is released.
    fn apply_release_effects(&self, state: &mut State)
    {
        for release_effect in &self.release_effects {
            release_effect(state);
        }
    }

    /// Makes this hook run an effect when it triggers.
    pub fn add_effect(&mut self, effect: Effect) {
        self.effects.push(effect);
    }

    /// Makes this hook invoke an external subprocess when this hook is triggered.
    pub fn add_command(&mut self, program: String, args: Vec<String>) {
        self.add_effect(
            Box::new(move |_| {
                subprocess::try_spawn(program.clone(), args.clone()).print_err();
            })
        );
    }
}

/// The part of the --hook that is responsible for handling the send-key= clause.
/// Implemented separately from the hook because it is possible we want to remove this
/// functionality from the --hook itself and move it to a --withhold instead.
pub struct EventDispatcher {
    /// Keys that shall be sent on press and release.
    send_keys: Vec<Key>,
    /// The last event that activated the corresponding Hook/Trigger.
    activating_event: Option<Event>,
}

impl EventDispatcher {
    pub fn from_send_keys(send_keys: Vec<Key>) -> EventDispatcher {
        EventDispatcher {
            send_keys,
            activating_event: None
        }
    }

    /// Similar in purpose to apply().
    fn map_event(&mut self, event: Event, trigger_response: TriggerResponse, events_out: &mut Vec<Event>) {
        match trigger_response {
            TriggerResponse::Activates => {
                events_out.push(event);
                self.activating_event = Some(event);
                for key in &self.send_keys {
                    let mut additional_event = key.merge(event);
                    additional_event.value = 1;
                    additional_event.flags.unset(EventFlag::Withholdable);
                    events_out.push(additional_event);
                };
            },
            TriggerResponse::Releases => {
                let activating_event = match self.activating_event {
                    Some(activating_event) => activating_event,
                    None => {
                        crate::utils::warn_once("Internal error: a hook released without record of being activated by any event. This is a bug.");
                        event
                    }
                };
                for key in self.send_keys.iter().rev() {
                    let mut additional_event = key.merge(activating_event);
                    additional_event.value = 0;
                    additional_event.flags.unset(EventFlag::Withholdable);
                    events_out.push(additional_event);
                }
                events_out.push(event);
            },
            TriggerResponse::Matches | TriggerResponse::None => {
                events_out.push(event);
            },
        }
    }

    /// Computes additional capabilities that can be generated by the send_keys and writes them
    /// to caps_out. This function does not add the base capabilities to the output.
    /// 
    /// Similar in purpose to apply_to_all_caps(), but does not copy the base capabilities.
    fn generate_additional_caps(&self, trigger: &Trigger, caps: &[Capability], caps_out: &mut Vec<Capability>) {
        // TODO: Fix encapsulation?
        let keys: Vec<&Key> = trigger.trackers.iter().map(|tracker| &tracker.key).collect();
        // TODO: write unittest for this function.
        let mut additional_caps: HashSet<Capability> = HashSet::new();
        // TODO: reduce this implementation to a special case of Map.

        for cap_in in caps {
            let matches_cap = keys.iter()
                .map(|key| key.matches_cap(cap_in)).max();
            match matches_cap {
                Some(CapMatch::Yes | CapMatch::Maybe) => {},
                Some(CapMatch::No) | None => continue,
            };

            additional_caps.extend(self.send_keys.iter().map(
                |key| {
                    let mut new_cap = key.merge_cap(*cap_in);
                    new_cap.value_range = Range::new(Some(0), Some(1));
                    new_cap
                }
            ));
        }

        caps_out.extend(additional_caps);
    }
}

/// If this hook has a period set, acquires a Token from the loopback and arranges for a
/// `wakeup()` call later. If no period is set, return `ExpirationTime::Never`.
fn acquire_expiration_token(period: Option<Duration>, loopback: &mut LoopbackHandle) -> ExpirationTime {
    match period {
        Some(duration) => ExpirationTime::Until(loopback.schedule_wakeup_in(duration)),
        None => ExpirationTime::Never,
    }
}
