use super::*;

/// A measurement restarted when the state counts a reset of it and
/// this replica applied none, or it started elsewhere than the last
/// one applied starts it, once that start is past by
/// [`RESET_PENDING`]. Without a reset counted nothing restarted; one
/// not started has not.
#[test]
fn a_measurement_restarted_unless_it_started_at_the_last_reset() {
    let at = 100 * WINDOW;
    let later = at + RESET_PENDING;
    let none = Resets::default();
    let applied = Resets {
        counted: 1,
        at: Some(at),
    };
    let missed = Resets {
        counted: 1,
        at: None,
    };
    assert!(!restarted(None, missed, later));
    // No reset ever: every replica measures from its own start.
    assert!(!restarted(Some(7 * WINDOW), none, later));
    // Counted and never applied: started after the reset.
    assert!(restarted(Some(7 * WINDOW), missed, later));
    assert!(!restarted(Some(at), applied, later));
    // Started elsewhere than the reset applied starts it.
    assert!(restarted(Some(at + WINDOW), applied, later));
    assert!(restarted(Some(at - WINDOW), applied, later));
    // Applied, and not reached yet: pending.
    assert!(!restarted(Some(at - WINDOW), applied, later - 1));
}

/// A start on libmxl's clock reads as the UTC time that far back from
/// now, within the time the two clock reads take.
#[test]
fn a_start_reads_as_utc_that_far_back() {
    let now = crate::replica::tai_samples();
    let since = utc_of(now - 60 * crate::replica::RATE, now);
    let want = std::time::SystemTime::now() - Duration::from_secs(60);
    let want = want
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    // Seconds into the day, from "YYYY-MM-DDTHH:MM:SS.mmmZ".
    let ms = |t: &str| {
        let n = |r: std::ops::Range<usize>| t[r].parse::<i64>().unwrap();
        ((n(11..13) * 60 + n(14..16)) * 60 + n(17..19)) * 1000 + n(20..23)
    };
    let day = 86_400_000;
    let diff = (ms(&since) - want.rem_euclid(day)).rem_euclid(day);
    assert!(diff.min(day - diff) <= 1000, "{since}");
    assert!(since.ends_with('Z'), "{since}");
}

#[test]
fn the_resets_counted_name_their_bus() {
    let state = json!({ "loudness": { "a": { "running": true }, "b": { "resets": 3 } } });
    assert_eq!(
        resets_of(&state),
        BTreeMap::from([("a".to_owned(), 0), ("b".to_owned(), 3)])
    );
}

/// A tone channel's input is live for good, as an attached flow is;
/// a pruned or detached one is not.
#[test]
fn a_tone_input_counts_as_attached() {
    assert!(attached(InputState::Attached));
    assert!(attached(InputState::Tone));
    assert!(!attached(InputState::Pruned));
    assert!(!attached(InputState::Detached));
}

/// The margin an input needs is the larger of the two the engine
/// reports, either of which may be absent.
#[test]
fn the_margin_needed_is_the_larger_report() {
    let status = |warning, short| InputStatus {
        name: "in".into(),
        state: InputState::Attached,
        up: true,
        readable: Some(true),
        flow_id: String::new(),
        error: None,
        margin_short: short,
        margin_warning: warning,
        counters: Default::default(),
        meters: Default::default(),
        pre_strip_meters: None,
    };
    assert_eq!(margin_needed(&status(None, None)), 0);
    assert_eq!(margin_needed(&status(Some(1303), None)), 1303);
    assert_eq!(margin_needed(&status(None, Some(1008))), 1008);
    assert_eq!(margin_needed(&status(Some(700), Some(1008))), 1008);
    assert_eq!(margin_needed(&status(Some(1399), Some(1008))), 1399);
}

/// A rise of a drop count or a new divergence resends, at most once
/// per RESEND_EVERY, logged once per episode; generation_late is not
/// read at all.
#[test]
fn dropped_entries_resend_with_a_backoff() {
    let t0 = std::time::Instant::now();
    let at = |ms: u64| t0 + Duration::from_millis(ms);
    let mut d = Drops::new([0, 0], None);
    assert_eq!(d.tick([0, 0], &None, at(0)), Act::None);
    assert_eq!(d.tick([1, 0], &None, at(40)), Act::Resend { log: true });
    assert_eq!(d.tick([1, 0], &None, at(80)), Act::None);
    // Another drop within the second waits, and readiness fails meanwhile.
    assert_eq!(d.tick([2, 0], &None, at(120)), Act::Hold);
    assert_eq!(d.tick([3, 0], &None, at(160)), Act::None);
    assert_eq!(d.tick([3, 0], &None, at(1_040)), Act::Resend { log: false });
    // Quiet for a second: the next drop starts a new episode.
    assert_eq!(d.tick([3, 0], &None, at(2_100)), Act::None);
    let diverged = Some("entry 9 late".to_owned());
    assert_eq!(
        d.tick([3, 0], &diverged, at(2_140)),
        Act::Resend { log: true }
    );
    assert_eq!(d.tick([3, 0], &diverged, at(3_200)), Act::None);
}

/// A set's contact timers take the engine's block, margin, hold and
/// contact window, and the cap `replica.maxApplyLeadMs` sets.
#[test]
fn a_set_times_contact_by_its_engine_and_its_cap() {
    let id = |n: u8| format!("00000000-0000-4000-8000-00000000000{n}");
    let config = json!({
        "replica": {
            "replicas": 3, "maxApplyLeadMs": 80,
            "peers": ["f-0.f:7443", "f-1.f:7443", "f-2.f:7443"],
            "peer": { "bind": "0.0.0.0", "port": 7443, "tls": {
                "mode": "files", "certFile": "/t/tls.crt", "keyFile": "/t/tls.key",
                "clientCaFile": "/t/ca.crt" } },
            "marker": "m",
        },
        "shape": crate::shape::tests::small(),
        "mxl": {
            "domains": [{ "name": "d", "path": "/dev/shm/d" }],
            "inputs": { "ch1": { "flow": id(1), "altFlow": id(5) }, "ch2": { "flow": id(2) },
                        "st1": { "flow": id(3) } },
            "outputs": { "out-pgm": { "flows": [id(4), id(6), id(7)] } },
        },
        "engine": { "blockSize": 96, "readerMarginSamples": 960 },
    });
    let (config, _, _) =
        crate::config::Config::parse(&config.to_string(), || "faderz-1".into()).unwrap();
    let engine = config.audio.as_ref().unwrap();
    let t = timing(engine, config.set.as_ref().unwrap());
    assert_eq!((t.block, t.margin), (96, 960));
    assert_eq!(t.max_apply_lead, Duration::from_millis(80));
    assert_eq!(t.hold, Duration::from_millis(u64::from(engine.hold_ms)));
    assert_eq!(
        t.contact_window,
        Duration::from_millis(u64::from(engine.contact_window_ms))
    );
}

/// counters.schema.json names exactly the counters the `counters`
/// message carries, each with a title and a description, so the
/// console never shows a counter it cannot explain.
#[test]
fn the_counters_schema_names_every_counter_sent() {
    let schema: Value =
        serde_json::from_str(include_str!("../../api/ws/counters.schema.json")).unwrap();
    let named: std::collections::BTreeSet<&str> = (schema["properties"].as_object())
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let c = Counters::default();
    let sent: std::collections::BTreeSet<String> = (engine_counters(&c).iter())
        .map(|(name, _)| format!("{name}_total"))
        .chain(["input_too_late_blocks_total".into(), "resends_total".into()])
        // App::counters_message adds the tallies taken late.
        .chain(["afv_late_total".into()])
        .collect();
    assert_eq!(
        named,
        sent.iter().map(String::as_str).collect(),
        "counters.schema.json and the counters message"
    );
    for name in named {
        for key in ["title", "description"] {
            let text = schema["properties"][name][key].as_str().unwrap_or("");
            assert!(!text.is_empty(), "{name} has no {key}");
        }
    }
}

/// A strip parameter the RT thread refused counts as a drop.
#[test]
fn a_refused_strip_entry_resends() {
    let t0 = std::time::Instant::now();
    let c = Counters::default();
    let mut d = Drops::new(dropped(&c), None);
    assert_eq!(d.tick(dropped(&c), &None, t0), Act::None);
    c.strip_refused.store(1, Relaxed);
    let at = t0 + Duration::from_millis(40);
    assert_eq!(d.tick(dropped(&c), &None, at), Act::Resend { log: true });
    c.command_late.store(1, Relaxed);
    let at = t0 + Duration::from_millis(1_100);
    assert_eq!(d.tick(dropped(&c), &None, at), Act::Resend { log: false });
}
