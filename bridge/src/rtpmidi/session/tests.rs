use super::*;

const PEER_SSRC: u32 = 0xabcd;
const TOKEN: u32 = 0x1111;

fn addr(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

/// Counts up from 1: the session's ssrc, sequence numbers and tokens.
fn counter() -> Box<dyn FnMut() -> u32 + Send> {
    let mut n = 0;
    Box::new(move || {
        n += 1;
        n
    })
}

fn listener() -> Session {
    Session::new(
        Config {
            name: "faderz".into(),
            allow: vec!["198.51.100.0/24".parse().unwrap()],
            invite: None,
        },
        counter(),
        0,
    )
}

fn sent(s: &mut Session) -> Vec<Transmit> {
    std::iter::from_fn(|| s.poll_transmit()).collect()
}

fn events(s: &mut Session) -> Vec<Event> {
    std::iter::from_fn(|| s.poll_event()).collect()
}

fn invitation(token: u32, ssrc: u32) -> Vec<u8> {
    exchange(b"IN", token, ssrc, Some("laptop"))
}

/// A listener in a session with 198.51.100.1, control from port 6000 and
/// data from 6001 (as a NAT could map them: not 5004/5005).
fn up() -> Session {
    let mut s = listener();
    s.received(
        Socket::Control,
        addr("198.51.100.1:6000"),
        &invitation(TOKEN, PEER_SSRC),
        0,
    );
    s.received(
        Socket::Data,
        addr("198.51.100.1:6001"),
        &invitation(TOKEN, PEER_SSRC),
        10,
    );
    sent(&mut s);
    events(&mut s);
    s
}

fn midi_packet(seq: u16, commands: &[Vec<u8>]) -> Vec<u8> {
    payload::encode(
        Header {
            seq,
            timestamp: 0,
            ssrc: PEER_SSRC,
        },
        commands,
    )
}

#[test]
fn an_invitation_is_accepted_on_both_ports_at_the_addresses_it_came_from() {
    let mut s = listener();
    s.received(
        Socket::Control,
        addr("198.51.100.1:6000"),
        &invitation(TOKEN, PEER_SSRC),
        0,
    );
    let out = sent(&mut s);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].socket, Socket::Control);
    assert_eq!(out[0].to, addr("198.51.100.1:6000"));
    assert!(matches!(
        parse(&out[0].bytes),
        Some(Command::Accepted {
            token: TOKEN,
            ssrc: 1,
            ..
        })
    ));
    assert!(out[0].bytes.ends_with(b"faderz\0"));
    assert!(!s.is_up());
    assert!(events(&mut s).is_empty());

    s.received(
        Socket::Data,
        addr("198.51.100.1:7777"),
        &invitation(TOKEN, PEER_SSRC),
        5,
    );
    let out = sent(&mut s);
    assert_eq!(out[0].socket, Socket::Data);
    assert_eq!(out[0].to, addr("198.51.100.1:7777"));
    assert!(matches!(
        parse(&out[0].bytes),
        Some(Command::Accepted { .. })
    ));
    assert!(s.is_up());
    assert_eq!(
        events(&mut s),
        vec![Event::Up {
            name: "laptop".into(),
            peer: addr("198.51.100.1:6000")
        }]
    );
    // MIDI goes to the data port's address.
    assert!(s.send_midi(&[vec![0x90, 1, 2]], 6));
    assert_eq!(sent(&mut s)[0].to, addr("198.51.100.1:7777"));
}

#[test]
fn invitations_from_peers_not_allowed_or_of_another_version_are_refused() {
    let mut s = listener();
    s.received(
        Socket::Control,
        addr("192.0.2.5:5004"),
        &invitation(TOKEN, PEER_SSRC),
        0,
    );
    let mut v3 = invitation(TOKEN, PEER_SSRC);
    v3[7] = 3;
    s.received(Socket::Control, addr("198.51.100.1:5004"), &v3, 0);
    // A data port invitation without the control port's first.
    s.received(
        Socket::Data,
        addr("198.51.100.1:5005"),
        &invitation(TOKEN, PEER_SSRC),
        0,
    );
    // The source not allowed gets no answer; the others a NO.
    let out = sent(&mut s);
    assert_eq!(out.len(), 2);
    for t in &out {
        assert_eq!(parse(&t.bytes), Some(Command::Rejected { token: TOKEN }));
        assert_eq!(t.to.ip(), addr("198.51.100.1:1").ip());
    }
    assert!(!s.is_up());
    assert!(events(&mut s).is_empty());
    // Nor on the data port, nor however often it tries.
    for i in 0..100 {
        s.received(
            Socket::Data,
            addr("192.0.2.5:5005"),
            &invitation(TOKEN, PEER_SSRC),
            i,
        );
    }
    assert!(sent(&mut s).is_empty());
    // An IPv4-mapped IPv6 source is checked as IPv4.
    s.received(
        Socket::Control,
        addr("[::ffff:198.51.100.2]:5004"),
        &invitation(TOKEN, PEER_SSRC),
        0,
    );
    let out = sent(&mut s);
    assert!(matches!(
        parse(&out[0].bytes),
        Some(Command::Accepted { .. })
    ));
    assert_eq!(out[0].to, addr("198.51.100.2:5004"));
}

#[test]
fn a_second_peer_waits_and_the_same_peer_restarts_the_session() {
    let mut s = up();
    s.received(
        Socket::Control,
        addr("198.51.100.9:5004"),
        &invitation(7, 8),
        20,
    );
    assert!(matches!(
        parse(&sent(&mut s)[0].bytes),
        Some(Command::Rejected { .. })
    ));
    assert!(s.is_up());
    // A lost answer: the same invitation again is answered again.
    s.received(
        Socket::Data,
        addr("198.51.100.1:6001"),
        &invitation(TOKEN, PEER_SSRC),
        30,
    );
    assert!(matches!(
        parse(&sent(&mut s)[0].bytes),
        Some(Command::Accepted { .. })
    ));
    assert!(events(&mut s).is_empty());
    // The peer restarted: a new token from the same address.
    s.received(
        Socket::Control,
        addr("198.51.100.1:6000"),
        &invitation(2, PEER_SSRC + 1),
        40,
    );
    assert_eq!(
        events(&mut s),
        vec![Event::Down {
            reason: "the peer invited again"
        }]
    );
    // Only the answer goes out: a bye would reach the new session.
    let out = sent(&mut s);
    assert_eq!(out.len(), 1);
    assert!(matches!(
        parse(&out[0].bytes),
        Some(Command::Accepted { token: 2, .. })
    ));
    s.received(
        Socket::Data,
        addr("198.51.100.1:6001"),
        &invitation(2, PEER_SSRC + 1),
        50,
    );
    assert!(matches!(events(&mut s)[..], [Event::Up { .. }]));
}

#[test]
fn clock_sync_is_answered_and_its_absence_ends_the_session() {
    let mut s = up();
    let ck0 = sync(PEER_SSRC, 0, [1234, 0, 0]);
    s.received(Socket::Data, addr("198.51.100.1:6001"), &ck0, 30 * SECOND);
    let out = sent(&mut s);
    assert_eq!(out[0].to, addr("198.51.100.1:6001"));
    assert_eq!(
        parse(&out[0].bytes),
        Some(Command::Sync {
            ssrc: 1,
            count: 1,
            timestamps: [1234, 30 * SECOND / 100, 0]
        })
    );
    s.received(
        Socket::Data,
        addr("198.51.100.1:6001"),
        &sync(PEER_SSRC, 2, [1234, 5, 6]),
        31 * SECOND,
    );
    assert!(sent(&mut s).is_empty());
    assert_eq!(s.deadline(), 151 * SECOND);
    s.tick(150 * SECOND);
    assert!(s.is_up());
    s.tick(151 * SECOND);
    assert!(!s.is_up());
    assert_eq!(
        events(&mut s),
        vec![Event::Down {
            reason: "no clock sync"
        }]
    );
    let out = sent(&mut s);
    assert_eq!(out.len(), 2);
    assert_eq!(
        out.iter().map(|t| (t.socket, t.to)).collect::<Vec<_>>(),
        vec![
            (Socket::Control, addr("198.51.100.1:6000")),
            (Socket::Data, addr("198.51.100.1:6001"))
        ]
    );
    assert!(
        out.iter()
            .all(|t| parse(&t.bytes) == Some(Command::Bye { ssrc: 1 }))
    );
}

#[test]
fn a_bye_ends_the_session_and_only_from_the_peer() {
    let mut s = up();
    s.received(
        Socket::Control,
        addr("198.51.100.1:6000"),
        &exchange(b"BY", TOKEN, 999, None),
        1,
    );
    assert!(s.is_up());
    s.received(
        Socket::Control,
        addr("198.51.100.1:6000"),
        &exchange(b"BY", TOKEN, PEER_SSRC, None),
        2,
    );
    assert!(!s.is_up());
    assert_eq!(
        events(&mut s),
        vec![Event::Down {
            reason: "the peer said bye"
        }]
    );
    assert!(sent(&mut s).is_empty());
}

#[test]
fn midi_from_the_peer_comes_out_and_is_acknowledged() {
    let mut s = up();
    s.received(
        Socket::Data,
        addr("198.51.100.1:6001"),
        &midi_packet(100, &[vec![0x90, 104, 0x7f], vec![0xe0, 0, 0x40]]),
        SECOND,
    );
    assert_eq!(
        events(&mut s),
        vec![
            Event::Midi(vec![0x90, 104, 0x7f]),
            Event::Midi(vec![0xe0, 0, 0x40])
        ]
    );
    // From another address, or with another ssrc: ignored.
    s.received(
        Socket::Data,
        addr("198.51.100.70:6001"),
        &midi_packet(101, &[vec![0x90, 1, 1]]),
        SECOND,
    );
    let mut other = midi_packet(101, &[vec![0x90, 1, 1]]);
    other[11] ^= 1;
    s.received(Socket::Data, addr("198.51.100.1:6001"), &other, SECOND);
    // Malformed: refused.
    s.received(
        Socket::Data,
        addr("198.51.100.1:6001"),
        &[0x80, 0x61, 0, 101],
        SECOND,
    );
    // A copy of a packet: dropped.
    s.received(
        Socket::Data,
        addr("198.51.100.1:6001"),
        &midi_packet(100, &[vec![0x90, 2, 2]]),
        SECOND,
    );
    assert!(events(&mut s).is_empty());
    assert_eq!(s.deadline(), SECOND);
    s.tick(SECOND);
    let out = sent(&mut s);
    assert_eq!(out[0].socket, Socket::Control);
    assert_eq!(out[0].to, addr("198.51.100.1:6000"));
    assert_eq!(
        parse(&out[0].bytes),
        Some(Command::Feedback { ssrc: 1, seq: 100 })
    );
    // Nothing new arrived: no more feedback.
    s.tick(4 * SECOND);
    assert!(sent(&mut s).is_empty());
}

#[test]
fn a_gap_in_the_peers_sequence_numbers_asks_for_the_state_again() {
    let mut s = up();
    let p = |seq| midi_packet(seq, &[vec![0x90, 1, 1]]);
    s.received(Socket::Data, addr("198.51.100.1:6001"), &p(65535), 0);
    s.received(Socket::Data, addr("198.51.100.1:6001"), &p(0), 0);
    assert!(!events(&mut s).contains(&Event::Lost));
    s.received(Socket::Data, addr("198.51.100.1:6001"), &p(2), 0);
    assert_eq!(
        events(&mut s),
        vec![Event::Missed, Event::Lost, Event::Midi(vec![0x90, 1, 1])]
    );
    // The resend at most once per hold-off; every gap counts.
    s.received(Socket::Data, addr("198.51.100.1:6001"), &p(5), SECOND);
    assert_eq!(
        events(&mut s),
        vec![Event::Missed, Event::Midi(vec![0x90, 1, 1])]
    );
    s.received(Socket::Data, addr("198.51.100.1:6001"), &p(9), 3 * SECOND);
    assert!(events(&mut s).starts_with(&[Event::Missed, Event::Lost]));
}

#[test]
fn a_gap_right_after_a_refresh_still_counts() {
    let mut s = up();
    s.received(
        Socket::Data,
        addr("198.51.100.1:6001"),
        &midi_packet(1, &[vec![0xfe]]),
        0,
    );
    s.send_midi(&[vec![0x90, 1, 1]], SECOND);
    sent(&mut s);
    // No feedback: the state is refreshed at 11 s.
    s.tick(11 * SECOND);
    assert!(events(&mut s).contains(&Event::Lost));
    // A gap a second later is inside the resend's hold-off, and
    // still counts as missed.
    s.received(
        Socket::Data,
        addr("198.51.100.1:6001"),
        &midi_packet(3, &[vec![0xfe]]),
        12 * SECOND,
    );
    assert_eq!(events(&mut s), vec![Event::Missed, Event::Midi(vec![0xfe])]);
}

#[test]
fn feedback_behind_what_was_sent_long_ago_asks_for_the_state_again() {
    let mut s = up();
    let first = {
        s.send_midi(&[vec![0x90, 1, 1]], 0);
        let (h, _) = Decoder::default().decode(&sent(&mut s)[0].bytes).unwrap();
        h.seq
    };
    s.send_midi(&[vec![0x90, 2, 2]], 100_000);
    s.send_midi(&[vec![0x90, 3, 3]], 200_000);
    sent(&mut s);
    let rs = |seq: u16| feedback(PEER_SSRC, seq);
    // Up to date: nothing lost.
    s.received(
        Socket::Control,
        addr("198.51.100.1:6000"),
        &rs(first.wrapping_add(2)),
        SECOND,
    );
    assert!(events(&mut s).is_empty());
    s.send_midi(&[vec![0x90, 4, 4]], 2 * SECOND);
    s.send_midi(&[vec![0x90, 5, 5]], 2 * SECOND);
    // The peer has the first of them, and the second is only just out.
    s.received(
        Socket::Control,
        addr("198.51.100.1:6000"),
        &rs(first.wrapping_add(3)),
        2 * SECOND + 100_000,
    );
    assert!(events(&mut s).is_empty());
    s.send_midi(&[vec![0x90, 6, 6]], 3 * SECOND);
    // Still behind the packet sent at 2 s: lost.
    s.received(
        Socket::Control,
        addr("198.51.100.1:6000"),
        &rs(first.wrapping_add(3)),
        3 * SECOND,
    );
    assert_eq!(events(&mut s), vec![Event::Lost]);
}

#[test]
fn the_configured_peer_is_invited_on_both_ports_and_synced() {
    let target = addr("192.0.2.101:5004");
    let mut s = Session::new(
        Config {
            name: "faderz".into(),
            allow: vec![],
            invite: Some(target),
        },
        counter(),
        0,
    );
    s.tick(0);
    let out = sent(&mut s);
    assert_eq!((out[0].socket, out[0].to), (Socket::Control, target));
    let Some(Command::Invitation {
        token,
        ssrc: 1,
        version: 2,
        name,
    }) = parse(&out[0].bytes)
    else {
        panic!("{:?}", out[0]);
    };
    assert_eq!(name, "faderz");
    // Unanswered: sent again every second.
    s.tick(SECOND);
    assert_eq!(sent(&mut s).len(), 1);
    // An answer with another token, or from elsewhere, changes nothing.
    s.received(
        Socket::Control,
        target,
        &exchange(b"OK", token + 1, 77, Some("x")),
        SECOND,
    );
    s.received(
        Socket::Control,
        addr("192.0.2.102:5004"),
        &exchange(b"OK", token, 77, Some("x")),
        SECOND,
    );
    assert!(sent(&mut s).is_empty());
    s.received(
        Socket::Control,
        target,
        &exchange(b"OK", token, 77, Some("X-Touch")),
        SECOND,
    );
    let out = sent(&mut s);
    assert_eq!(
        (out[0].socket, out[0].to),
        (Socket::Data, addr("192.0.2.101:5005"))
    );
    assert!(matches!(
        parse(&out[0].bytes),
        Some(Command::Invitation { .. })
    ));
    s.received(
        Socket::Data,
        addr("192.0.2.101:5005"),
        &exchange(b"OK", token, 77, Some("X-Touch")),
        SECOND,
    );
    assert!(s.is_up());
    assert!(matches!(events(&mut s)[..], [Event::Up { .. }]));
    // The first clock sync goes out at once.
    let out = sent(&mut s);
    assert_eq!(
        parse(&out[0].bytes),
        Some(Command::Sync {
            ssrc: 1,
            count: 0,
            timestamps: [SECOND / 100, 0, 0]
        })
    );
    let ck1 = sync(77, 1, [SECOND / 100, 999, 0]);
    s.received(Socket::Data, addr("192.0.2.101:5005"), &ck1, SECOND + 2000);
    assert_eq!(
        parse(&sent(&mut s)[0].bytes),
        Some(Command::Sync {
            ssrc: 1,
            count: 2,
            timestamps: [SECOND / 100, 999, (SECOND + 2000) / 100]
        })
    );
    // Unanswered syncs end the session and the peer is invited again.
    s.tick(3 * SECOND);
    s.tick(5 * SECOND);
    s.tick(7 * SECOND);
    assert!(s.is_up());
    s.tick(9 * SECOND);
    assert!(!s.is_up());
    assert_eq!(
        events(&mut s),
        vec![Event::Down {
            reason: "no answer to clock sync"
        }]
    );
    sent(&mut s);
    // The peer is invited again after the back-off.
    assert_eq!(s.deadline(), 19 * SECOND);
    s.tick(19 * SECOND - 1);
    assert!(sent(&mut s).is_empty());
    s.tick(19 * SECOND);
    let out = sent(&mut s);
    assert_eq!((out[0].socket, out[0].to), (Socket::Control, target));
}

#[test]
fn a_refused_or_unanswered_invitation_backs_off() {
    let target = addr("192.0.2.101:5004");
    let mut s = Session::new(
        Config {
            name: "faderz".into(),
            allow: vec![],
            invite: Some(target),
        },
        counter(),
        0,
    );
    s.tick(0);
    let Some(Command::Invitation { token, .. }) = parse(&sent(&mut s)[0].bytes) else {
        panic!();
    };
    s.received(
        Socket::Control,
        target,
        &exchange(b"NO", token, 77, None),
        0,
    );
    assert_eq!(s.deadline(), INVITE_BACKOFF);
    s.tick(INVITE_BACKOFF - 1);
    assert!(sent(&mut s).is_empty());
    s.tick(INVITE_BACKOFF);
    assert_eq!(sent(&mut s).len(), 1);
    let mut t = INVITE_BACKOFF;
    for _ in 1..INVITE_TRIES {
        t += INVITE_INTERVAL;
        s.tick(t);
        assert_eq!(sent(&mut s).len(), 1);
    }
    t += INVITE_INTERVAL;
    s.tick(t);
    assert!(sent(&mut s).is_empty());
    assert_eq!(s.deadline(), t + INVITE_BACKOFF);
}

#[test]
fn a_data_port_invitation_that_does_not_follow_in_time_is_dropped() {
    let mut s = listener();
    let a = addr("198.51.100.1:6000");
    s.received(Socket::Control, a, &invitation(TOKEN, PEER_SSRC), 0);
    // A copy of the control port's invitation is answered again.
    s.received(Socket::Control, a, &invitation(TOKEN, PEER_SSRC), SECOND);
    let out = sent(&mut s);
    assert_eq!(out.len(), 2);
    assert!(out.iter().all(|t| t.to == a
        && matches!(
            parse(&t.bytes),
            Some(Command::Accepted { token: TOKEN, .. })
        )));
    assert_eq!(s.deadline(), ACCEPT_TIMEOUT);
    s.tick(ACCEPT_TIMEOUT - 1);
    s.received(
        Socket::Data,
        addr("198.51.100.1:6001"),
        &invitation(TOKEN, PEER_SSRC),
        ACCEPT_TIMEOUT - 1,
    );
    assert!(s.is_up());

    let mut s = listener();
    s.received(Socket::Control, a, &invitation(TOKEN, PEER_SSRC), 0);
    sent(&mut s);
    s.tick(ACCEPT_TIMEOUT);
    s.received(
        Socket::Data,
        addr("198.51.100.1:6001"),
        &invitation(TOKEN, PEER_SSRC),
        ACCEPT_TIMEOUT,
    );
    assert!(!s.is_up());
    assert!(matches!(
        parse(&sent(&mut s)[0].bytes),
        Some(Command::Rejected { .. })
    ));
    assert!(events(&mut s).is_empty());
}

#[test]
fn feedback_counts_only_in_a_session_and_for_its_own_packets() {
    let mut s = up();
    s.send_midi(&[vec![0x90, 1, 1]], 0);
    s.send_midi(&[vec![0x90, 2, 2]], 0);
    let first = Decoder::default()
        .decode(&sent(&mut s)[0].bytes)
        .unwrap()
        .0
        .seq;
    // The peer restarts: while its new session is being accepted, its
    // feedback about the old one is not a loss.
    s.received(
        Socket::Control,
        addr("198.51.100.1:6000"),
        &invitation(2, PEER_SSRC),
        SECOND,
    );
    events(&mut s);
    sent(&mut s);
    s.received(
        Socket::Control,
        addr("198.51.100.1:6000"),
        &feedback(PEER_SSRC, first),
        2 * SECOND,
    );
    assert!(events(&mut s).is_empty());
    s.received(
        Socket::Data,
        addr("198.51.100.1:6001"),
        &invitation(2, PEER_SSRC),
        2 * SECOND,
    );
    events(&mut s);
    s.received(
        Socket::Control,
        addr("198.51.100.1:6000"),
        &feedback(PEER_SSRC, first),
        3 * SECOND,
    );
    assert!(events(&mut s).is_empty());
}

#[test]
fn without_feedback_the_state_is_refreshed_every_ten_seconds_while_sending() {
    let mut s = up();
    // Nothing sent: nothing to refresh.
    assert_eq!(s.deadline(), 10 + SYNC_TIMEOUT);
    s.tick(20 * SECOND);
    assert!(events(&mut s).is_empty());
    s.send_midi(&[vec![0x90, 1, 1]], 21 * SECOND);
    sent(&mut s);
    assert_eq!(s.deadline(), 30 * SECOND);
    s.tick(30 * SECOND - 1);
    assert!(events(&mut s).is_empty());
    s.tick(30 * SECOND);
    assert_eq!(events(&mut s), vec![Event::Lost]);
    // Feedback within the window: the peer says what it has.
    s.send_midi(&[vec![0x90, 2, 2]], 31 * SECOND);
    let seq = Decoder::default()
        .decode(&sent(&mut s)[0].bytes)
        .unwrap()
        .0
        .seq;
    s.received(
        Socket::Control,
        addr("198.51.100.1:6000"),
        &feedback(PEER_SSRC, seq),
        32 * SECOND,
    );
    s.tick(40 * SECOND);
    assert!(events(&mut s).is_empty());
}

#[test]
fn invitations_from_peers_not_allowed_are_summarised() {
    let mut s = listener();
    for i in 0..5 {
        s.received(
            Socket::Control,
            addr("192.0.2.5:5004"),
            &invitation(TOKEN, PEER_SSRC),
            i,
        );
    }
    // The first is reported, the rest wait for the next summary.
    assert_eq!(s.refused, 4);
    assert_eq!(s.refused_from, Some(addr("192.0.2.5:5004")));
    s.received(
        Socket::Control,
        addr("192.0.2.6:5004"),
        &invitation(TOKEN, PEER_SSRC),
        REFUSED_REPORT,
    );
    assert_eq!(s.refused, 0);
    assert_eq!(s.refused_reported, Some(REFUSED_REPORT));
}

#[test]
fn closing_says_bye_and_malformed_commands_are_ignored() {
    let mut s = up();
    for junk in [
        &[0xff, 0xff][..],
        b"\xff\xffIN\0\0",
        b"\xff\xffCK\0\0\0\x01\0",
        b"\xff\xffZZ123456789012",
    ] {
        s.received(Socket::Control, addr("198.51.100.1:6000"), junk, 1);
        s.received(Socket::Data, addr("198.51.100.1:6001"), junk, 1);
    }
    assert!(s.is_up());
    assert!(sent(&mut s).is_empty());
    s.close(2);
    assert_eq!(sent(&mut s).len(), 2);
    assert_eq!(events(&mut s), vec![Event::Down { reason: "closed" }]);
}

#[test]
fn a_peer_heard_again_after_a_silence_asks_for_the_state_again() {
    let mut s = up();
    // Feedback every second, as an X-Touch sends it: no loss.
    for t in 1..=5 {
        s.received(
            Socket::Control,
            addr("198.51.100.1:6000"),
            &feedback(PEER_SSRC, 0),
            t * SECOND,
        );
    }
    assert!(events(&mut s).is_empty());
    // A pulled cable: nothing for 3 s, then the peer is back in the same
    // session.
    s.received(
        Socket::Control,
        addr("198.51.100.1:6000"),
        &feedback(PEER_SSRC, 0),
        5 * SECOND + PEER_SILENT,
    );
    assert_eq!(events(&mut s), vec![Event::Lost]);
    // A peer that is quiet for 10 s as a rule, as a Mac's clock sync, is
    // not cut off by that.
    let mut s = up();
    for t in 1..=6 {
        s.received(
            Socket::Control,
            addr("198.51.100.1:6000"),
            &feedback(PEER_SSRC, 0),
            t * 10 * SECOND,
        );
    }
    assert!(events(&mut s).is_empty());
    // Another address is no sign of the peer.
    s.received(
        Socket::Control,
        addr("198.51.100.9:6000"),
        &feedback(PEER_SSRC, 0),
        20 * SECOND,
    );
    assert!(events(&mut s).is_empty());
}
