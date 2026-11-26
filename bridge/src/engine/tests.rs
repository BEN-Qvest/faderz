use super::*;

pub(crate) const SESSION: &str = "c2Vzc2lvbi1vZi10aGUtYnJpZGdlLXRlc3Qtc3VpdGU";

pub(crate) fn engine(endpoints: usize) -> Engine {
    let mut n = 0;
    Engine::new(
        endpoints,
        SESSION.into(),
        1_000,
        Box::new(move || {
            n += 1;
            format!("00000000-0000-4000-8000-{n:012}")
        }),
    )
}

pub(crate) fn sent(actions: &[Action], link: LinkId) -> Vec<Value> {
    actions
        .iter()
        .filter_map(|a| match a {
            Action::Send { link: l, text } if *l == link => serde_json::from_str(text).ok(),
            _ => None,
        })
        .collect()
}

pub(crate) fn snapshot(replica: &str, revision: u64) -> String {
    json!({
        "type": "snapshot", "replica": { "id": replica, "role": "leader" },
        "you": {}, "epoch": "1", "revision": revision,
        "shape": { "channels": [{ "id": "ch1", "name": "Mic 1" }], "buses": [] },
        "state": { "channels": { "ch1": { "fader": -90, "mute": false, "solo": "off" } }, "consoles": [] },
        "owners": {},
    })
    .to_string()
}

fn replicas(replica: &str, commitable: bool) -> String {
    json!({ "type": "replicas", "replica": { "id": replica, "role": "leader" },
        "replicas": [{ "id": replica, "role": "leader", "healthy": true, "commitable": commitable }] })
    .to_string()
}

/// An engine with a live primary on link 1 at t = 0.
pub(crate) fn live(endpoints: usize) -> Engine {
    let mut e = engine(endpoints);
    e.start(0);
    e.opened(1, 0);
    e.received(1, &snapshot("r1", 5), 0);
    e.take_actions();
    e.take_events();
    e
}

#[test]
fn the_hello_names_a_console_and_its_position_once_it_has_state() {
    let mut e = engine(1);
    e.start(0);
    assert_eq!(
        e.take_actions(),
        vec![Action::Connect {
            link: 1,
            endpoint: 0
        }]
    );
    e.opened(1, 10);
    let hello = &sent(&e.take_actions(), 1)[0];
    assert_eq!(hello["kind"], "console");
    assert_eq!(hello["mode"], "primary");
    assert_eq!(hello["session"], SESSION);
    assert!(hello.get("lastRevision").is_none());
    e.received(1, &snapshot("r1", 5), 20);
    assert_eq!(e.take_events(), vec![Event::Synced]);
    // Lost and reconnected: the hello carries where it was.
    e.closed(1, 30);
    e.tick(30 + PRIMARY_RETRY_MS[1]);
    let Some(Action::Connect { link, .. }) = e
        .take_actions()
        .into_iter()
        .find(|a| matches!(a, Action::Connect { .. }))
    else {
        panic!("reconnects");
    };
    e.opened(link, 400);
    let hello = &sent(&e.take_actions(), link)[0];
    assert_eq!(hello["lastRevision"], 5);
    assert_eq!(hello["epoch"], "1");
}

#[test]
fn continuous_writes_are_coalesced_to_fifty_per_second_latest_wins() {
    let mut e = live(1);
    let path = "/channels/ch1/fader";
    e.set(path, json!(-10), 100);
    e.set(path, json!(-9), 105);
    e.set(path, json!(-8), 110);
    let first = sent(&e.take_actions(), 1);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0]["value"], -10);
    assert_eq!(e.display(path), Some(&json!(-8)));
    assert_eq!(e.next_deadline(), 120);
    e.tick(120);
    let second = sent(&e.take_actions(), 1);
    assert_eq!(second.len(), 1);
    assert_eq!(second[0]["value"], -8);
    assert!(second[0]["seq"].as_u64() > first[0]["seq"].as_u64());
    // Discrete controls go out one by one.
    e.set("/channels/ch1/mute", json!(true), 121);
    e.set("/channels/ch1/mute", json!(false), 122);
    assert_eq!(sent(&e.take_actions(), 1).len(), 2);
}

#[test]
fn seqs_start_above_the_start_time_and_grow_per_control() {
    let mut e = live(1);
    e.touch("/channels/ch1/fader", 1);
    e.release("/channels/ch1/fader", 2);
    e.touch("/channels/ch1/mute", 3);
    let s: Vec<u64> = sent(&e.take_actions(), 1)
        .iter()
        .map(|m| m["seq"].as_u64().unwrap())
        .collect();
    assert_eq!(s, vec![1001, 1002, 1001]);
}

#[test]
fn a_503_sends_the_command_again_with_its_id_and_a_409_is_refused() {
    let mut e = live(1);
    e.touch("/channels/ch1/fader", 0);
    let touch = sent(&e.take_actions(), 1).remove(0);
    let error = json!({ "type": "error", "replica": { "id": "r1", "role": "leader" }, "code": 503,
        "message": "not applied", "commandId": touch["id"], "path": touch["path"] });
    e.received(1, &error.to_string(), 10);
    e.tick(10 + NOT_APPLIED_RETRY_MS - 1);
    assert!(
        sent(&e.take_actions(), 1)
            .iter()
            .all(|m| m["type"] == "ping")
    );
    e.tick(10 + NOT_APPLIED_RETRY_MS);
    let again: Vec<Value> = sent(&e.take_actions(), 1)
        .into_iter()
        .filter(|m| m["type"] == "touch")
        .collect();
    assert_eq!(again, vec![touch.clone()]);
    let owner = json!({ "principal": "ana", "handle": "aaaaaaaaaaaaaaaaaaaaaa", "kind": "ui", "connection": "c" });
    let conflict = json!({ "type": "error", "replica": { "id": "r1", "role": "leader" }, "code": 409,
        "message": "owned by another session", "commandId": touch["id"], "path": touch["path"],
        "current": -90, "owner": owner });
    e.received(1, &conflict.to_string(), 300);
    assert_eq!(
        e.take_events(),
        vec![Event::Refused {
            path: "/channels/ch1/fader".into(),
            owner
        }]
    );
    assert!(!e.touch_pending("/channels/ch1/fader"));
}

#[test]
fn a_delta_listing_a_command_answers_it_and_the_applied_value_shows() {
    let mut e = live(1);
    let path = "/channels/ch1/fader";
    e.set(path, json!(-5), 0);
    let set = sent(&e.take_actions(), 1).remove(0);
    assert_eq!(e.display(path), Some(&json!(-5)));
    let delta = json!({ "type": "delta", "replica": { "id": "r1", "role": "leader" }, "revision": 6,
        "atIndex": 1, "changes": [{ "path": path, "value": -5 }],
        "origins": [{ "owner": { "handle": e.handle() }, "commandId": set["id"], "seq": set["seq"] }] });
    e.received(1, &delta.to_string(), 10);
    assert_eq!(e.mirror().revision, 6);
    assert!(e.local.is_empty());
    assert!(e.outbox.is_empty());
}

#[test]
fn a_command_unanswered_for_3_s_is_dropped_and_its_value_reverts() {
    let mut e = live(1);
    e.set("/channels/ch1/mute", json!(true), 0);
    assert_eq!(e.display("/channels/ch1/mute"), Some(&json!(true)));
    e.tick(COMMAND_EXPIRY_MS);
    assert!(e.outbox.is_empty());
    assert_eq!(e.display("/channels/ch1/mute"), Some(&json!(false)));
}

#[test]
fn a_revision_gap_asks_for_a_resync() {
    let mut e = live(1);
    let delta = json!({ "type": "delta", "replica": { "id": "r1", "role": "leader" }, "revision": 7,
        "owners": [{ "path": "/channels/ch1/fader", "owner": null }] });
    e.received(1, &delta.to_string(), 0);
    let resync = sent(&e.take_actions(), 1).remove(0);
    assert_eq!(
        resync,
        json!({ "type": "resync", "lastRevision": 5, "epoch": "1" })
    );
}

#[test]
fn a_silent_primary_is_dead_after_750_ms_and_the_synced_standby_is_promoted() {
    let mut e = live(2);
    // The standby opens on the other endpoint once the primary is live.
    assert_eq!(e.standby().map(|s| s.endpoint), Some(1));
    let sb = e.standby().unwrap().id;
    e.take_actions();
    e.opened(sb, 5);
    let hello = sent(&e.take_actions(), sb).remove(0);
    assert_eq!(hello["mode"], "standby");
    assert_eq!(hello["lastRevision"], 5);
    e.received(
        sb,
        &json!({ "type": "resume", "replica": { "id": "r2", "role": "follower" },
        "you": {}, "epoch": "1", "revision": 5, "deltas": [] })
        .to_string(),
        6,
    );
    e.touch("/channels/ch1/fader", 7);
    let touch = sent(&e.take_actions(), 1).remove(0);
    // Both links keep beating; only the primary goes silent.
    let mut t = 0;
    while t < DEAD_MS {
        t += 50;
        e.received(
            sb,
            &json!({ "type": "ping", "replica": { "id": "r2", "role": "follower" }, "t": t })
                .to_string(),
            t,
        );
        e.tick(t);
    }
    let actions = e.take_actions();
    assert!(actions.contains(&Action::Close { link: 1 }));
    let on_standby = sent(&actions, sb);
    let types: Vec<&str> = on_standby
        .iter()
        .filter_map(|m| m["type"].as_str())
        .filter(|t| *t != "pong" && *t != "ping")
        .collect();
    assert_eq!(types, vec!["promote", "resync"]);
    assert_eq!(e.primary().map(|p| p.id), Some(sb));
    // The touch goes out again with its id once the promoted standby answers.
    e.received(
        sb,
        &json!({ "type": "resume", "replica": { "id": "r2", "role": "leader" },
        "you": {}, "epoch": "1", "revision": 5, "deltas": [] })
        .to_string(),
        t + 1,
    );
    let resent: Vec<Value> = sent(&e.take_actions(), sb)
        .into_iter()
        .filter(|m| m["type"] == "touch")
        .collect();
    assert_eq!(resent, vec![touch]);
    assert_eq!(e.take_events().last(), Some(&Event::Synced));
}

#[test]
fn deadlines_grow_with_the_measured_round_trip() {
    let mut e = live(1);
    assert_eq!(e.dead_ms(), DEAD_MS);
    assert_eq!(e.sync_timeout(), SYNC_TIMEOUT_MS);
    e.tick(HEARTBEAT_MS);
    let ping = sent(&e.take_actions(), 1).remove(0);
    let t = ping["t"].as_u64().unwrap();
    // A 400 ms round trip: rto = 400 + 4 * 200.
    e.received(
        1,
        &json!({ "type": "pong", "replica": { "id": "r1", "role": "leader" }, "t": t }).to_string(),
        t + 400,
    );
    assert_eq!(e.rto(), Some(1200.0));
    assert_eq!(e.dead_ms(), DEAD_MS + 1200);
    assert_eq!(e.sync_timeout(), READ_INDEX_HOLD_MS + 2400);
    assert_eq!(e.connect_timeout(Role::Primary), 4800);
}

#[test]
fn a_primary_that_cannot_commit_is_left_for_a_standby_that_can() {
    let mut e = live(2);
    let sb = e.standby().unwrap().id;
    e.opened(sb, 1);
    e.received(sb, &snapshot("r2", 5), 2);
    e.received(sb, &replicas("r2", true), 3);
    e.received(1, &replicas("r1", false), 4);
    e.take_actions();
    e.tick(4 + COMMIT_SETTLE_MS);
    assert_eq!(e.primary().map(|p| p.id), Some(sb));
    assert!(e.take_actions().contains(&Action::Close { link: 1 }));
}

#[test]
fn without_a_standby_the_primary_is_retried_on_the_next_endpoint_then_backs_off() {
    let mut e = engine(2);
    e.start(0);
    e.closed(1, 0);
    e.tick(PRIMARY_RETRY_MS[1]);
    assert!(e.take_actions().contains(&Action::Connect {
        link: 2,
        endpoint: 1
    }));
    e.closed(2, 300);
    e.tick(300 + PRIMARY_RETRY_MS[2]);
    assert!(e.take_actions().contains(&Action::Connect {
        link: 3,
        endpoint: 0
    }));
    e.closed(3, 900);
    e.tick(900 + OFFLINE_RETRY_MS - 1);
    assert!(
        !e.take_actions()
            .iter()
            .any(|a| matches!(a, Action::Connect { .. }))
    );
    e.tick(900 + OFFLINE_RETRY_MS);
    assert!(e.take_actions().contains(&Action::Connect {
        link: 4,
        endpoint: 1
    }));
}

#[test]
fn a_handover_moves_the_session_to_a_new_connection_on_the_same_replica() {
    let mut e = live(1);
    e.handover(10);
    let Some(Action::Connect { link, endpoint: 0 }) = e.take_actions().pop() else {
        panic!("a new connection");
    };
    e.opened(link, 20);
    assert_eq!(sent(&e.take_actions(), link)[0]["mode"], "standby");
    e.received(
        link,
        &json!({ "type": "resume", "replica": { "id": "r1", "role": "leader" },
        "you": {}, "epoch": "1", "revision": 5, "deltas": [] })
        .to_string(),
        30,
    );
    let actions = e.take_actions();
    assert!(actions.contains(&Action::Close { link: 1 }));
    let types: Vec<Value> = sent(&actions, link)
        .into_iter()
        .map(|m| m["type"].clone())
        .collect();
    assert_eq!(types, vec![json!("promote"), json!("resync")]);
    assert_eq!(e.primary().map(|p| p.id), Some(link));
}

#[test]
fn consoles_go_out_after_every_sync_and_a_selection_lapses_with_its_connection() {
    let mut e = live(1);
    let console = |bank: u64| {
        json!({ "type": "console", "id": "xt", "name": "X-Touch",
        "protocol": "mcu", "transport": "bridge", "strips": 8, "bankOffset": bank })
    };
    e.console(console(0), 0);
    e.select(Some("ch1"));
    assert_eq!(sent(&e.take_actions(), 1).len(), 2);
    // Bank changes within 100 ms go out once, the latest.
    e.console(console(8), 10);
    e.console(console(16), 20);
    assert!(sent(&e.take_actions(), 1).is_empty());
    e.tick(100);
    let out: Vec<Value> = sent(&e.take_actions(), 1)
        .into_iter()
        .filter(|m| m["type"] == "console")
        .collect();
    assert_eq!(out, vec![console(16)]);
    // A new connection gets the consoles again, never the selection.
    e.closed(1, 200);
    e.tick(1000);
    let link = e.primary().unwrap().id;
    e.opened(link, 1000);
    e.received(link, &snapshot("r1", 9), 1001);
    let types: Vec<Value> = sent(&e.take_actions(), link)
        .into_iter()
        .map(|m| m["type"].clone())
        .collect();
    assert_eq!(types, vec![json!("hello"), json!("console")]);
    assert_eq!(e.selection(), None);
}

#[test]
fn a_console_registration_not_applied_goes_out_again() {
    let mut e = live(1);
    let console = json!({ "type": "console", "id": "xt", "name": "X-Touch",
        "protocol": "mcu", "transport": "bridge", "strips": 8, "bankOffset": 0 });
    e.console(console.clone(), 0);
    e.take_actions();
    let error = json!({ "type": "error", "replica": { "id": "r1", "role": "leader" }, "code": 503, "message": "not applied" });
    e.received(1, &error.to_string(), 10);
    assert_eq!(
        e.take_events(),
        vec![Event::Error("503 not applied".into())]
    );
    e.tick(CONSOLE_INTERVAL_MS);
    let out: Vec<Value> = sent(&e.take_actions(), 1)
        .into_iter()
        .filter(|m| m["type"] == "console")
        .collect();
    assert_eq!(out, vec![console]);
}

/// A bridge restarted with a new session finds its consoles still held
/// by the old one until that lease lapses: the registration goes out
/// again, backing off, until it is taken.
#[test]
fn a_console_held_by_another_session_is_registered_again_until_taken() {
    let mut e = live(1);
    let console = json!({ "type": "console", "id": "xt", "name": "X-Touch",
        "protocol": "mcu", "transport": "bridge", "strips": 8, "bankOffset": 0 });
    e.console(console.clone(), 0);
    e.take_actions();
    let refused =
        json!({ "type": "error", "replica": { "id": "r1", "role": "leader" }, "code": 409,
        "message": "console xt is connected to another session" })
        .to_string();
    // faderz keeps beating, so the link stays up.
    let tick = |e: &mut Engine, t: u64| {
        let ping = json!({ "type": "ping", "replica": { "id": "r1", "role": "leader" }, "t": t });
        e.received(1, &ping.to_string(), t);
        e.tick(t);
    };
    let consoles_out = |e: &mut Engine| -> usize {
        sent(&e.take_actions(), 1)
            .iter()
            .filter(|m| m["type"] == "console")
            .count()
    };
    e.received(1, &refused, 10);
    tick(&mut e, 10 + CONSOLE_RETRY_MS - 1);
    assert_eq!(consoles_out(&mut e), 0);
    tick(&mut e, 10 + CONSOLE_RETRY_MS);
    assert_eq!(consoles_out(&mut e), 1);
    // Refused again: the delay doubles.
    let t = 10 + CONSOLE_RETRY_MS;
    e.received(1, &refused, t);
    tick(&mut e, t + 2 * CONSOLE_RETRY_MS - 1);
    assert_eq!(consoles_out(&mut e), 0);
    tick(&mut e, t + 2 * CONSOLE_RETRY_MS);
    assert_eq!(consoles_out(&mut e), 1);
    // Taken: the next refusal starts from the first delay again.
    let ours = json!({ "type": "delta", "replica": { "id": "r1", "role": "leader" }, "revision": 6,
        "changes": [{ "path": "/consoles", "value": [{ "id": "xt", "name": "X-Touch", "handle": e.handle(),
            "protocol": "mcu", "transport": "bridge", "connected": true, "bankOffset": 0, "strips": 8 }] }] });
    e.received(1, &ours.to_string(), 5000);
    assert_eq!(e.console_refusals, 0);
}

#[test]
fn reconcile_releases_what_is_not_touched_and_touches_what_is() {
    let mut e = live(1);
    let me =
        json!({ "principal": "p", "handle": e.handle(), "kind": "console", "connection": "c" });
    e.mirror.owners.insert("/channels/ch1/mute".into(), me);
    let touching: HashSet<String> = ["/channels/ch1/fader".to_owned()].into();
    e.reconcile(&touching, 0);
    let out: Vec<(Value, Value)> = sent(&e.take_actions(), 1)
        .into_iter()
        .map(|m| (m["type"].clone(), m["path"].clone()))
        .collect();
    assert_eq!(
        out,
        vec![
            (json!("release"), json!("/channels/ch1/mute")),
            (json!("touch"), json!("/channels/ch1/fader")),
        ]
    );
}

/// A snapshot on `replica` of `shape` with the duck group comm in auto.
fn meter_snapshot(replica: &str, revision: u64, shape: Value) -> String {
    json!({
        "type": "snapshot", "replica": { "id": replica, "role": "leader" },
        "you": {}, "epoch": "1", "revision": revision, "shape": shape,
        "state": { "channels": {}, "ducks": { "comm": { "mode": "auto", "hold": false } }, "consoles": [] },
        "owners": {},
    })
    .to_string()
}

fn credits(actions: &[Action], link: LinkId) -> Vec<Value> {
    sent(actions, link)
        .into_iter()
        .filter(|m| m["type"] == "credit")
        .map(|m| m["n"].clone())
        .collect()
}

#[test]
fn meter_credits_go_to_the_primary_topped_up_once_half_are_used() {
    let frame = crate::meters::tests::example();
    let mut e = engine(2);
    e.start(0);
    e.opened(1, 0);
    e.received(
        1,
        &meter_snapshot("r1", 5, crate::meters::tests::shape()),
        0,
    );
    assert_eq!(credits(&e.take_actions(), 1), vec![json!(METER_CREDIT_CAP)]);
    // The standby gets none, and its frames use none of the primary's.
    let sb = e.standby().unwrap().id;
    e.opened(sb, 1);
    e.received(
        sb,
        &meter_snapshot("r2", 5, crate::meters::tests::shape()),
        2,
    );
    e.binary(sb, &frame, 3);
    assert!(credits(&e.take_actions(), sb).is_empty());
    for t in [40, 80, 120] {
        e.binary(1, &frame, t);
    }
    assert!(credits(&e.take_actions(), 1).is_empty());
    e.binary(1, &frame, 160);
    assert_eq!(credits(&e.take_actions(), 1), vec![json!(4)]);
    // Promoted, the standby holds none until it is granted the cap.
    e.closed(1, 200);
    e.received(
        sb,
        &json!({ "type": "resume", "replica": { "id": "r2", "role": "leader" },
        "you": {}, "epoch": "1", "revision": 5, "deltas": [] })
        .to_string(),
        210,
    );
    assert_eq!(e.primary().map(|p| p.id), Some(sb));
    assert_eq!(
        credits(&e.take_actions(), sb),
        vec![json!(METER_CREDIT_CAP)]
    );
    // A shape without duck groups: the strips' meters read every frame.
    let mut shape = crate::meters::tests::shape();
    shape["ducks"] = json!([]);
    let mut e = engine(1);
    e.start(0);
    e.opened(1, 0);
    e.received(1, &meter_snapshot("r1", 5, shape), 0);
    assert_eq!(credits(&e.take_actions(), 1), vec![json!(METER_CREDIT_CAP)]);
}

#[test]
fn a_meter_frame_keeps_the_duck_readings_and_another_layout_asks_once() {
    let shape = crate::meters::tests::shape();
    let frame = crate::meters::tests::example();
    let mut e = engine(1);
    e.start(0);
    e.opened(1, 0);
    e.received(1, &meter_snapshot("r1", 5, shape.clone()), 0);
    e.take_actions();
    e.binary(1, &frame, 40);
    let read = Duck {
        envelope: 0.5,
        engaged: true,
        at: 40,
    };
    assert_eq!(e.duck("comm"), Some(&read));
    assert_eq!(e.peaks().dbfs, vec![-6.5]);
    assert_eq!((e.peaks().at, e.peaks().frames), (40, 1));
    // Of another length: dropped.
    e.binary(1, &frame[..frame.len() - 2], 80);
    assert_eq!(e.duck("comm"), Some(&read));
    // Of another layout: dropped, and one resync asks for the shape.
    let mut other = frame.clone();
    other[8] ^= 1;
    let resyncs = |e: &mut Engine| -> Vec<Value> {
        sent(&e.take_actions(), 1)
            .into_iter()
            .filter(|m| m["type"] == "resync")
            .collect()
    };
    e.binary(1, &other, 120);
    assert_eq!(e.duck("comm"), Some(&read));
    assert_eq!(
        resyncs(&mut e),
        vec![json!({ "type": "resync", "lastRevision": 5, "epoch": "1", "full": true })]
    );
    // The same shape again: its frames are dropped without asking.
    e.received(1, &meter_snapshot("r1", 5, shape.clone()), 130);
    e.binary(1, &other, 160);
    assert!(resyncs(&mut e).is_empty());
    // A new shape: asked again.
    let mut two = shape;
    two["ducks"] = json!([{ "id": "comm", "name": "C" }, { "id": "b", "name": "B" }]);
    e.received(1, &meter_snapshot("r1", 5, two), 170);
    e.binary(1, &other, 200);
    assert_eq!(resyncs(&mut e).len(), 1);
}

#[test]
fn continuous_paths_follow_the_contract() {
    for p in [
        "/channels/ch1/fader",
        "/buses/pgm/fader",
        "/monitor/level",
        "/vcas/v1/offset",
        "/channels/c/sends/a1/level",
        "/auxes/a/limiter/ceiling",
        "/channels/c/eq/bands/2/gain",
        "/buses/pgm/comp/mix",
    ] {
        assert!(continuous(p), "{p}");
    }
    for p in [
        "/channels/ch1/mute",
        "/channels/ch1/solo",
        "/monitor/dim",
        "/channels/c/sends/a1/on",
        "/channels/c/comp/enabled",
    ] {
        assert!(!continuous(p), "{p}");
    }
}
