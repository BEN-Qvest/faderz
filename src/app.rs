//! What the REST routes and the WebSocket share: the replica, the
//! commands in flight, the fan-out of deltas, and the parts of server
//! messages every route sends.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use samplez_replica::Replica;
use samplez_replica::entry::{NodeId, SessionKey};
use samplez_replica::membership;
use samplez_replica::state_machine::StateMachine;
use samplez_service::config::AuthMode;
use samplez_service::metrics::{Counter, Metrics};
use samplez_service::replica::{Ack, Begin, Commands, ReplicaInfo, Resolution};
use samplez_service::ws::{Delta, Frame, Hub, Slot, To};
use samplez_service::{Identity, Owner, Position, ReplicatedState};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::{Instant, MissedTickBehavior};

use crate::audio::Audio;
use crate::config::Set;
use crate::state::{Answer, Command, Mixer};

thread_local! {
    /// Whether this thread is inside [`with_state`].
    static IN_STATE: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f` on the replicated state of `replica`. An entry arriving
/// meanwhile is applied only once `f` returns, so `f` takes what it needs
/// and does not call into the replica: [`App::info`] and
/// [`App::contact_metrics`] wait on the replica's own locks and would
/// hold applying off for as long. Debug builds assert that.
pub fn with_state<T>(replica: &Replica<Mixer>, f: impl FnOnce(&StateMachine<Mixer>) -> T) -> T {
    struct Leave(bool);
    impl Drop for Leave {
        fn drop(&mut self) {
            IN_STATE.set(self.0);
        }
    }
    let _leave = Leave(IN_STATE.replace(true));
    replica.store().with_state(f)
}

/// Asserts in debug builds that this thread is not inside
/// [`with_state`].
fn outside_state() {
    debug_assert!(
        !IN_STATE.get(),
        "a call into the replica inside App::with_state, which holds off applying entries"
    );
}

/// Deltas waiting per client before it is caught up by snapshot instead.
pub const CLIENT_DELTAS: usize = 1024;
/// Commands waiting for their answer on this replica at once, every
/// connection and REST together.
pub const COMMANDS_IN_FLIGHT: usize = 4096;

pub struct App {
    pub replica: Arc<Replica<Mixer>>,
    /// The replica's id in the contract (`ReplicaRef.id`).
    pub name: String,
    /// The id of each replica of the set by ordinal ([`member_names`]);
    /// empty for a set of one.
    members: Vec<String>,
    pub hub: Hub,
    pub commands: Commands,
    /// The revision of the last delta handed to the hub.
    published: watch::Sender<u64>,
    /// Each session's selection. Not replicated.
    selections: Mutex<BTreeMap<SessionKey, Selection>>,
    /// The sessions with a connection open on this replica.
    sessions: Mutex<BTreeMap<SessionKey, Present>>,
    /// What `presence` names a session without a display name.
    unnamed: &'static str,
    /// When each asking session last had a control request for a path
    /// forwarded.
    requests: Mutex<BTreeMap<(SessionKey, String), Instant>>,
    /// The engine's counters, inputs and loudness measurements last read,
    /// as the `values`, `inputs` and `loudness` of the `counters` message
    /// ([`App::count`]); none without an engine.
    counters: Mutex<Option<Value>>,
    /// The engine's inputs and outputs last read ([`App::count`]); none
    /// without an engine.
    engine: Mutex<Option<Value>>,
    /// When each follower last matched all of this leader's log, as
    /// [`replicas`](App::replicas) saw it; read without an engine only.
    caught_up: Mutex<BTreeMap<NodeId, Instant>>,
    /// When [`replicas`](App::replicas) last heard each other replica.
    heard: Mutex<BTreeMap<NodeId, Instant>>,
    /// The contact window, `contactWindowMs`, that contact runs with.
    contact_window: Duration,
    /// The tallies this replica took whose transition starts before it
    /// took them, as the `counters` message's `afv_late_total`.
    pub afv_late: Counter,
}

/// A connection counted by [`App::connected`]; dropping it counts it
/// closed.
pub struct Connected<'a> {
    app: &'a App,
    session: SessionKey,
}

impl Drop for Connected<'_> {
    fn drop(&mut self) {
        self.app.disconnected(&self.session);
    }
}

/// A session with a connection open on this replica.
struct Present {
    /// Its connections open on this replica.
    open: usize,
    /// Its presence record without the times, from the latest hello; none
    /// before the first.
    record: Option<Value>,
    /// When its first open connection opened.
    since: SystemTime,
    /// When it last sent a command, a selection or a control request, or
    /// opened its first connection.
    active: SystemTime,
}

/// A session's selection.
struct Selection {
    /// The owner record of the connection that made it.
    owner: Value,
    element: String,
    /// When it was made.
    at: Instant,
    /// Whether its session has held a lease since.
    leased: bool,
}

/// The `replicas` message: the latest goes out on every change.
pub const REPLICAS: Slot = Slot {
    name: "replicas",
    every: Duration::ZERO,
};
/// The `selections` message: at most one per connection every 100 ms.
pub const SELECTIONS: Slot = Slot {
    name: "selections",
    every: Duration::from_millis(100),
};
/// The `presence` message: at most one per connection every 100 ms.
pub const PRESENCE: Slot = Slot {
    name: "presence",
    every: Duration::from_millis(100),
};
/// How often [`App::count`] reads and sends the engine's counters.
pub const COUNTERS_PERIOD: Duration = Duration::from_secs(1);
/// How long a selection of a session that has not held a lease is kept:
/// one lease.
const SELECTION_GRACE: Duration = samplez_replica::session::LEASE;
/// A session's requests for one control forwarded at most this often.
pub const REQUEST_INTERVAL: Duration = Duration::from_secs(3);
/// How long after the asker's replica took a control request another
/// replica still hands it to the holder.
const REQUEST_LATE: Duration = Duration::from_secs(10);

/// The id of each replica of `set` by ordinal: `name` for this one, and
/// for the others the StatefulSet's name and their ordinal, as its pods
/// are named, or, without a StatefulSet, their peer address as listed
/// (`host:port`, unique by validation). Entry n is the replica of
/// ordinal n, as `peers[n]` is its address.
pub fn member_names(set: &Set, name: &str) -> Vec<String> {
    (set.peers.iter().enumerate())
        .map(|(n, addr)| match &set.statefulset {
            _ if n == usize::from(set.ordinal) => name.to_owned(),
            Some(statefulset) => format!("{statefulset}-{n}"),
            None => addr.clone(),
        })
        .collect()
}

/// How many log entries a follower may trail the leader's log by and
/// still count as heard: replication runs in batches, so an exact match
/// would flap during a burst of writes.
const REPLICATION_LAG: u64 = 16;

/// How long ago a follower may last have matched the leader's whole log
/// and still count as heard: a live one does between batches of writes,
/// a stopped one never does again once the log moves on.
const CAUGHT_UP_WINDOW: Duration = Duration::from_secs(3);

/// How long another replica goes unheard before it is listed down: the
/// contact layer holds a peer silent after a few milliseconds without a
/// report, which a scheduling or network hiccup gives a live one.
const DOWN_AFTER: Duration = Duration::from_secs(1);

/// Whether a replica heard now (`heard_now`) or within [`DOWN_AFTER`]
/// before `now` is up; `heard` holds when each was last heard.
fn up_within(
    id: NodeId,
    heard_now: bool,
    heard: &mut BTreeMap<NodeId, Instant>,
    now: Instant,
) -> bool {
    if heard_now {
        heard.insert(id, now);
    }
    heard
        .get(&id)
        .is_some_and(|&t| now.duration_since(t) < DOWN_AFTER)
}

/// Whether a follower that has matched `matched` of a leader whose log
/// ends at `last` is within [`REPLICATION_LAG`] of it.
fn within_lag(matched: Option<u64>, last: Option<u64>) -> bool {
    match (matched, last) {
        (Some(matched), Some(last)) => matched.saturating_add(REPLICATION_LAG) >= last,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// The ordinal of an audio replica's node id; a witness sorts last.
fn ordinal_of(id: NodeId) -> u16 {
    match membership::slot_of(id) {
        membership::Slot::Audio(ordinal) => ordinal,
        membership::Slot::Witness => u16::MAX,
    }
}

/// Model `Build`: the faderz version and the samplez release it was built
/// against, as build.rs read it from Cargo.lock.
pub fn build() -> Value {
    json!({ "faderz": env!("CARGO_PKG_VERSION"), "samplez": env!("FADERZ_SAMPLEZ") })
}

/// The faderz CHANGELOG.md of the release this binary was built from,
/// embedded at build time: `GET /about` serves it as it is.
pub const CHANGELOG: &str = include_str!("../CHANGELOG.md");

/// Whether replica `me` hears replica `id` of its set. With contact
/// running: contact holds a majority and has had a report from `id`
/// directly within its window, `window`. Not whether contact holds `id`
/// silent: for one window after contact is regained no peer is silent,
/// one stopped long before included. Without (a set without an engine),
/// what raft shows: a follower hears the leader it follows, for as long
/// as raft names one (it drops the name when the election timeout passes
/// without a heartbeat); the leader hears a follower whose matched log is
/// within [`REPLICATION_LAG`] entries of its own and that has matched all
/// of it within [`CAUGHT_UP_WINDOW`] (`caught_up` holds when it last
/// did).
fn hears(
    id: NodeId,
    me: NodeId,
    contact: Option<&samplez_replica::contact::Metrics>,
    window: Duration,
    metrics: &openraft::RaftMetrics<NodeId, openraft::BasicNode>,
    caught_up: &mut BTreeMap<NodeId, Instant>,
    now: Instant,
) -> bool {
    if let Some(contact) = contact {
        return contact.in_contact
            && (contact.peers.iter())
                .any(|p| p.node == id && p.last_report_age.is_some_and(|age| age <= window));
    }
    if metrics.current_leader == Some(me) {
        let matched = (metrics.replication.as_ref())
            .and_then(|r| r.get(&id).copied().flatten())
            .map(|log| log.index);
        if matched.is_some() && matched == metrics.last_log_index {
            caught_up.insert(id, now);
        }
        return within_lag(matched, metrics.last_log_index)
            && (caught_up.get(&id)).is_some_and(|&t| now.duration_since(t) < CAUGHT_UP_WINDOW);
    }
    metrics.current_leader == Some(id)
}

/// Each voter as the replicas list shows it, and whether a majority of
/// the voters is up now: replica `me` while `ready`, another while heard
/// now (`hears`). The list shows another one up while heard now or
/// within [`DOWN_AFTER`]; the majority leaves that grace out, as `me`'s
/// own commitable follows it and a client fails over on that.
fn listing(
    voters: &[NodeId],
    me: NodeId,
    ready: bool,
    heard: &mut BTreeMap<NodeId, Instant>,
    now: Instant,
    mut hears: impl FnMut(NodeId) -> bool,
) -> (BTreeMap<NodeId, bool>, bool) {
    let mut up = BTreeMap::new();
    let mut heard_now = 0;
    for &id in voters {
        let (now_up, listed) = if id == me {
            (ready, ready)
        } else {
            let h = hears(id);
            (h, up_within(id, h, heard, now))
        };
        heard_now += usize::from(now_up);
        up.insert(id, listed);
    }
    (up, 2 * heard_now > voters.len())
}

impl App {
    pub fn new(
        replica: Arc<Replica<Mixer>>,
        name: String,
        set: Option<&Set>,
        contact_window: Duration,
        auth: AuthMode,
        metrics: &Metrics,
    ) -> Arc<App> {
        let published = watch::Sender::new(replica.revision());
        Arc::new(App {
            commands: Commands::new(replica.clone(), COMMANDS_IN_FLIGHT),
            replica,
            members: set.map_or_else(Vec::new, |set| member_names(set, &name)),
            name,
            hub: Hub::new(CLIENT_DELTAS, metrics),
            published,
            selections: Mutex::default(),
            sessions: Mutex::default(),
            unnamed: if auth == AuthMode::Token {
                "token"
            } else {
                "anonymous"
            },
            requests: Mutex::default(),
            counters: Mutex::default(),
            engine: Mutex::default(),
            caught_up: Mutex::default(),
            heard: Mutex::default(),
            contact_window,
            afv_late: metrics.counter(
                "faderz_afv_late_total",
                "Tallies taken after the sample index their transition starts at.",
                &[],
            ),
        })
    }

    pub fn epoch(&self) -> u64 {
        self.replica.epoch()
    }

    /// [`with_state`] on this app's replica.
    pub fn with_state<T>(&self, f: impl FnOnce(&StateMachine<Mixer>) -> T) -> T {
        with_state(&self.replica, f)
    }

    /// The replica's own view of itself; takes the replica's locks.
    fn info(&self) -> ReplicaInfo {
        outside_state();
        self.replica.replica()
    }

    /// The audio replica's contact metrics; takes the replica's locks.
    fn contact_metrics(&self) -> Option<samplez_replica::contact::Metrics> {
        outside_state();
        self.replica.contact_metrics()
    }

    /// Model `ReplicaRef`: the replica's role in the contract is leader or
    /// follower; openraft's other states are not leading.
    pub fn replica_ref(&self) -> Value {
        let role = if self.info().role == "leader" {
            "leader"
        } else {
            "follower"
        };
        json!({ "id": self.name, "role": role })
    }

    /// Model `ReplicaStatus` of every replica of the set as this one sees
    /// it, by ordinal: itself as [`ReplicatedState::replica`] reports it,
    /// and each other voter that serves clients (not a witness) by what
    /// this replica hears of it ([`hears`]). The leader is the one
    /// raft follows. Another replica can commit, as far as this one can
    /// tell, while it is heard and a majority of the voters is: this
    /// replica counting itself when it is ready. Every entry, this
    /// replica's own included, is commitable by that one rule.
    pub fn replicas(&self) -> Value {
        let info = self.info();
        let mut own = self.replica_ref();
        own["healthy"] = json!(info.ready);
        own["commitable"] = json!(info.commitable);
        if self.members.is_empty() {
            return json!([own]);
        }
        let metrics = self.replica.raft().metrics().borrow().clone();
        let contact = self.contact_metrics();
        let me = self.replica.id();
        let voters: Vec<NodeId> = metrics.membership_config.membership().voter_ids().collect();
        let now = Instant::now();
        let mut caught_up = self.caught_up.lock().expect("caught_up lock poisoned");
        let mut heard = self.heard.lock().expect("heard lock poisoned");
        // Heard now, and listed: a replica unheard for less than
        // [`DOWN_AFTER`] is listed up, but the quorum, which this replica's
        // own commitable and so a client's failover follow, counts what is
        // heard now.
        let (up, quorum) = listing(&voters, me, info.ready, &mut heard, now, |id| {
            hears(
                id,
                me,
                contact.as_ref(),
                self.contact_window,
                &metrics,
                &mut caught_up,
                now,
            )
        });
        let mut listed: Vec<(u16, Value)> = Vec::new();
        for &id in &voters {
            let membership::Slot::Audio(ordinal) = membership::slot_of(id) else {
                continue;
            };
            let healthy = up[&id];
            let status = if id == me {
                own["commitable"] = json!(healthy && quorum);
                own.clone()
            } else {
                let Some(name) = self.members.get(usize::from(ordinal)) else {
                    continue;
                };
                let role = if metrics.current_leader == Some(id) {
                    "leader"
                } else {
                    "follower"
                };
                json!({ "id": name, "role": role, "healthy": healthy, "commitable": healthy && quorum })
            };
            listed.push((ordinal, status));
        }
        // Not yet a voter itself, as while it joins: listed all the same.
        if !voters.contains(&me) {
            listed.push((ordinal_of(me), own));
        }
        listed.sort_by_key(|(ordinal, _)| *ordinal);
        Value::Array(listed.into_iter().map(|(_, s)| s).collect())
    }

    /// The `replicas` message.
    pub fn replicas_message(&self) -> String {
        json!({
            "type": "replicas",
            "replica": self.replica_ref(),
            "build": build(),
            "replicas": self.replicas(),
        })
        .to_string()
    }

    /// The `delta` message at `revision` of a delta the state machine
    /// keeps without them ([`samplez_replica::state_machine::Product::delta`]).
    pub fn delta_message(&self, revision: u64, body: &str) -> String {
        splice(
            &format!(
                r#"{{"type":"delta","replica":{},"revision":{revision}"#,
                self.replica_ref()
            ),
            body,
        )
    }

    /// What a command that resolved as `ack` got beside it, from the
    /// state's table of refusals and stored scenes.
    pub fn answer(&self, session: &SessionKey, ack: &Ack) -> Option<Answer> {
        match ack.resolution {
            Resolution::Applied | Resolution::Unchanged | Resolution::Duplicate => {
                self.with_state(|sm| sm.product.answer(session, &ack.command).cloned())
            }
            _ => None,
        }
    }

    /// The `atIndex` of the delta at `revision`, while this replica holds
    /// it.
    pub async fn at_index(&self, revision: u64) -> Option<u64> {
        let since = Position {
            epoch: self.epoch(),
            revision: revision.checked_sub(1)?,
        };
        let deltas = self.replica.deltas_since(since).await?;
        let delta = deltas.into_iter().find(|d| d.revision == revision)?;
        serde_json::from_str::<Value>(&delta.text).ok()?["atIndex"].as_u64()
    }

    /// The `counters` message of the engine's counters, inputs and
    /// loudness measurements last read, with the tallies taken late
    /// ([`App::afv_late`]); none before the first read or without an
    /// engine.
    pub fn counters_message(&self) -> Option<String> {
        let read = self
            .counters
            .lock()
            .expect("counters lock poisoned")
            .clone()?;
        let mut values = read["values"].clone();
        values["afv_late_total"] = json!(self.afv_late.get());
        let m = json!({
            "type": "counters",
            "replica": self.replica_ref(),
            "values": values,
            "inputs": read["inputs"],
            "loudness": read["loudness"],
        });
        Some(m.to_string())
    }

    /// The model `Status` of `GET /status`: this replica as the replicated
    /// state sees it, its audio replica's state in contact, and the
    /// engine's inputs and outputs as last read.
    pub fn status(&self) -> Value {
        use samplez_replica::state::ReplicaState as S;
        let info = self.info();
        let contact = self.contact_metrics();
        let in_contact = contact.as_ref().map(|m| m.in_contact);
        // None for a replica without an engine, and until the first step.
        let state = (contact.and_then(|m| m.replica?.state)).map(|s| match s {
            S::Starting => "starting",
            S::Committing => "committing",
            S::Withdrawn => "withdrawn",
            S::Diverged => "diverged",
            S::VersionMismatch => "versionMismatch",
            S::Resync => "resync",
        });
        json!({
            "replica": {
                "id": self.name,
                "node": self.replica.id().to_string(),
                "role": info.role,
                "leader": info.leader,
                "inSync": info.in_sync,
                "ready": info.ready,
                "commitable": info.commitable,
                "inContact": in_contact,
                "state": state,
            },
            "engine": self.engine.lock().expect("engine lock poisoned").clone(),
        })
    }

    /// Reads the engine's counters every [`COUNTERS_PERIOD`] and sends
    /// them to every connection, standby ones included, so a promoted
    /// standby has them at once; a connection not sent the last yet gets
    /// only the newest. Never returns.
    pub async fn count(&self, audio: &Audio) {
        let mut ticks = tokio::time::interval(COUNTERS_PERIOD);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            ticks.tick().await;
            let (counters, engine) = audio.readings();
            *self.counters.lock().expect("counters lock poisoned") = Some(counters);
            *self.engine.lock().expect("engine lock poisoned") = Some(engine);
            if let Some(m) = self.counters_message() {
                self.hub.publish(Frame::Counters(m.into()));
            }
        }
    }

    /// Publishes every delta to the clients once this replica applied
    /// it, with this replica's ref, and a `replicas` message whenever
    /// what it says changes. Never returns.
    pub async fn publish(&self) {
        let mut last = self.replica.revision();
        let mut replicas = self.replicas_message();
        loop {
            tokio::select! {
                () = self.replica.applied(last + 1) => {
                    let since = Position { epoch: self.epoch(), revision: last };
                    match self.replica.deltas_since(since).await {
                        Some(deltas) => {
                            for d in deltas {
                                last = d.revision;
                                let body = self
                                    .deliver_requests(&d.text)
                                    .unwrap_or_else(|| d.text.to_string());
                                let text = self.delta_message(d.revision, &body);
                                self.hub.publish(Frame::Delta(Delta { text: text.into(), ..d }));
                            }
                            self.published.send_replace(last);
                        }
                        // Dropped from the deltas held: clients catch up
                        // at the next delta, whose prev they did not see.
                        None => {
                            last = self.replica.revision();
                            self.published.send_replace(last);
                        }
                    }
                }
                () = tokio::time::sleep(Duration::from_millis(250)) => {
                    let now = self.replicas_message();
                    if now != replicas {
                        self.hub.publish(Frame::Status(REPLICAS, now.clone().into()));
                        replicas = now;
                    }
                    self.lapse_selections();
                }
            }
        }
    }
}

impl App {
    /// The `selections` message.
    pub fn selections_message(&self) -> String {
        let list: Vec<Value> = self
            .selections
            .lock()
            .expect("selections lock poisoned")
            .values()
            .map(|s| json!({ "session": s.owner, "element": s.element }))
            .collect();
        json!({ "type": "selections", "replica": self.replica_ref(), "selections": list })
            .to_string()
    }

    /// Sets or clears the selection of `session`, `owner` its owner
    /// record, and tells every connection when that changed it. An
    /// element the shape has no strip or master for is ignored.
    pub fn select(&self, session: SessionKey, owner: Value, element: Option<&str>) {
        let known = element.is_none_or(|e| {
            self.with_state(|sm| {
                let shape = sm.product.shape();
                shape.is_channel(e) || shape.is_output_bus(e)
            })
        });
        if !known {
            return;
        }
        let leased = self.with_state(|sm| sm.leases.contains_key(&session));
        let changed = {
            let mut selections = self.selections.lock().expect("selections lock poisoned");
            match element {
                Some(e) if selections.get(&session).is_none_or(|s| s.element != e) => {
                    let selection = Selection {
                        owner,
                        element: e.to_owned(),
                        at: Instant::now(),
                        leased,
                    };
                    selections.insert(session, selection);
                    true
                }
                None => selections.remove(&session).is_some(),
                Some(_) => false,
            }
        };
        if changed {
            self.hub
                .publish(Frame::Status(SELECTIONS, self.selections_message().into()));
        }
    }

    /// Counts a connection of `session` opened on this replica until the
    /// guard returned is dropped, as it also is by a panic or a cancelled
    /// serve.
    pub fn connected(&self, session: SessionKey) -> Connected<'_> {
        let mut sessions = self.sessions.lock().expect("sessions lock poisoned");
        let now = SystemTime::now();
        sessions
            .entry(session.clone())
            .or_insert(Present {
                open: 0,
                record: None,
                since: now,
                active: now,
            })
            .open += 1;
        Connected { app: self, session }
    }

    /// Counts a connection of `session` closed, and drops the session's
    /// selection and presence with its last one.
    fn disconnected(&self, session: &SessionKey) {
        let gone = {
            let mut sessions = self.sessions.lock().expect("sessions lock poisoned");
            let present = sessions.get_mut(session).expect("counted when opened");
            present.open -= 1;
            if present.open == 0 {
                sessions.remove(session)
            } else {
                None
            }
        };
        if let Some(gone) = gone {
            self.select(session.clone(), Value::Null, None);
            if gone.record.is_some() {
                self.publish_presence();
            }
        }
    }

    /// Records what the hello of a connection of `session` said: its
    /// owner record `owner` and `identity`. The session is listed in
    /// `presence` from its first hello on.
    pub fn greeted(&self, session: &SessionKey, owner: &Value, identity: &Identity) {
        let record = json!({
            "handle": owner["handle"],
            "kind": owner["kind"],
            "user": identity.name.as_deref().unwrap_or(self.unnamed),
            "role": identity.role,
        });
        let changed = {
            let mut sessions = self.sessions.lock().expect("sessions lock poisoned");
            let Some(present) = sessions.get_mut(session) else {
                return;
            };
            present.record.as_ref() != Some(&record) && {
                present.record = Some(record);
                true
            }
        };
        if changed {
            self.publish_presence();
        }
    }

    /// Records activity of `session`: a command, a selection or a control
    /// request. `presence` carries it to the second, so it goes out again
    /// at most once a second per session.
    pub fn active(&self, session: &SessionKey) {
        let changed = {
            let mut sessions = self.sessions.lock().expect("sessions lock poisoned");
            let Some(present) = sessions.get_mut(session) else {
                return;
            };
            let now = SystemTime::now();
            let second = |t: SystemTime| t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
            let changed = second(now) != second(present.active);
            present.active = now;
            changed && present.record.is_some()
        };
        if changed {
            self.publish_presence();
        }
    }

    /// The `presence` message: the sessions connected to this replica that
    /// have sent a hello, in the order they connected.
    pub fn presence_message(&self) -> String {
        let mut list: Vec<(SystemTime, Value)> = self
            .sessions
            .lock()
            .expect("sessions lock poisoned")
            .values()
            .filter_map(|p| {
                let mut record = p.record.clone()?;
                record["since"] = json!(rfc3339(p.since));
                record["active"] = json!(rfc3339(p.active));
                Some((p.since, record))
            })
            .collect();
        list.sort_by_key(|(since, _)| *since);
        let sessions: Vec<Value> = list.into_iter().map(|(_, r)| r).collect();
        json!({ "type": "presence", "replica": self.replica_ref(), "sessions": sessions })
            .to_string()
    }

    fn publish_presence(&self) {
        self.hub
            .publish(Frame::Status(PRESENCE, self.presence_message().into()));
    }

    /// Drops the selections of sessions whose lease has ended. A session
    /// gets its first lease at samplez's next renewal of the live sessions,
    /// about a second after it connected, so one that has not held a lease
    /// yet keeps its selection for one lease from when it made it.
    fn lapse_selections(&self) {
        let leases = self.with_state(|sm| sm.leases.clone());
        let lapsed = {
            let mut selections = self.selections.lock().expect("selections lock poisoned");
            let before = selections.len();
            selections.retain(|session, s| {
                if leases.contains_key(session) {
                    s.leased = true;
                    return true;
                }
                !s.leased && s.at.elapsed() < SELECTION_GRACE
            });
            selections.len() != before
        };
        if lapsed {
            self.hub
                .publish(Frame::Status(SELECTIONS, self.selections_message().into()));
        }
    }

    /// Orders `from`'s request for `path` into the replicated log, where
    /// every replica applies it after the delta that made the holder the
    /// owner, so the request never reaches a client ahead of the
    /// ownership it is about, and the replica the holder's primary
    /// connection is on hands it to it ([`App::publish`]), whichever
    /// replica the asker is on. Nothing when nobody else owns it, or
    /// when `from` asked for it within [`REQUEST_INTERVAL`].
    pub async fn request(&self, from: Owner, asker: Value, path: String) {
        let key = SessionKey::from(&from);
        let held = self.with_state(|sm| sm.product.owner_of(&path));
        if held.is_none_or(|holder| holder == key) {
            return;
        }
        {
            let mut requests = self.requests.lock().expect("requests lock poisoned");
            let now = Instant::now();
            requests.retain(|_, at| now.duration_since(*at) < REQUEST_INTERVAL);
            if requests.insert((key, path.clone()), now).is_some() {
                return;
            }
        }
        let Ok(id) = uuid() else { return };
        let at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
        let command = Command::ControlRequest {
            id: id.clone(),
            path,
            from: asker,
            at,
        };
        if let Begin::New(pending) = self.commands.begin(&from, &id) {
            let body = serde_json::to_vec(&command).expect("a command serializes");
            let _ = pending.submit(body.into()).await;
        }
    }

    /// Hands the control requests a delta carries to the primary
    /// connections of their holders on this replica, and returns the
    /// delta the clients see, without them.
    fn deliver_requests(&self, text: &str) -> Option<String> {
        if !text.contains(r#""requests""#) {
            return None;
        }
        let mut delta: Value = serde_json::from_str(text).ok()?;
        let requests = delta.as_object_mut()?.remove("requests")?;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(0));
        for r in requests.as_array().into_iter().flatten() {
            // One a lagging replica applies late is not handed on: the
            // holder is asked for a control it may have let go long since.
            let asked = r["at"].as_u64().unwrap_or(0);
            if now.saturating_sub(asked) > u64::try_from(REQUEST_LATE.as_millis()).unwrap_or(0) {
                continue;
            }
            let (Some(principal), Some(session)) =
                (r["to"]["principal"].as_str(), r["to"]["session"].as_str())
            else {
                continue;
            };
            let message = json!({
                "type": "controlRequest",
                "replica": self.replica_ref(),
                "path": r["path"],
                "from": r["from"],
            })
            .to_string();
            let to = To::Primary { principal, session };
            self.hub.send(to, Frame::Text(message.into()));
        }
        Some(delta.to_string())
    }
}

/// `prefix` (an object's opening and first members) followed by the
/// members of the JSON object `object`.
pub fn splice(prefix: &str, object: &str) -> String {
    let members = object.trim().strip_prefix('{').unwrap_or("}");
    if members.trim_start().starts_with('}') {
        format!("{prefix}}}")
    } else {
        format!("{prefix},{members}")
    }
}

/// The current time as RFC 3339 UTC with milliseconds.
pub fn now_rfc3339() -> String {
    rfc3339(SystemTime::now())
}

/// `at` as RFC 3339 UTC with milliseconds.
pub fn rfc3339(at: SystemTime) -> String {
    let now = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs();
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Civil date from days since 1970-01-01 (H. Hinnant, chrono-compatible).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60,
        now.subsec_millis()
    )
}

/// A random (version 4) UUID, for commands the server issues itself.
///
/// # Errors
///
/// When the operating system's random source fails.
pub fn uuid() -> std::io::Result<String> {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&crate::replica::random()?.to_le_bytes());
    b[8..].copy_from_slice(&crate::replica::random()?.to_le_bytes());
    b[6] = b[6] & 0x0f | 0x40;
    b[8] = b[8] & 0x3f | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

#[cfg(test)]
pub(crate) mod tests;
