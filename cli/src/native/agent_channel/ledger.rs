//! The outcome ledger and the owner fences (agent-channel contract §4
//! "Fencing" and §6). A host that lost its channel asks here what became of
//! each frame it sent, so no frame is ever sent twice on a guess, and an
//! Action's older owner never starts anything once a newer one reached the
//! daemon.
//!
//! It lives in memory outside command custody: asking about a frame never
//! waits behind the step it asks about. Its lock is never held across an
//! await. It is bounded, and an entry it no longer holds answers `unknown`,
//! an answer that can never change.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::watch;

use super::frame::{ActionId, ChannelId, FrameId, Owner};

/// Frames whose outcome the ledger holds, oldest evicted first.
pub(crate) const ENTRIES: usize = 256;
/// Bytes of outcomes the ledger holds.
pub(crate) const BYTES: usize = 16 << 20;
/// Actions whose highest owner generation the daemon remembers.
pub(crate) const FENCES: usize = 4096;
/// Ended channels whose last received id is remembered.
const ENDED_CHANNELS: usize = 1024;

pub(crate) struct Ledger {
    state: Mutex<State>,
    /// Counts changes, so an `op_status` that waits wakes on each one.
    changed: watch::Sender<u64>,
}

#[derive(Default)]
struct State {
    /// One entry per `sequence` frame, in the order they were received.
    entries: VecDeque<Entry>,
    bytes: usize,
    channels: HashMap<ChannelId, Channel>,
    /// Channels in the order they stopped.
    stopped: VecDeque<ChannelId>,
    fences: HashMap<ActionId, Fence>,
    clock: u64,
}

struct Entry {
    channel: ChannelId,
    id: FrameId,
    action: Option<ActionId>,
    state: EntryState,
    bytes: usize,
}

enum EntryState {
    /// Received, waiting behind the channel's earlier frames.
    Queued,
    Running,
    /// `{success, steps, browser}`.
    Settled(Value),
    NotStarted,
}

impl Entry {
    fn settled(&self) -> bool {
        matches!(self.state, EntryState::Settled(_) | EntryState::NotStarted)
    }

    fn status(&self) -> Status {
        match &self.state {
            EntryState::Queued | EntryState::Running => Status::Running,
            EntryState::Settled(result) => Status::Settled(result.clone()),
            EntryState::NotStarted => Status::NotStarted,
        }
    }
}

struct Channel {
    last_received: FrameId,
    /// Its connection ended or its owner was fenced: nothing more starts.
    stopped: bool,
    /// The highest generation it carried frames under, per Action.
    actions: HashMap<ActionId, u64>,
}

struct Fence {
    generation: u64,
    touched: u64,
}

/// What became of one frame, as `op_status` answers it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Status {
    /// It ran: `{success, steps, browser}`, a stopped frame's steps being its
    /// acknowledged prefix.
    Settled(Value),
    /// Nothing of it ran, and nothing ever will.
    NotStarted,
    /// It runs, waits its turn, or may still arrive on a live channel.
    Running,
    /// The ledger does not hold it, and never will.
    Unknown,
}

impl Status {
    pub(crate) fn to_json(&self) -> Value {
        match self {
            Status::Settled(result) => json!({ "state": "settled", "result": result }),
            Status::NotStarted => json!({ "state": "not_started" }),
            Status::Running => json!({ "state": "running" }),
            Status::Unknown => json!({ "state": "unknown" }),
        }
    }
}

/// A frame of an owner older than one the daemon has seen for its Action.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Fenced;

/// An `op_status` about a frame another Action sent.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct OtherAction;

impl Default for Ledger {
    fn default() -> Self {
        Self {
            state: Mutex::default(),
            changed: watch::channel(0).0,
        }
    }
}

impl Ledger {
    fn update<T>(&self, change: impl FnOnce(&mut State) -> T) -> T {
        let result = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.clock += 1;
            let result = change(&mut state);
            state.bound();
            result
        };
        self.changed
            .send_modify(|count| *count = count.wrapping_add(1));
        result
    }

    fn read<T>(&self, read: impl FnOnce(&State) -> T) -> T {
        read(&self.state.lock().unwrap_or_else(|error| error.into_inner()))
    }

    /// Opens `channel`. An id this daemon already knows is refused: the host
    /// makes a fresh one for every open, and the ledger's key must not mix
    /// two connections' frames.
    pub(crate) fn open(&self, channel: ChannelId) -> bool {
        self.update(|state| {
            if state.channels.contains_key(&channel) {
                return false;
            }
            state.channels.insert(
                channel,
                Channel {
                    last_received: 0,
                    stopped: false,
                    actions: HashMap::new(),
                },
            );
            true
        })
    }

    /// Records the arrival of frame `id` on `channel`, before anything of it
    /// runs. A `sequence` (`queued`) gets its entry now: waiting its turn,
    /// or, when `refused` on arrival, not started.
    pub(crate) fn receive(
        &self,
        channel: ChannelId,
        id: FrameId,
        queued: Option<Option<Owner>>,
        refused: bool,
    ) {
        self.update(|state| {
            let Some(record) = state.channels.get_mut(&channel) else {
                return;
            };
            record.last_received = record.last_received.max(id);
            if let Some(owner) = queued {
                let entry_state = if refused || record.stopped {
                    EntryState::NotStarted
                } else {
                    EntryState::Queued
                };
                state.put(channel, id, owner, entry_state);
            }
        })
    }

    /// Admits a frame of `owner` on `channel`, or refuses it as fenced. A
    /// newer generation becomes the Action's fence and stops every other
    /// channel that carried the Action under an older one.
    pub(crate) fn register(&self, channel: ChannelId, owner: Owner) -> Result<(), Fenced> {
        self.update(|state| {
            let now = state.clock;
            let raised = match state.fences.get_mut(&owner.action) {
                Some(fence) if owner.generation < fence.generation => {
                    fence.touched = now;
                    return Err(Fenced);
                }
                Some(fence) => {
                    fence.touched = now;
                    let raised = owner.generation > fence.generation;
                    fence.generation = owner.generation;
                    raised
                }
                None => {
                    state.fences.insert(
                        owner.action,
                        Fence {
                            generation: owner.generation,
                            touched: now,
                        },
                    );
                    true
                }
            };
            if raised {
                let older: Vec<ChannelId> = state
                    .channels
                    .iter()
                    .filter(|(id, record)| {
                        **id != channel
                            && record
                                .actions
                                .get(&owner.action)
                                .is_some_and(|carried| *carried < owner.generation)
                    })
                    .map(|(id, _)| *id)
                    .collect();
                for older in older {
                    state.stop(older);
                }
            }
            if let Some(record) = state.channels.get_mut(&channel) {
                let carried = record.actions.entry(owner.action).or_insert(0);
                *carried = (*carried).max(owner.generation);
            }
            Ok(())
        })
    }

    /// Starts frame `id`: true when it may run. On a channel that stopped it
    /// is recorded not started instead.
    pub(crate) fn start(&self, channel: ChannelId, id: FrameId, owner: Option<Owner>) -> bool {
        self.update(|state| {
            let running = state
                .channels
                .get(&channel)
                .is_some_and(|record| !record.stopped);
            let entry_state = if running {
                EntryState::Running
            } else {
                EntryState::NotStarted
            };
            state.put(channel, id, owner, entry_state);
            running
        })
    }

    /// Whether the next step of a running frame may start.
    pub(crate) fn may_continue(&self, channel: ChannelId) -> bool {
        self.read(|state| {
            state
                .channels
                .get(&channel)
                .is_some_and(|record| !record.stopped)
        })
    }

    /// Records what a frame did: `{success, steps, browser}`.
    pub(crate) fn settle(&self, channel: ChannelId, id: FrameId, result: Value) {
        self.update(|state| {
            let bytes = result.to_string().len();
            if let Some(entry) = state.entry_mut(channel, id) {
                entry.state = EntryState::Settled(result);
                let previous = std::mem::replace(&mut entry.bytes, bytes);
                state.bytes = state.bytes - previous + bytes;
            }
        })
    }

    /// Records that nothing of frame `id` ran: it was refused before any step.
    pub(crate) fn not_started(&self, channel: ChannelId, id: FrameId, owner: Option<Owner>) {
        self.update(|state| state.put(channel, id, owner, EntryState::NotStarted))
    }

    /// The connection of `channel` ended: nothing more starts on it.
    pub(crate) fn end(&self, channel: ChannelId) {
        self.update(|state| state.stop(channel))
    }

    /// What became of frame `of` of `action`.
    pub(crate) fn status(
        &self,
        action: ActionId,
        (channel, id): (ChannelId, FrameId),
    ) -> Result<Status, OtherAction> {
        self.read(|state| match state.entry(channel, id) {
            Some(entry) if entry.action != Some(action) => Err(OtherAction),
            Some(entry) => Ok(entry.status()),
            None => Ok(match state.channels.get(&channel) {
                // Received and not held: evicted, or not a sequence.
                Some(record) if id <= record.last_received => Status::Unknown,
                Some(record) if record.stopped => Status::NotStarted,
                Some(_) => Status::Running,
                None => Status::Unknown,
            }),
        })
    }

    /// Every entry the ledger holds for `action`, in the order received.
    pub(crate) fn entries(&self, action: ActionId) -> Vec<(ChannelId, FrameId, Status)> {
        self.read(|state| {
            state
                .entries
                .iter()
                .filter(|entry| entry.action == Some(action))
                .map(|entry| (entry.channel, entry.id, entry.status()))
                .collect()
        })
    }

    /// `status`, held while it is `running` for at most `wait`.
    pub(crate) async fn status_within(
        &self,
        action: ActionId,
        of: (ChannelId, FrameId),
        wait: Duration,
    ) -> Result<Status, OtherAction> {
        self.within(
            wait,
            || self.status(action, of),
            |status| {
                status
                    .as_ref()
                    .is_ok_and(|status| *status == Status::Running)
            },
        )
        .await
    }

    /// `entries`, held while any of them is `running` for at most `wait`.
    pub(crate) async fn entries_within(
        &self,
        action: ActionId,
        wait: Duration,
    ) -> Vec<(ChannelId, FrameId, Status)> {
        self.within(
            wait,
            || self.entries(action),
            |entries| {
                entries
                    .iter()
                    .any(|(_, _, status)| *status == Status::Running)
            },
        )
        .await
    }

    async fn within<T>(
        &self,
        wait: Duration,
        read: impl Fn() -> T,
        running: impl Fn(&T) -> bool,
    ) -> T {
        let deadline = tokio::time::Instant::now() + wait;
        let mut changed = self.changed.subscribe();
        loop {
            changed.borrow_and_update();
            let answer = read();
            if !running(&answer) || tokio::time::Instant::now() >= deadline {
                return answer;
            }
            tokio::select! {
                _ = changed.changed() => {}
                _ = tokio::time::sleep_until(deadline) => {}
            }
        }
    }
}

impl State {
    fn entry(&self, channel: ChannelId, id: FrameId) -> Option<&Entry> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.channel == channel && entry.id == id)
    }

    fn entry_mut(&mut self, channel: ChannelId, id: FrameId) -> Option<&mut Entry> {
        self.entries
            .iter_mut()
            .rev()
            .find(|entry| entry.channel == channel && entry.id == id)
    }

    /// Sets a frame's state, creating its entry when it has none yet.
    fn put(&mut self, channel: ChannelId, id: FrameId, owner: Option<Owner>, state: EntryState) {
        if let Some(entry) = self.entry_mut(channel, id) {
            entry.state = state;
            return;
        }
        self.entries.push_back(Entry {
            channel,
            id,
            action: owner.map(|owner| owner.action),
            state,
            bytes: 0,
        });
    }

    /// Stops `channel`: its frames waiting their turn will never start.
    fn stop(&mut self, channel: ChannelId) {
        let Some(record) = self.channels.get_mut(&channel) else {
            return;
        };
        if record.stopped {
            return;
        }
        record.stopped = true;
        self.stopped.push_back(channel);
        for entry in self.entries.iter_mut() {
            if entry.channel == channel && matches!(entry.state, EntryState::Queued) {
                entry.state = EntryState::NotStarted;
            }
        }
    }

    /// Keeps the ledger within its bounds: the oldest settled entries go
    /// first (an entry that may still change is never evicted), then the
    /// oldest stopped channels and the least recently seen fences.
    fn bound(&mut self) {
        while self.entries.len() > ENTRIES || self.bytes > BYTES {
            let Some(oldest) = self.entries.iter().position(Entry::settled) else {
                break;
            };
            let entry = self.entries.remove(oldest).unwrap();
            self.bytes -= entry.bytes;
        }
        while self.stopped.len() > ENDED_CHANNELS {
            let channel = self.stopped.pop_front().unwrap();
            self.channels.remove(&channel);
        }
        while self.fences.len() > FENCES {
            let oldest = *self
                .fences
                .iter()
                .min_by_key(|(_, fence)| fence.touched)
                .unwrap()
                .0;
            self.fences.remove(&oldest);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(n: u8) -> ChannelId {
        ChannelId::parse(&format!("5b0c0b1e-6f0a-4c1c-9d59-0e3f2b7a9c{n:02x}")).unwrap()
    }

    fn owner(action: u16, generation: u64) -> Owner {
        Owner {
            action: action_id(action),
            generation,
        }
    }

    fn action_id(n: u16) -> ActionId {
        ActionId::for_test(n)
    }

    fn result(success: bool) -> Value {
        json!({ "success": success, "steps": [], "browser": { "namespace": "n", "session": "browser",
            "capture": { "status": "not_requested" } } })
    }

    #[test]
    fn a_frame_is_running_until_it_settles_and_then_keeps_its_result() {
        let ledger = Ledger::default();
        let (c, a) = (channel(1), owner(1, 41));
        assert!(ledger.open(c));
        ledger.receive(c, 7, Some(Some(a)), false);
        assert_eq!(ledger.status(a.action, (c, 7)), Ok(Status::Running));
        assert!(ledger.start(c, 7, Some(a)));
        assert_eq!(ledger.status(a.action, (c, 7)), Ok(Status::Running));
        ledger.settle(c, 7, result(true));
        assert_eq!(
            ledger.status(a.action, (c, 7)),
            Ok(Status::Settled(result(true)))
        );
        assert_eq!(
            ledger.entries(a.action),
            vec![(c, 7, Status::Settled(result(true)))]
        );
        assert_eq!(
            Status::Settled(result(true)).to_json(),
            json!({ "state": "settled", "result": result(true) })
        );
    }

    #[test]
    fn a_frame_not_yet_received_runs_later_on_a_live_channel_and_never_on_an_ended_one() {
        let ledger = Ledger::default();
        let (c, a) = (channel(1), owner(1, 41));
        ledger.open(c);
        ledger.receive(c, 3, None, false);
        assert_eq!(ledger.status(a.action, (c, 9)), Ok(Status::Running));
        ledger.end(c);
        assert_eq!(ledger.status(a.action, (c, 9)), Ok(Status::NotStarted));
        // Received and not a sequence (or evicted): nothing can be said.
        assert_eq!(ledger.status(a.action, (c, 3)), Ok(Status::Unknown));
        // A channel this process never saw: it restarted, or the id is wrong.
        assert_eq!(
            ledger.status(a.action, (channel(9), 1)),
            Ok(Status::Unknown)
        );
    }

    #[test]
    fn frames_waiting_their_turn_when_the_connection_ends_never_start() {
        let ledger = Ledger::default();
        let (c, a) = (channel(1), owner(1, 41));
        ledger.open(c);
        ledger.receive(c, 7, Some(Some(a)), false);
        assert!(ledger.start(c, 7, Some(a)));
        ledger.receive(c, 8, Some(Some(a)), false);
        ledger.end(c);
        // The running frame completes and is recorded; the queued one is not.
        assert!(!ledger.may_continue(c));
        ledger.settle(c, 7, result(true));
        assert_eq!(ledger.status(a.action, (c, 8)), Ok(Status::NotStarted));
        assert!(!ledger.start(c, 8, Some(a)));
        assert_eq!(ledger.status(a.action, (c, 8)), Ok(Status::NotStarted));
        assert_eq!(
            ledger.status(a.action, (c, 7)),
            Ok(Status::Settled(result(true)))
        );
    }

    #[test]
    fn a_frame_refused_on_arrival_or_before_its_steps_is_recorded_not_started() {
        let ledger = Ledger::default();
        let (c, a) = (channel(1), owner(1, 41));
        ledger.open(c);
        ledger.receive(c, 7, Some(Some(a)), false);
        ledger.receive(c, 8, Some(Some(a)), true);
        assert_eq!(ledger.status(a.action, (c, 8)), Ok(Status::NotStarted));
        ledger.not_started(c, 7, Some(a));
        assert_eq!(ledger.status(a.action, (c, 7)), Ok(Status::NotStarted));
    }

    #[test]
    fn a_channel_id_is_opened_once() {
        let ledger = Ledger::default();
        assert!(ledger.open(channel(1)));
        assert!(!ledger.open(channel(1)));
        ledger.end(channel(1));
        assert!(!ledger.open(channel(1)));
    }

    #[test]
    fn an_older_owner_is_fenced_and_a_newer_one_stops_the_older_channels() {
        let ledger = Ledger::default();
        let (old, new, other) = (channel(1), channel(2), channel(3));
        for c in [old, new, other] {
            ledger.open(c);
        }
        assert_eq!(ledger.register(old, owner(1, 41)), Ok(()));
        assert_eq!(ledger.register(other, owner(2, 5)), Ok(()));
        ledger.receive(old, 7, Some(Some(owner(1, 41))), false);
        assert!(ledger.start(old, 7, Some(owner(1, 41))));
        ledger.receive(old, 8, Some(Some(owner(1, 41))), false);
        // The same generation again is the same owner.
        assert_eq!(ledger.register(old, owner(1, 41)), Ok(()));
        assert!(ledger.may_continue(old));
        // A newer owner reaches the daemon on another channel.
        assert_eq!(ledger.register(new, owner(1, 42)), Ok(()));
        assert!(!ledger.may_continue(old));
        assert!(ledger.may_continue(new));
        // Another Action's channel is untouched.
        assert!(ledger.may_continue(other));
        // What the old channel had queued never starts; its running frame
        // still settles.
        assert_eq!(
            ledger.status(owner(1, 42).action, (old, 8)),
            Ok(Status::NotStarted)
        );
        ledger.settle(old, 7, result(true));
        assert_eq!(
            ledger.status(owner(1, 42).action, (old, 7)),
            Ok(Status::Settled(result(true)))
        );
        // The old owner is refused from now on, on any channel.
        assert_eq!(ledger.register(old, owner(1, 41)), Err(Fenced));
        assert_eq!(ledger.register(other, owner(1, 41)), Err(Fenced));
        // The channel that raises the fence is never stopped by it, though
        // it carried the Action under the older generation before.
        assert_eq!(ledger.register(new, owner(1, 43)), Ok(()));
        assert!(ledger.may_continue(new));
        assert!(ledger.may_continue(other));
    }

    #[test]
    fn op_status_answers_only_for_the_asking_action() {
        let ledger = Ledger::default();
        let c = channel(1);
        ledger.open(c);
        ledger.receive(c, 7, Some(Some(owner(1, 41))), false);
        ledger.receive(c, 8, Some(None), false);
        assert_eq!(ledger.status(owner(2, 1).action, (c, 7)), Err(OtherAction));
        assert_eq!(ledger.status(owner(1, 41).action, (c, 8)), Err(OtherAction));
        assert_eq!(ledger.entries(owner(2, 1).action), vec![]);
        assert_eq!(
            ledger.entries(owner(1, 41).action),
            vec![(c, 7, Status::Running)]
        );
    }

    #[test]
    fn the_ledger_keeps_its_bounds_and_forgets_the_oldest_settled_outcomes_first() {
        let ledger = Ledger::default();
        let (c, a) = (channel(1), owner(1, 41));
        ledger.open(c);
        // The oldest frame is still running: it is never evicted.
        ledger.receive(c, 1, Some(Some(a)), false);
        ledger.start(c, 1, Some(a));
        for id in 2..=(ENTRIES as u64 + 5) {
            ledger.receive(c, id, Some(Some(a)), false);
            ledger.start(c, id, Some(a));
            ledger.settle(c, id, result(true));
        }
        let held = ledger.entries(a.action);
        assert_eq!(held.len(), ENTRIES);
        assert_eq!(held[0], (c, 1, Status::Running));
        assert_eq!(held[1].1, 7);
        assert_eq!(ledger.status(a.action, (c, 2)), Ok(Status::Unknown));
        assert_eq!(ledger.status(a.action, (c, 6)), Ok(Status::Unknown));

        // The byte bound: large outcomes push the old ones out.
        let big = json!({ "success": true, "steps": [], "browser": {},
            "pad": "x".repeat(3 << 20) });
        let bytes = Ledger::default();
        bytes.open(c);
        for id in 1..=6 {
            bytes.receive(c, id, Some(Some(a)), false);
            bytes.start(c, id, Some(a));
            bytes.settle(c, id, big.clone());
        }
        let held = bytes.entries(a.action);
        assert!(held.len() * (3 << 20) <= BYTES, "{}", held.len());
        assert_eq!(held.last().unwrap().1, 6);
        assert_eq!(bytes.status(a.action, (c, 1)), Ok(Status::Unknown));
    }

    #[test]
    fn fences_are_kept_for_the_most_recently_seen_actions() {
        let ledger = Ledger::default();
        let c = channel(1);
        ledger.open(c);
        ledger.register(c, owner(0, 10)).unwrap();
        for action in 1..=FENCES as u16 {
            ledger.register(c, owner(action, 1)).unwrap();
        }
        // Action 0 was the least recently seen: its fence is gone, so the
        // dispatcher alone fences its older owners from now on.
        assert_eq!(ledger.register(c, owner(0, 3)), Ok(()));
        // Action 1 is still fenced.
        ledger.register(c, owner(1, 9)).unwrap();
        assert_eq!(ledger.register(c, owner(1, 8)), Err(Fenced));
    }

    #[tokio::test]
    async fn a_waiting_op_status_answers_as_soon_as_the_frame_leaves_running() {
        let ledger = std::sync::Arc::new(Ledger::default());
        let (c, a) = (channel(1), owner(1, 41));
        ledger.open(c);
        ledger.receive(c, 7, Some(Some(a)), false);
        ledger.start(c, 7, Some(a));
        let started = std::time::Instant::now();
        let waiting = tokio::spawn({
            let ledger = ledger.clone();
            async move {
                ledger
                    .status_within(a.action, (c, 7), Duration::from_secs(10))
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        ledger.settle(c, 7, result(false));
        assert_eq!(waiting.await.unwrap(), Ok(Status::Settled(result(false))));
        assert!(started.elapsed() < Duration::from_secs(5));
        // Still running at the end of its wait: running.
        ledger.receive(c, 8, Some(Some(a)), false);
        let started = std::time::Instant::now();
        assert_eq!(
            ledger
                .status_within(a.action, (c, 8), Duration::from_millis(60))
                .await,
            Ok(Status::Running)
        );
        assert!(started.elapsed() >= Duration::from_millis(60));
        let entries = ledger
            .entries_within(a.action, Duration::from_millis(10))
            .await;
        assert_eq!(entries.len(), 2);
    }
}
