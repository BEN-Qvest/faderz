//! One AppleMIDI session (Apple's MIDI Network Driver Protocol) without
//! I/O: invitations (IN, OK, NO), clock sync (CK), bye (BY) and receiver
//! feedback (RS) on a control and a data port, and the RTP-MIDI packets
//! on the data port. It accepts invitations from allowed peers, invites
//! a configured peer, and answers every datagram at the address it came
//! from. Times are microseconds on any monotonic clock.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};

use super::Cidr;
use super::payload::{self, Decoder, Header};

const SECOND: u64 = 1_000_000;
/// An unanswered invitation is sent again after this, up to
/// `INVITE_TRIES` times.
const INVITE_INTERVAL: u64 = SECOND;
const INVITE_TRIES: u32 = 12;
/// Wait before inviting again after a refusal, a session that ended or
/// `INVITE_TRIES` unanswered invitations.
const INVITE_BACKOFF: u64 = 10 * SECOND;
/// The data port's invitation must follow the control port's within this.
const ACCEPT_TIMEOUT: u64 = 5 * SECOND;
/// An initiator syncs at least once a minute (Apple's protocol); a
/// session without a sync for twice that is gone.
const SYNC_TIMEOUT: u64 = 120 * SECOND;
/// The first syncs of a session as initiator are this close, then
/// `SYNC_INTERVAL` apart.
const SYNC_FIRST_INTERVAL: u64 = 2 * SECOND;
const SYNC_FIRST: u32 = 6;
const SYNC_INTERVAL: u64 = 20 * SECOND;
/// As initiator, this many syncs in a row without an answer end the
/// session.
const SYNC_UNANSWERED: u32 = 3;
/// Receiver feedback is sent at most this often while packets arrive.
const FEEDBACK_INTERVAL: u64 = SECOND;
/// A packet sent this long before the peer's feedback that does not
/// cover it was lost.
const FEEDBACK_GRACE: u64 = SECOND / 2;
/// At most one [`Event::Lost`] per this.
const LOST_HOLDOFF: u64 = 2 * SECOND;
/// While packets go out and no receiver feedback says what arrived,
/// the peer's whole state is sent again this often.
const REFRESH_INTERVAL: u64 = 10 * SECOND;
/// Invitations refused because their source is not allowed are logged
/// in a summary at most this often.
const REFUSED_REPORT: u64 = 60 * SECOND;
/// How many sent packets are remembered for feedback.
const SENT_KEPT: usize = 256;
const VERSION: u32 = 2;

/// Which of the session's two sockets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Socket {
    Control,
    Data,
}

/// A datagram to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transmit {
    pub socket: Socket,
    pub to: SocketAddr,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// A session started, with the peer's name and control address.
    Up { name: String, peer: SocketAddr },
    /// The session ended.
    Down { reason: &'static str },
    /// Packets to the peer may have been lost: what it shows is to be
    /// sent again. At most one per `LOST_HOLDOFF`.
    Lost,
    /// Packets from the peer went missing, a fader let go among them
    /// perhaps. Every gap, without a hold-off.
    Missed,
    /// A MIDI message from the peer.
    Midi(Vec<u8>),
}

#[derive(Debug, Clone)]
pub struct Config {
    /// The session name sent with invitations and answers.
    pub name: String,
    /// Peers whose invitations are accepted.
    pub allow: Vec<Cidr>,
    /// The control port of a peer to invite.
    pub invite: Option<SocketAddr>,
}

/// An AppleMIDI command packet.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Invitation {
        version: u32,
        token: u32,
        ssrc: u32,
        name: String,
    },
    Accepted {
        token: u32,
        ssrc: u32,
        name: String,
    },
    Rejected {
        token: u32,
    },
    Bye {
        ssrc: u32,
    },
    Sync {
        ssrc: u32,
        count: u8,
        timestamps: [u64; 3],
    },
    Feedback {
        ssrc: u32,
        seq: u16,
    },
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// The command in `b`; `None` when `b` is no command or a malformed one.
fn parse(b: &[u8]) -> Option<Command> {
    if b.get(..2)? != [0xff, 0xff] {
        return None;
    }
    let name = |b: &[u8]| {
        let raw = b.get(16..).unwrap_or_default();
        let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        String::from_utf8_lossy(&raw[..end]).into_owned()
    };
    Some(match b.get(2..4)? {
        b"IN" => Command::Invitation {
            version: u32_at(b, 4)?,
            token: u32_at(b, 8)?,
            ssrc: u32_at(b, 12)?,
            name: name(b),
        },
        b"OK" => {
            u32_at(b, 4)?;
            Command::Accepted {
                token: u32_at(b, 8)?,
                ssrc: u32_at(b, 12)?,
                name: name(b),
            }
        }
        b"NO" => Command::Rejected {
            token: u32_at(b, 8)?,
        },
        b"BY" => Command::Bye {
            ssrc: u32_at(b, 12)?,
        },
        b"CK" => Command::Sync {
            ssrc: u32_at(b, 4)?,
            count: *b.get(8)?,
            timestamps: [u64_at(b, 12)?, u64_at(b, 20)?, u64_at(b, 28)?],
        },
        b"RS" => Command::Feedback {
            ssrc: u32_at(b, 4)?,
            seq: u16::from_be_bytes(b.get(8..10)?.try_into().ok()?),
        },
        _ => return None,
    })
}

/// IN, OK, NO or BY.
fn exchange(kind: &[u8; 2], token: u32, ssrc: u32, name: Option<&str>) -> Vec<u8> {
    let mut p = vec![0xff, 0xff, kind[0], kind[1]];
    p.extend_from_slice(&VERSION.to_be_bytes());
    p.extend_from_slice(&token.to_be_bytes());
    p.extend_from_slice(&ssrc.to_be_bytes());
    if let Some(name) = name {
        p.extend_from_slice(name.as_bytes());
        p.push(0);
    }
    p
}

fn sync(ssrc: u32, count: u8, timestamps: [u64; 3]) -> Vec<u8> {
    let mut p = vec![0xff, 0xff, b'C', b'K'];
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(&[count, 0, 0, 0]);
    for t in timestamps {
        p.extend_from_slice(&t.to_be_bytes());
    }
    p
}

fn feedback(ssrc: u32, seq: u16) -> Vec<u8> {
    let mut p = vec![0xff, 0xff, b'R', b'S'];
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&[0, 0]);
    p
}

#[derive(Debug, Clone)]
struct Peer {
    ip: IpAddr,
    control: SocketAddr,
    data: SocketAddr,
    token: u32,
    ssrc: u32,
    name: String,
}

#[derive(Debug)]
enum State {
    Idle {
        /// When the configured peer is invited next.
        invite_at: u64,
    },
    /// Inviting the configured peer: `peer` is set once its control
    /// port accepted, and the data port is invited.
    Inviting {
        token: u32,
        peer: Option<Peer>,
        tries: u32,
        next: u64,
    },
    /// The control port accepted an invitation; the data port's is due.
    Accepting { peer: Peer, since: u64 },
    Up {
        peer: Peer,
        initiator: bool,
        last_sync: u64,
        next_sync: u64,
        syncs: u32,
        unanswered: u32,
    },
}

pub struct Session {
    config: Config,
    ssrc: u32,
    random: Box<dyn FnMut() -> u32 + Send>,
    state: State,
    out: VecDeque<Transmit>,
    events: VecDeque<Event>,
    decoder: Decoder,
    /// The next sequence number sent.
    seq: u16,
    /// Recently sent sequence numbers and when.
    sent: VecDeque<(u16, u64)>,
    /// The last sequence number received.
    received: Option<u16>,
    feedback_due: bool,
    feedback_at: u64,
    lost_at: Option<u64>,
    /// When the peer's state was last sent whole (a session start or a
    /// loss), whether packets went out since, and when the peer's
    /// feedback last came.
    refreshed_at: u64,
    sent_since_refresh: bool,
    feedback_seen: Option<u64>,
    /// Invitations refused from sources not allowed since the last
    /// summary, the last of them, and when the summary was written.
    refused: u32,
    refused_from: Option<SocketAddr>,
    refused_reported: Option<u64>,
}

impl Session {
    pub fn new(config: Config, mut random: Box<dyn FnMut() -> u32 + Send>, now: u64) -> Session {
        let ssrc = random();
        let seq = random() as u16;
        Session {
            config,
            ssrc,
            random,
            state: State::Idle { invite_at: now },
            out: VecDeque::new(),
            events: VecDeque::new(),
            decoder: Decoder::default(),
            seq,
            sent: VecDeque::new(),
            received: None,
            feedback_due: false,
            feedback_at: 0,
            lost_at: None,
            refreshed_at: now,
            sent_since_refresh: false,
            feedback_seen: None,
            refused: 0,
            refused_from: None,
            refused_reported: None,
        }
    }

    pub fn is_up(&self) -> bool {
        matches!(self.state, State::Up { .. })
    }

    pub fn poll_transmit(&mut self) -> Option<Transmit> {
        self.out.pop_front()
    }

    pub fn poll_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    fn send(&mut self, socket: Socket, to: SocketAddr, bytes: Vec<u8>) {
        self.out.push_back(Transmit { socket, to, bytes });
    }

    /// The 100 microsecond units AppleMIDI timestamps count in.
    fn stamp(now: u64) -> u64 {
        now / 100
    }

    fn allowed(&self, ip: IpAddr) -> bool {
        self.config.allow.iter().any(|c| c.contains(ip))
    }

    /// When [`Session::tick`] has something to do next.
    pub fn deadline(&self) -> u64 {
        let state = match &self.state {
            State::Idle { invite_at } if self.config.invite.is_some() => *invite_at,
            State::Idle { .. } => u64::MAX,
            State::Inviting { next, .. } => *next,
            State::Accepting { since, .. } => since + ACCEPT_TIMEOUT,
            State::Up {
                initiator: true,
                next_sync,
                ..
            } => *next_sync,
            State::Up { last_sync, .. } => last_sync + SYNC_TIMEOUT,
        };
        let feedback = if self.feedback_due && self.is_up() {
            self.feedback_at + FEEDBACK_INTERVAL
        } else {
            u64::MAX
        };
        let refresh = if self.sent_since_refresh && self.is_up() {
            self.refreshed_at + REFRESH_INTERVAL
        } else {
            u64::MAX
        };
        state.min(feedback).min(refresh)
    }

    /// One datagram from `from` on `socket`.
    pub fn received(&mut self, socket: Socket, from: SocketAddr, bytes: &[u8], now: u64) {
        let from = SocketAddr::new(from.ip().to_canonical(), from.port());
        match parse(bytes) {
            Some(Command::Invitation {
                version,
                token,
                ssrc,
                name,
            }) => self.invited(socket, from, version, token, ssrc, name, now),
            Some(Command::Accepted { token, ssrc, name }) => {
                self.accepted(socket, from, token, ssrc, name, now)
            }
            Some(Command::Rejected { token }) => {
                if let State::Inviting { token: t, .. } = self.state
                    && t == token
                    && Some(from.ip()) == self.invite_ip()
                {
                    tracing::warn!(%from, "the peer refused the invitation");
                    self.state = State::Idle {
                        invite_at: now + INVITE_BACKOFF,
                    };
                }
            }
            Some(Command::Bye { ssrc }) => {
                if self
                    .peer()
                    .is_some_and(|p| p.ip == from.ip() && p.ssrc == ssrc)
                {
                    self.down("the peer said bye", false, now);
                }
            }
            Some(Command::Sync {
                ssrc,
                count,
                timestamps,
            }) => self.synced(from, ssrc, count, timestamps, now),
            Some(Command::Feedback { ssrc, seq }) => {
                if let State::Up { peer, .. } = &self.state
                    && peer.ip == from.ip()
                    && peer.ssrc == ssrc
                {
                    self.feedback(seq, now);
                }
            }
            None if socket == Socket::Data => self.midi(from, bytes, now),
            None => {}
        }
    }

    fn invite_ip(&self) -> Option<IpAddr> {
        self.config.invite.map(|a| a.ip().to_canonical())
    }

    fn peer(&self) -> Option<&Peer> {
        match &self.state {
            State::Up { peer, .. } | State::Accepting { peer, .. } => Some(peer),
            _ => None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn invited(
        &mut self,
        socket: Socket,
        from: SocketAddr,
        version: u32,
        token: u32,
        ssrc: u32,
        name: String,
        now: u64,
    ) {
        // A source not allowed gets no answer: it may be forged, or a
        // scan.
        if !self.allowed(from.ip()) {
            return self.refused(from, &name, now);
        }
        let no = exchange(b"NO", token, self.ssrc, None);
        if version != VERSION {
            tracing::warn!(%from, version, "refused an invitation of an unknown protocol version");
            return self.send(socket, from, no);
        }
        let ok = exchange(b"OK", token, self.ssrc, Some(&self.config.name));
        let same = |p: &Peer| p.ip == from.ip() && p.token == token && p.ssrc == ssrc;
        match (socket, &self.state) {
            // A copy of an invitation already accepted: its answer was lost.
            (Socket::Control, State::Accepting { peer, .. } | State::Up { peer, .. })
                if same(peer) && peer.control == from =>
            {
                self.send(socket, from, ok)
            }
            (Socket::Data, State::Up { peer, .. }) if same(peer) && peer.data == from => {
                self.send(socket, from, ok)
            }
            (Socket::Data, State::Accepting { peer, .. }) if same(peer) => {
                let mut peer = peer.clone();
                peer.data = from;
                tracing::info!(name = peer.name, control = %peer.control, data = %from, "session started");
                self.events.push_back(Event::Up {
                    name: peer.name.clone(),
                    peer: peer.control,
                });
                self.started(peer, false, now);
                self.send(socket, from, ok);
            }
            (Socket::Data, _) => self.send(socket, from, no),
            // One session at a time: a new one from the same address
            // replaces it (the peer restarted), another address waits.
            (Socket::Control, State::Accepting { peer, .. } | State::Up { peer, .. })
                if peer.ip != from.ip() =>
            {
                tracing::warn!(%from, busy = %peer.control, "refused an invitation while in a session");
                self.send(socket, from, no)
            }
            (Socket::Control, _) => {
                // No bye: it would reach the new session's ports.
                if self.is_up() {
                    self.down("the peer invited again", false, now);
                }
                self.state = State::Accepting {
                    peer: Peer {
                        ip: from.ip(),
                        control: from,
                        data: from,
                        token,
                        ssrc,
                        name,
                    },
                    since: now,
                };
                self.send(socket, from, ok);
            }
        }
    }

    fn refused(&mut self, from: SocketAddr, name: &str, now: u64) {
        tracing::debug!(%from, name, "ignored an invitation from a peer not allowed");
        self.refused += 1;
        self.refused_from = Some(from);
        if self
            .refused_reported
            .is_none_or(|t| now >= t + REFUSED_REPORT)
        {
            tracing::warn!(
                count = self.refused,
                last = %from,
                "ignored invitations from peers not allowed"
            );
            self.refused = 0;
            self.refused_reported = Some(now);
        }
    }

    fn started(&mut self, peer: Peer, initiator: bool, now: u64) {
        self.decoder.reset();
        self.received = None;
        self.sent.clear();
        self.feedback_due = false;
        self.lost_at = None;
        self.refreshed_at = now;
        self.sent_since_refresh = false;
        self.feedback_seen = None;
        self.state = State::Up {
            peer,
            initiator,
            last_sync: now,
            next_sync: now,
            syncs: 0,
            unanswered: 0,
        };
    }

    fn accepted(
        &mut self,
        socket: Socket,
        from: SocketAddr,
        token: u32,
        ssrc: u32,
        name: String,
        now: u64,
    ) {
        let Some(target) = self.config.invite else {
            return;
        };
        let State::Inviting { token: t, peer, .. } = &mut self.state else {
            return;
        };
        if *t != token || from.ip() != target.ip().to_canonical() {
            return;
        }
        match (socket, peer.take()) {
            (Socket::Control, None) => {
                let data = SocketAddr::new(from.ip(), target.port().wrapping_add(1));
                let p = Peer {
                    ip: from.ip(),
                    control: from,
                    data,
                    token,
                    ssrc,
                    name,
                };
                let invite = exchange(b"IN", token, self.ssrc, Some(&self.config.name));
                self.state = State::Inviting {
                    token,
                    peer: Some(p),
                    tries: 1,
                    next: now + INVITE_INTERVAL,
                };
                self.send(Socket::Data, data, invite);
            }
            (Socket::Data, Some(mut p)) if p.ssrc == ssrc => {
                p.data = from;
                tracing::info!(control = %p.control, data = %from, "session started");
                self.events.push_back(Event::Up {
                    name: p.name.clone(),
                    peer: p.control,
                });
                self.started(p, true, now);
                self.tick(now);
            }
            (_, p) => {
                if let State::Inviting { peer, .. } = &mut self.state {
                    *peer = p;
                }
            }
        }
    }

    fn synced(&mut self, from: SocketAddr, ssrc: u32, count: u8, ts: [u64; 3], now: u64) {
        let me = self.ssrc;
        let State::Up {
            peer,
            last_sync,
            unanswered,
            ..
        } = &mut self.state
        else {
            return;
        };
        if peer.ip != from.ip() || peer.ssrc != ssrc {
            return;
        }
        *last_sync = now;
        let answer = match count {
            0 => Some(sync(me, 1, [ts[0], Self::stamp(now), 0])),
            1 => {
                *unanswered = 0;
                tracing::debug!(
                    round_trip_ms = Self::stamp(now).saturating_sub(ts[0]) as f64 / 10.0,
                    "clock sync"
                );
                Some(sync(me, 2, [ts[0], ts[1], Self::stamp(now)]))
            }
            _ => None,
        };
        if let Some(a) = answer {
            self.send(Socket::Data, from, a);
        }
    }

    fn midi(&mut self, from: SocketAddr, bytes: &[u8], now: u64) {
        let Some(peer) = self.peer().filter(|p| p.ip == from.ip()) else {
            return;
        };
        if !self.is_up() {
            return;
        }
        // Another stream, a copy or a late packet: dropped before its
        // SysEx segments can mix into this stream's.
        if let Some(h) = bytes.get(..12) {
            let seq = u16::from_be_bytes([h[2], h[3]]);
            let other = u32_at(h, 8) != Some(peer.ssrc);
            if other
                || self
                    .received
                    .is_some_and(|r| seq.wrapping_sub(r) as i16 <= 0)
            {
                return;
            }
        }
        let (header, messages) = match self.decoder.decode(bytes) {
            Ok(decoded) => decoded,
            Err(e) => {
                tracing::debug!(%from, "refused a packet: {}", e.0);
                return;
            }
        };
        if let Some(last) = self.received {
            let ahead = header.seq.wrapping_sub(last);
            if ahead > 1 {
                tracing::debug!(missed = ahead - 1, "packets from the peer lost");
                self.events.push_back(Event::Missed);
                self.lost(now);
            }
        }
        self.received = Some(header.seq);
        self.feedback_due = true;
        self.events.extend(messages.into_iter().map(Event::Midi));
    }

    /// The peer received everything up to `seq`.
    fn feedback(&mut self, seq: u16, now: u64) {
        self.feedback_seen = Some(now);
        let Some(at) = self.sent.iter().position(|(s, _)| *s == seq) else {
            return;
        };
        let lost = self
            .sent
            .get(at + 1)
            .is_some_and(|(_, t)| now >= t + FEEDBACK_GRACE);
        // The acknowledged one stays, so that the same feedback later
        // still finds what followed it.
        self.sent.drain(..at);
        if lost {
            tracing::debug!("packets to the peer lost");
            self.lost(now);
        }
    }

    fn lost(&mut self, now: u64) {
        if self.lost_at.is_none_or(|t| now >= t + LOST_HOLDOFF) {
            self.lost_at = Some(now);
            self.refreshed_at = now;
            self.sent_since_refresh = false;
            self.events.push_back(Event::Lost);
        }
    }

    /// Ends the session; says bye to the peer when `bye`.
    fn down(&mut self, reason: &'static str, bye: bool, now: u64) {
        if let State::Up { peer, .. } | State::Accepting { peer, .. } = &self.state {
            let peer = peer.clone();
            if bye {
                let by = exchange(b"BY", peer.token, self.ssrc, None);
                self.send(Socket::Control, peer.control, by.clone());
                self.send(Socket::Data, peer.data, by);
            }
            if self.is_up() {
                tracing::info!(peer = %peer.control, reason, "session ended");
                self.events.push_back(Event::Down { reason });
            }
        }
        self.decoder.reset();
        self.feedback_due = false;
        self.state = State::Idle {
            invite_at: now + INVITE_BACKOFF,
        };
    }

    /// Sends `commands` to the peer in one packet; false without a
    /// session.
    pub fn send_midi(&mut self, commands: &[Vec<u8>], now: u64) -> bool {
        let State::Up { peer, .. } = &self.state else {
            return false;
        };
        let to = peer.data;
        let header = Header {
            seq: self.seq,
            timestamp: Self::stamp(now) as u32,
            ssrc: self.ssrc,
        };
        if self.sent.len() == SENT_KEPT {
            self.sent.pop_front();
        }
        self.sent.push_back((self.seq, now));
        self.sent_since_refresh = true;
        self.seq = self.seq.wrapping_add(1);
        self.send(Socket::Data, to, payload::encode(header, commands));
        true
    }

    /// Ends the session with a bye, as when the bridge stops.
    pub fn close(&mut self, now: u64) {
        self.down("closed", true, now);
    }

    /// Timers: invitations, clock sync, timeouts and feedback.
    pub fn tick(&mut self, now: u64) {
        let me = self.ssrc;
        match &mut self.state {
            State::Idle { invite_at } => {
                if let Some(target) = self.config.invite
                    && now >= *invite_at
                {
                    let token = (self.random)();
                    self.state = State::Inviting {
                        token,
                        peer: None,
                        tries: 1,
                        next: now + INVITE_INTERVAL,
                    };
                    let invite = exchange(b"IN", token, me, Some(&self.config.name));
                    self.send(Socket::Control, target, invite);
                }
            }
            State::Inviting {
                token,
                peer,
                tries,
                next,
            } => {
                if now < *next {
                    return;
                }
                if *tries >= INVITE_TRIES {
                    tracing::info!("no answer to the invitation");
                    self.state = State::Idle {
                        invite_at: now + INVITE_BACKOFF,
                    };
                    return;
                }
                *tries += 1;
                *next = now + INVITE_INTERVAL;
                let invite = exchange(b"IN", *token, me, Some(&self.config.name));
                let (socket, to) = match peer {
                    Some(p) => (Socket::Data, p.data),
                    None => (Socket::Control, self.config.invite.expect("inviting")),
                };
                self.send(socket, to, invite);
            }
            State::Accepting { since, .. } => {
                if now >= *since + ACCEPT_TIMEOUT {
                    tracing::info!("the data port's invitation did not come");
                    self.state = State::Idle { invite_at: now };
                }
            }
            State::Up {
                peer,
                initiator: true,
                next_sync,
                syncs,
                unanswered,
                ..
            } => {
                if now >= *next_sync {
                    if *unanswered >= SYNC_UNANSWERED {
                        return self.down("no answer to clock sync", true, now);
                    }
                    *unanswered += 1;
                    *syncs += 1;
                    *next_sync = now
                        + if *syncs < SYNC_FIRST {
                            SYNC_FIRST_INTERVAL
                        } else {
                            SYNC_INTERVAL
                        };
                    let to = peer.data;
                    self.send(Socket::Data, to, sync(me, 0, [Self::stamp(now), 0, 0]));
                }
            }
            State::Up { last_sync, .. } => {
                if now >= *last_sync + SYNC_TIMEOUT {
                    return self.down("no clock sync", true, now);
                }
            }
        }
        if self.feedback_due
            && now >= self.feedback_at + FEEDBACK_INTERVAL
            && let (Some(seq), Some(peer)) = (self.received, self.peer())
        {
            let to = peer.control;
            self.feedback_due = false;
            self.feedback_at = now;
            self.send(Socket::Control, to, feedback(me, seq));
        }
        // No feedback said what arrived since the last refresh: send the
        // whole state again, in case some of it did not.
        if self.is_up()
            && self.sent_since_refresh
            && now >= self.refreshed_at + REFRESH_INTERVAL
            && self.feedback_seen.is_none_or(|t| t < self.refreshed_at)
        {
            tracing::debug!("no feedback from the peer: refreshing its state");
            self.lost_at = Some(now);
            self.refreshed_at = now;
            self.sent_since_refresh = false;
            self.events.push_back(Event::Lost);
        } else if self.is_up() && now >= self.refreshed_at + REFRESH_INTERVAL {
            self.refreshed_at = now;
            self.sent_since_refresh = false;
        }
    }
}

#[cfg(test)]
mod tests;
