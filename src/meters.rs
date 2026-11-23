//! Meter frames (api/ws/server-message.schema.json `MeterFrame`, format
//! 4): a 12-byte header, the replica id, then Int16 values in shape
//! order, the duck groups' readings last.
//!
//! A channel's peak, true peak and momentary loudness are an engine meter
//! of its input at its `meterTap`: input reads the meter after the
//! channel delay and before the strip (samplez `preStripMeter`), so trim,
//! filters and dynamics do not reach it, and null while there is none.
//! That meter runs only while the tap is input
//! ([`crate::mix::pre_strip_meters`]), and measures afresh from the switch
//! to input. preFader reads the meter after
//! the strip; postFader that one times the channel's fader, mute and VCA
//! gain. The tap gains are those of the state last applied, while the
//! engine ramps a change from its index and meters in 100 ms windows, so
//! for a window or so after a fader move the levels lead the audio. A
//! bus, aux or mix-minus output reads the meters of the first full-width
//! output it is the source of, or the engine meter on matrix columns of
//! its own when no full-width output carries it. A monitor output reads
//! its engine meter, whose matrix columns carry what it plays, the listen
//! sum while anything solos, before its level, dim, cut, mono and speaker
//! mutes and solos ([`crate::mix`]), whether an output carries it or
//! not. A bus, aux or mix-minus output of the stereo layout reads the
//! engine's correlation of its L and R legs, any other null. A level of
//! digital silence reads the floor, -120 dB; null is a value not
//! measured.
//!
//! A duck group reads the engine's `DuckMeter` of its group over the
//! last meter slice of 10 ms it was computed in: the largest key level,
//! the largest envelope and whether its detector was engaged.

use samplez::{DuckMeter, InputMeters, OutputMeters, Status};

use crate::mix::MeterTap;
use crate::shape::Shape;

const MAGIC: u8 = 0x4d;
const VERSION: u8 = 4;
/// Full envelope, full depth: the envelope 0 to 1 in ten-thousandths.
const ENVELOPE_FULL: f64 = 10_000.0;
/// No value: not measured.
const NULL: i16 = i16::MIN;
/// Digital silence: -120 dB, the floor of a level.
const FLOOR: i16 = -12_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Channel,
    Bus,
    Monitor,
}

/// Where an element's meters come from.
#[derive(Clone, Copy)]
enum Source {
    /// Input slot `slot`.
    Channel(usize),
    Output(usize),
    /// An engine meter, numbered as the engine's `meters`.
    Meter(usize),
}

/// The meters a frame is made of.
pub struct Readings<'a> {
    pub outputs: Vec<&'a OutputMeters>,
    pub meters: Vec<&'a OutputMeters>,
    pub inputs: Vec<&'a InputMeters>,
    /// Each input's meter before its strip, as `inputs`.
    pub pre_strip: Vec<Option<&'a InputMeters>>,
    /// Each duck group's readings, numbered as the shape's ducks.
    pub ducks: &'a [DuckMeter],
    /// Each channel's tap gains ([`crate::mix::meter_taps`]).
    pub taps: &'a [MeterTap],
}

impl<'a> Readings<'a> {
    pub fn of(status: &'a Status, taps: &'a [MeterTap]) -> Readings<'a> {
        Readings {
            outputs: status.outputs.iter().map(|o| &o.meters).collect(),
            meters: status.meters.iter().map(|m| &m.meters).collect(),
            inputs: status.inputs.iter().map(|i| &i.meters).collect(),
            pre_strip: (status.inputs.iter())
                .map(|i| i.pre_strip_meters.as_ref())
                .collect(),
            ducks: &status.ducks,
            taps,
        }
    }
}

/// The order and size of a frame's values for one shape.
pub struct Layout {
    /// Kind, legs and source of each element.
    elements: Vec<(Kind, usize, Source)>,
    /// The id of each element, as `elements`.
    ids: Vec<String>,
    /// The shape's outputs: the engine's meters are numbered after them.
    outputs: usize,
    /// Duck groups, one engine group each in shape order.
    ducks: usize,
    /// FNV-1a 32-bit of the layout text.
    pub fingerprint: u32,
}

impl Layout {
    pub fn of(shape: &Shape) -> Layout {
        let meters = shape.meter_columns();
        let meter = |id: &str| {
            let at = meters.iter().position(|m| m.0 == id);
            Source::Meter(at.expect("a monitor output, or an element no output carries, has one"))
        };
        let source = |id: &str, width: usize| match shape.full_output(id, width) {
            Some(o) => Source::Output(o),
            None => meter(id),
        };
        let mut text = String::new();
        let mut elements = Vec::new();
        let mut ids = Vec::new();
        let mut add = |kind, section: &str, id: &str, legs: usize, source| {
            text.push_str(&format!("{section}/{id}/{legs};"));
            elements.push((kind, legs, source));
            ids.push(id.to_owned());
        };
        for (slot, c) in shape.channels.iter().enumerate() {
            add(Kind::Channel, "c", &c.id, c.width, Source::Channel(slot));
        }
        for b in &shape.buses {
            add(Kind::Bus, "b", &b.id, b.width, source(&b.id, b.width));
        }
        for a in &shape.auxes {
            add(Kind::Bus, "a", &a.id, a.width, source(&a.id, a.width));
        }
        for m in &shape.mix_minus {
            add(Kind::Bus, "m", &m.id, m.width, source(&m.id, m.width));
        }
        for m in &shape.monitor_outputs {
            add(Kind::Monitor, "o", &m.id, m.width, meter(&m.id));
        }
        for d in &shape.ducks {
            text.push_str(&format!("d/{}/1;", d.id));
        }
        Layout {
            elements,
            ids,
            outputs: shape.outputs.len(),
            ducks: shape.ducks.len(),
            fingerprint: fnv1a(&text),
        }
    }

    /// The number the engine gives the measurement of bus, aux or
    /// mix-minus `id`: its first full-width output's, or after the
    /// outputs its engine meter's ([`samplez::Entry::ResetLoudness`]).
    pub fn loudness_meter(&self, id: &str) -> Option<usize> {
        let at = self.ids.iter().position(|i| i == id)?;
        match self.elements[at] {
            (Kind::Bus, _, Source::Output(o)) => Some(o),
            (Kind::Bus, _, Source::Meter(m)) => Some(self.outputs + m),
            _ => None,
        }
    }

    /// The index in a frame's values of the first value of element `id`
    /// (a channel, bus, aux, mix-minus or monitor output), its peak of
    /// the first leg: the elements come in shape order, a channel with
    /// 2n + 1 values for n legs, a monitor output n and any other 3n + 8.
    pub fn offset(&self, id: &str) -> Option<usize> {
        let at = self.ids.iter().position(|i| i == id)?;
        Some(
            self.elements[..at]
                .iter()
                .map(|&(kind, n, _)| match kind {
                    Kind::Channel => 2 * n + 1,
                    Kind::Monitor => n,
                    Kind::Bus => 3 * n + 8,
                })
                .sum(),
        )
    }

    /// The frame numbered `sequence` of `replica` from the engine's
    /// readings.
    pub fn frame(&self, replica: &str, sequence: u32, r: &Readings<'_>) -> Vec<u8> {
        let len = replica.len().min(32);
        let mut b = vec![MAGIC, VERSION, len as u8, 0];
        b.extend_from_slice(&sequence.to_le_bytes());
        b.extend_from_slice(&self.fingerprint.to_le_bytes());
        b.extend_from_slice(&replica.as_bytes()[..len]);
        if len % 2 == 1 {
            b.push(0);
        }
        let mut put = |v: i16| b.extend_from_slice(&v.to_le_bytes());
        let legs = |v: Option<&Vec<f32>>, i: usize, g: f64| {
            v.and_then(|v| v.get(i))
                .map_or(NULL, |v| level(f64::from(*v) * g))
        };
        for &(kind, n, source) in &self.elements {
            if let Source::Channel(slot) = source {
                let tap = r.taps.get(slot);
                let g = tap.map_or(1.0, |t| t.meter);
                let input = if tap.is_some_and(|t| t.pre_strip) {
                    r.pre_strip.get(slot).copied().flatten()
                } else {
                    r.inputs.get(slot).copied()
                };
                for i in 0..n {
                    put(legs(input.map(|m| &m.peak), i, g));
                }
                for i in 0..n {
                    put(legs(input.map(|m| &m.true_peak), i, g));
                }
                // Loudness scales with the power, g squared; a tap that
                // passes nothing reads silence, as its peak does, once the
                // input is measured.
                let gain_db = 20.0 * g.log10();
                put(match input.and_then(|m| m.momentary) {
                    Some(_) if g <= 0.0 => FLOOR,
                    Some(l) => centi(l + gain_db),
                    None => NULL,
                });
                continue;
            }
            let m = match source {
                Source::Output(o) => r.outputs.get(o),
                Source::Meter(m) => r.meters.get(m),
                Source::Channel(_) => unreachable!("handled above"),
            }
            .copied();
            for i in 0..n {
                put(legs(m.map(|m| &m.peak), i, 1.0));
            }
            if kind == Kind::Monitor {
                continue;
            }
            for i in 0..n {
                put(legs(m.map(|m| &m.true_peak), i, 1.0));
            }
            for i in 0..n {
                put(legs(m.map(|m| &m.rms), i, 1.0));
            }
            let l = |of: fn(&OutputMeters) -> Option<f64>| m.and_then(of).map_or(NULL, centi);
            put(l(|m| m.momentary));
            put(l(|m| m.short_term));
            put(l(|m| m.integrated));
            put(l(|m| m.range));
            put(l(|m| m.max_momentary));
            put(l(|m| m.max_short_term));
            put(m.and_then(|m| m.correlation).map_or(NULL, centi));
            put(m.map_or(0, flags));
        }
        for g in 0..self.ducks {
            let [key, envelope, flags] = duck(r.ducks.get(g));
            put(key);
            put(envelope);
            put(flags);
        }
        b
    }
}

/// A duck group's values: its key level in centi-dBFS, null while the
/// group is not in use; its envelope in ten-thousandths, 0 while not in
/// use; flags, bit 0 engaged (triggered or holding).
fn duck(m: Option<&DuckMeter>) -> [i16; 3] {
    match m {
        Some(m) if m.slice.is_some() => {
            let envelope = (f64::from(m.envelope) * ENVELOPE_FULL + 0.5).floor();
            [
                level(f64::from(m.key)),
                envelope.clamp(0.0, ENVELOPE_FULL) as i16,
                i16::from(m.engaged),
            ]
        }
        _ => [NULL, 0, 0],
    }
}

/// Samples of running measurement after which the loudness range counts
/// as stable (model `Loudness.lraStable`).
const LRA_SETTLED: u64 = 60 * 48_000;
/// Samples in one meter window.
const WINDOW: u64 = 4_800;

/// Bit 0 `integratedValid`: an integrated loudness is measured; a gap
/// or a lost slice does not clear it, as samplez measures across them.
/// Bit 1 `lraStable`: that, and 60 s measured since.
fn flags(m: &OutputMeters) -> i16 {
    let measured = match (m.window, m.since) {
        (Some(w), Some(since)) => ((w + 1) * WINDOW).saturating_sub(since),
        _ => 0,
    };
    let valid = m.integrated.is_some();
    i16::from(valid) | i16::from(valid && measured >= LRA_SETTLED) << 1
}

/// A full-scale-1.0 level in centi-dB; silence is the floor, NaN null.
fn level(v: f64) -> i16 {
    if v > 0.0 {
        centi(20.0 * v.log10())
    } else if v == 0.0 {
        FLOOR
    } else {
        NULL
    }
}

/// Centi-units of a level, loudness or loudness range; NaN and
/// -Infinity, a loudness of silence, are null.
fn centi(db: f64) -> i16 {
    if db.is_nan() || db == f64::NEG_INFINITY {
        return NULL;
    }
    (db * 100.0 + 0.5).floor().clamp(-32767.0, 32767.0) as i16
}

fn fnv1a(text: &str) -> u32 {
    text.bytes().fold(2_166_136_261u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(16_777_619)
    })
}

#[cfg(test)]
mod tests;
