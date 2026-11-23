//! The live state as the engine's gain matrix: one row per input channel
//! (the legs of each channel in shape order, then those of the line-up
//! tone's generators, [`tone_widths`], then the pre-strip rows of each
//! channel with a direct out, [`pre_rows`]) and one column per output channel (the legs of
//! each shape output in shape order), then per leg of each element the
//! engine meters on columns of its own ([`Shape::meter_columns`]). A
//! monitor output's meter columns carry it before its level, dim, cut,
//! mono and speaker mutes and solos ([`columns`]).
//!
//! What a matrix coefficient can carry is applied: fader, mute, VCA
//! offsets and mutes, AFV open or closed, pan of a mono leg and the
//! fold-down of a wider channel, bus routes, sends
//! and their levels, bus, aux and mix-minus faders, mutes and polarity,
//! the mix-minus exclusions and talkback interrupts, and the monitor's
//! source or the channel and VCA PFL and AFL listens, downmix, level,
//! dim, talkback dim, cut, mono and speaker mutes and solos. Everything
//! else in the
//! state is the input's strip ([`crate::strip`]), a duck group
//! ([`crate::duck`]) or is kept and not applied. [`targets`] adds the
//! strips, the duck groups and their masks, and each input's flow.
//!
//! Every value is computed with `libm`, not the host's math library, so
//! replicas on other hosts derive the same bits from the same state.

use std::collections::BTreeSet;

use samplez::Targets;
use samplez_dsp::ToneParams;
use serde::Serialize;
use serde_json::Value;

use crate::meters::Layout;
use crate::shape::Shape;
use crate::state::tokens;
use crate::{duck, strip};

/// The start of the names of the engine inputs that play the line-up
/// tone (`/tone`, [`tone_input`]), after the channels' inputs: no channel
/// takes one, an id having no `/`.
pub const TONE_INPUT: &str = "/tone";
/// The line-up tone's frequency, Hz.
const TONE_HZ: u32 = 1000;

/// A fader at this level or below is closed.
const CLOSED_DB: f64 = -90.0;
/// The most a channel's fader and VCA offsets add up to.
const MAX_CHANNEL_DB: f64 = 20.0;

/// The gain of channel `id` after its fader, mute and VCAs, before AFV,
/// the same on every leg.
pub fn post_fader(state: &Value, id: &str) -> f64 {
    channel(state, id, Tap::PostFader)
}

/// Linear gain of `db`, 0 at [`CLOSED_DB`] and below.
pub fn gain(db: f64) -> f64 {
    if db <= CLOSED_DB {
        0.0
    } else {
        libm::pow(10.0, db / 20.0)
    }
}

fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

fn on(v: &Value) -> bool {
    v.as_bool().unwrap_or(false)
}

/// The channel mix-minus `id` carries in place of its mix while its
/// talkback interrupt is active, per model `Interrupt`; an interrupt with
/// no source changes nothing.
pub(crate) fn talkback<'a>(state: &'a Value, id: &str) -> Option<&'a str> {
    let interrupt = &state["mixMinus"][id]["interrupt"];
    on(&interrupt["active"])
        .then(|| interrupt["source"].as_str())
        .flatten()
}

/// Whether a mix-minus of `shape` carries its talkback, which dims the
/// monitor when `talkbackDim` is on.
fn talking(shape: &Shape, state: &Value) -> bool {
    (shape.mix_minus.iter()).any(|m| talkback(state, &m.id).is_some())
}

/// Where a channel's signal is taken.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tap {
    /// The selected input after its delay and before its strip. A
    /// channel meter reads it there ([`meter_taps`]) and a direct out
    /// takes it from the channel's pre-strip rows ([`pre_rows`]); the
    /// channel's own rows carry it after its strip.
    Input,
    /// After the strip: trim, leg trim, polarity and processing.
    PreFader,
    /// After fader, mute and VCAs, before AFV.
    PostFader,
    /// After AFV: what buses and post-fader sends get.
    Mix,
}

/// The gain of channel `id` at `tap`, the same on every leg.
fn channel(state: &Value, id: &str, tap: Tap) -> f64 {
    // Trim, leg trim, polarity and the processing are the input's strip,
    // which runs before the matrix; the input tap is on rows of its own,
    // ahead of it.
    if matches!(tap, Tap::Input | Tap::PreFader) {
        return 1.0;
    }
    let c = &state["channels"][id];
    let mut g = 1.0;
    let mut db = num(&c["fader"]);
    let mut muted = on(&c["mute"]);
    if let Some(vcas) = state["vcas"].as_object() {
        for v in vcas.values() {
            let member = v["members"]
                .as_array()
                .is_some_and(|m| m.iter().any(|m| m == id));
            if member {
                db += num(&v["offset"]);
                muted |= on(&v["mute"]);
            }
        }
    }
    if muted || num(&c["fader"]) <= CLOSED_DB {
        return 0.0;
    }
    g *= gain(db.min(MAX_CHANNEL_DB));
    if tap == Tap::PostFader {
        return g;
    }
    let afv = &state["afv"][id];
    if on(&afv["enabled"]) && !on(&afv["open"]) {
        g *= gain(num(&afv["offLevel"]));
    }
    g
}

/// Whether any channel or VCA solos, and whether any of them is in PFL:
/// per model `MonitorState` the monitor then plays the listen sum, and
/// per `MonitorMeter` PFL takes priority over AFL, so while any PFL is
/// on only the PFL listens are summed. A VCA solos even without members,
/// which makes the listen sum silent.
pub(crate) fn solos(state: &Value) -> (bool, bool) {
    let all = ["channels", "vcas"]
        .into_iter()
        .filter_map(|g| state[g].as_object())
        .flat_map(|m| m.values())
        .map(|e| e["solo"].as_str().unwrap_or("off"));
    all.fold((false, false), |(any, pfl), mode| {
        (any || mode != "off", pfl || mode == "pfl")
    })
}

/// Whether channel `id` listens in PFL and in AFL: per model `VcaState`
/// a VCA's solo listens to every member as if the member had it set, so
/// its own solo and that of each VCA it is a member of add up.
fn listens(state: &Value, id: &str) -> (bool, bool) {
    let own = std::iter::once(&state["channels"][id]["solo"]);
    let vcas = (state["vcas"].as_object().into_iter())
        .flat_map(|v| v.values())
        .filter(|v| {
            v["members"]
                .as_array()
                .is_some_and(|m| m.iter().any(|m| m == id))
        })
        .map(|v| &v["solo"]);
    own.chain(vcas).fold((false, false), |(pfl, afl), mode| {
        (pfl || mode == "pfl", afl || mode == "afl")
    })
}

/// The gain from leg `i` of a `from`-leg signal to leg `j` of a `to`-leg
/// element (model `Layout`: 1 mono, 2 stereo L R, 6 5.1 L R C LFE Ls Rs,
/// 8 7.1 L R C LFE Ls Rs Lrs Rrs), per model `PanState`: a mono signal
/// pans onto the first two legs at constant power by `pan` (-1 to 1); a
/// stereo one onto an element of two legs or more balances: `pan` lowers
/// the opposite leg linearly, to silence at the edge, and leaves the
/// other at unity, so the centre passes both legs; any other signal
/// ignores `pan`. A signal maps leg to leg by label onto an element as
/// wide or wider, and folds down onto a narrower one.
pub fn pan(from: usize, to: usize, i: usize, j: usize, pan: f64) -> f64 {
    match (from, to) {
        (1, 1) => 1.0,
        (2, 2..) => {
            let p = pan.clamp(-1.0, 1.0);
            let balance = if i == 0 { 1.0 - p } else { 1.0 + p };
            fold(from, to, i, j) * balance.min(1.0)
        }
        (1, _) => {
            let theta = (pan.clamp(-1.0, 1.0) + 1.0) * std::f64::consts::FRAC_PI_4;
            match j {
                0 => libm::cos(theta),
                1 => libm::sin(theta),
                _ => 0.0,
            }
        }
        // Onto mono: the stereo fold summed at -6 dB, so a correlated
        // pair keeps its level. The contract does not define it.
        (_, 1) => (fold(from, 2, i, 0) + fold(from, 2, i, 1)) / 2.0,
        _ => fold(from, to, i, j),
    }
}

/// Where a channel sits in the field its panner places it in (model
/// `PanState`): `lr` left to right, `frontRear` front to rear (`fr`
/// here), both -1 to 1, the front `spread` (`divergence` here) and the
/// `lfeSend` (`lfe`) in dB. The model's `fr`, `divergence` and `lfe` are
/// deprecated and never read.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Place {
    lr: f64,
    fr: f64,
    divergence: f64,
    lfe: f64,
}

impl Place {
    /// Centred at the front with full divergence, no LFE: the L/R pair a
    /// mono signal was on a surround element before the field, where a
    /// send, a direct out and a PFL listen put it.
    const FRONT: Place = Place {
        lr: 0.0,
        fr: -1.0,
        divergence: 1.0,
        lfe: -90.0,
    };

    /// A position on the left-right axis alone, at the front.
    fn lr(lr: f64) -> Place {
        Place { lr, ..Place::FRONT }
    }

    fn of(pan: &Value) -> Place {
        Place {
            lr: num(&pan["lr"]),
            fr: pan["frontRear"].as_f64().unwrap_or(Place::FRONT.fr),
            divergence: pan["spread"].as_f64().unwrap_or(Place::FRONT.divergence),
            lfe: pan["lfeSend"].as_f64().unwrap_or(Place::FRONT.lfe),
        }
    }
}

/// The gain of leg `j` of a 5.1 (L R C LFE Ls Rs) or 7.1 (L R C LFE Ls
/// Rs Lrs Rrs) element for a mono signal at `place`. The field is the
/// ITU-R BS.775 layout, L -30, C 0, R +30, Ls -110 and Rs +110 degrees
/// (7.1: sides at +-90 and rear surrounds at +-135), and every law is
/// constant power:
///
/// - Front: `lr` pans pairwise between adjacent speakers, L and C for
///   the left half, C and R for the right, so the centre of the field
///   is the C leg alone. `divergence` blends that law with a plain
///   constant-power pan between L and R, the phantom image: none of it
///   at 0, all of it at 1, a power split between.
/// - Rear: `lr` pans between the surrounds Ls and Rs; on 7.1 the side
///   pair and the rear pair take half the power each.
/// - `fr` crossfades the front into the rear, -1 the front alone, 1
///   the rear alone, and the middle half of each.
/// - The LFE leg takes the `lfe` send as an independent gain, silent at
///   -90 dB; it is no part of the pan.
fn field_gain(to: usize, j: usize, place: &Place) -> f64 {
    use std::f64::consts::FRAC_PI_2;
    let lr = place.lr.clamp(-1.0, 1.0);
    if j == 3 {
        return gain(place.lfe);
    }
    // (cos, sin) of the position along a pair, 0 at its first speaker.
    let pair = |at: f64| {
        let a = at.clamp(0.0, 1.0) * FRAC_PI_2;
        (libm::cos(a), libm::sin(a))
    };
    let front = {
        let (left, centre, right) = if lr <= 0.0 {
            let (a, b) = pair(lr + 1.0);
            (a, b, 0.0)
        } else {
            let (a, b) = pair(lr);
            (0.0, a, b)
        };
        let (keep, spread) = pair(place.divergence);
        let (phantom_l, phantom_r) = pair((lr + 1.0) / 2.0);
        match j {
            0 => libm::sqrt(keep * keep * left * left + spread * spread * phantom_l * phantom_l),
            1 => libm::sqrt(keep * keep * right * right + spread * spread * phantom_r * phantom_r),
            2 => keep * centre,
            _ => 0.0,
        }
    };
    let (rl, rr) = pair((lr + 1.0) / 2.0);
    let rear = match (j, to) {
        (4, 6) | (4, 8) => rl,
        (5, 6) | (5, 8) => rr,
        _ => 0.0,
    };
    let rear = if to == 8 {
        // The side pair and the rear pair, half the power each.
        match j {
            4 | 5 => rear / std::f64::consts::SQRT_2,
            6 => rl / std::f64::consts::SQRT_2,
            7 => rr / std::f64::consts::SQRT_2,
            _ => 0.0,
        }
    } else {
        rear
    };
    let (wf, wr) = pair((place.fr.clamp(-1.0, 1.0) + 1.0) / 2.0);
    wf * front + wr * rear
}

/// How a stereo channel or monitor uses its legs (model `StereoImage`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Image {
    Normal,
    LeftToBoth,
    RightToBoth,
    Swap,
}

impl Image {
    fn of(v: &Value) -> Image {
        match v.as_str() {
            Some("leftToBoth") => Image::LeftToBoth,
            Some("rightToBoth") => Image::RightToBoth,
            Some("swap") => Image::Swap,
            _ => Image::Normal,
        }
    }

    /// The share of leg `k` of the signal that leg `i` of the imaged
    /// stereo signal carries.
    fn share(self, i: usize, k: usize) -> f64 {
        let on = match self {
            Image::Normal => i == k,
            Image::LeftToBoth => k == 0,
            Image::RightToBoth => k == 1,
            Image::Swap => i != k,
        };
        f64::from(u8::from(on))
    }
}

/// [`pan`] of a signal at `place`, a mono one onto a surround element by
/// [`field_gain`].
fn pan_place(from: usize, to: usize, i: usize, j: usize, place: &Place) -> f64 {
    if from == 1 && to >= 6 {
        field_gain(to, j, place)
    } else {
        pan(from, to, i, j, place.lr)
    }
}

/// [`pan_place`] of a stereo signal whose legs `image` has put to use
/// first.
fn pan_image(from: usize, to: usize, i: usize, j: usize, place: &Place, image: Image) -> f64 {
    if from == 2 && image != Image::Normal {
        (0..2)
            .map(|a| image.share(a, i) * pan_place(from, to, a, j, place))
            .sum()
    } else {
        pan_place(from, to, i, j, place)
    }
}

/// Leg to leg by label when `to` is at least as wide as `from`; else 5.1
/// and 7.1 onto stereo by the ITU-R BS.775 Lo/Ro equations (centre and
/// surrounds at -3 dB, LFE dropped) and 7.1 onto 5.1 by summing each side
/// and back surround pair at -3 dB (ETSI TS 103 190-1 Table 219).
fn fold(from: usize, to: usize, i: usize, j: usize) -> f64 {
    const H: f64 = std::f64::consts::FRAC_1_SQRT_2;
    match (from, to) {
        (6 | 8, 2) => match (i, j) {
            (0, 0) | (1, 1) => 1.0,
            (2, _) | (4 | 6, 0) | (5 | 7, 1) => H,
            _ => 0.0,
        },
        (8, 6) => match (i, j) {
            (4, 4) | (5, 5) | (6, 4) | (7, 5) => H,
            _ => f64::from(u8::from(i == j)),
        },
        _ => f64::from(u8::from(i == j)),
    }
}

/// The output and meter channels' gains for each input channel:
/// `[input][column]`, the line-up tone generators' legs last. Each
/// channel's gain at each tap and each element's fader, mute and polarity
/// are taken once, then multiplied into every coefficient in the same
/// order of operations as one coefficient at a time, so the bits are
/// those.
pub fn matrix(shape: &Shape, state: &Value) -> Vec<Vec<f32>> {
    let factors: Vec<Factors> = (shape.channels.iter())
        .map(|c| Factors::of(state, &c.id))
        .collect();
    gains(shape, state, &factors)
}

/// [`matrix`] with each channel's `factors` given.
fn gains(shape: &Shape, state: &Value, factors: &[Factors]) -> Vec<Vec<f32>> {
    let all: Vec<usize> = (0..shape.channels.len()).collect();
    let mix = Mix::of(shape, state, &all, |slot| factors[slot]);
    let columns = columns(shape);
    let width = columns.iter().map(|c| c.1).sum();
    let tone_rows = tone_widths(shape).iter().sum::<usize>();
    let mut m = vec![vec![0.0; width]; mix.rows + tone_rows + pre_strip_rows(shape)];
    let rows: Vec<usize> = (0..mix.rows).collect();
    let mut g = Vec::with_capacity(mix.rows * 8);
    let mut first = 0;
    let mut placed = Vec::with_capacity(columns.len());
    for (source, width, meter) in columns {
        let ew = mix.element(source, meter, &mut g);
        put(&mut m, &rows, first, width, ew, &g);
        placed.push((source, width, first, meter));
        first += width;
    }
    put_tone(&mut m, mix.rows, shape, state, placed.iter().copied());
    let placed = placed
        .into_iter()
        .map(|(id, width, first, _)| (id, width, first));
    put_pre(&mut m, shape, state, placed);
    m
}

/// The first matrix row of each channel's pre-strip rows, in shape
/// order, none for a channel without them. A channel with a direct out
/// ([`Shape::has_direct_out`]) has them, one per leg: samplez adds an
/// input's pre-strip rows after every input's own rows, the line-up
/// tone generators' included, in configuration order.
pub(crate) fn pre_rows(shape: &Shape) -> Vec<Option<usize>> {
    let own: usize = shape.channels.iter().map(|c| c.width).sum();
    let mut row = own + tone_widths(shape).iter().sum::<usize>();
    (shape.channels.iter())
        .map(|c| {
            shape.has_direct_out(&c.id).then(|| {
                row += c.width;
                row - c.width
            })
        })
        .collect()
}

/// How many pre-strip rows the matrix has ([`pre_rows`]).
pub(crate) fn pre_strip_rows(shape: &Shape) -> usize {
    (shape.channels.iter())
        .filter(|c| shape.has_direct_out(&c.id))
        .map(|c| c.width)
        .sum()
}

/// Where a channel's direct out is taken, per model `DirectOut`.
fn direct_tap(channel: &Value) -> Tap {
    match channel["directOut"]["tap"].as_str() {
        Some("input") => Tap::Input,
        Some("postFader") => Tap::PostFader,
        _ => Tap::PreFader,
    }
}

/// Writes the pre-strip rows ([`pre_rows`]) of each of `columns`, an
/// element, its width and its first column. A channel's direct out at
/// [`Tap::Input`] takes the channel there at unity, leg to leg as
/// [`Mix::element`] places a direct out and folded as [`put`] folds it;
/// every other coefficient of those rows is 0. They take no duck mask
/// ([`duck::masks`]).
fn put_pre<'c>(
    m: &mut [Vec<f32>],
    shape: &Shape,
    state: &Value,
    columns: impl IntoIterator<Item = (&'c str, usize, usize)>,
) {
    let pre = pre_rows(shape);
    let mut g = Vec::new();
    let mut rows = Vec::new();
    for (id, width, first) in columns {
        for (c, at) in shape.channels.iter().zip(&pre) {
            let Some(at) = *at else {
                continue;
            };
            let input = c.id == id && direct_tap(&state["channels"][&c.id]) == Tap::Input;
            g.clear();
            g.resize(c.width * c.width, 0.0);
            if input {
                for i in 0..c.width {
                    for j in 0..c.width {
                        g[i * c.width + j] =
                            pan_image(c.width, c.width, i, j, &Place::FRONT, Image::Normal);
                    }
                }
            }
            rows.clear();
            rows.extend(at..at + c.width);
            put(m, &rows, first, width, c.width, &g);
        }
    }
}

/// The legs of each of the line-up tone's generators, one engine input
/// each ([`tone_input`]): one per distinct width of a bus, aux or
/// mix-minus, the elements `/tone/targets` names, narrowest first. A
/// target plays the generator as wide as it is, so its ident cuts its
/// own legs in turn.
pub fn tone_widths(shape: &Shape) -> Vec<usize> {
    let widths: BTreeSet<usize> = (shape.buses.iter().map(|e| e.width))
        .chain(shape.auxes.iter().map(|e| e.width))
        .chain(shape.mix_minus.iter().map(|e| e.width))
        .collect();
    widths.into_iter().collect()
}

/// The engine input of the line-up tone's generator `width` legs wide.
pub fn tone_input(width: usize) -> String {
    format!("{TONE_INPUT}/{width}")
}

/// The first of the tone rows, those of the generators of [`tone_widths`]
/// `widths` in order, that the generator `width` legs wide takes.
fn generator_row(widths: &[usize], width: usize) -> usize {
    widths.iter().take_while(|&&w| w < width).sum()
}

/// The line-up tone's generator: 1 kHz at `/tone/level`, the legs
/// identified with `/tone/ident`.
fn line_up(state: &Value) -> ToneParams {
    let t = &state["tone"];
    ToneParams {
        frequency_hz: TONE_HZ,
        level_db: num(&t["level"]) as f32,
        ident: on(&t["ident"]),
    }
}

/// Whether the line-up tone replaces the mix of element `id`: `/tone` is
/// on and its targets name it.
fn toned(state: &Value, id: &str) -> bool {
    let t = &state["tone"];
    on(&t["on"]) && (t["targets"].as_array()).is_some_and(|ids| ids.iter().any(|t| t == id))
}

/// Writes the line-up tones' rows, from `tone_row` on, of each of
/// `columns`, an element, its width, its first column and whether it is
/// an engine meter's ([`columns`]). Leg `l` of the
/// generator as wide as an element the tone replaces ([`toned`]) is at
/// unity in the column's leg `l`, so an output narrower than its element
/// takes the tone's first legs; [`Mix::element`] gives such an element
/// no channel. The element's fader is not in these gains, so the tone is
/// after it; the engine's limiter on the output is after the matrix. A
/// monitor playing such an element plays its tone ([`Mix::tone`]).
fn put_tone<'c>(
    m: &mut [Vec<f32>],
    tone_row: usize,
    shape: &Shape,
    state: &Value,
    columns: impl IntoIterator<Item = (&'c str, usize, usize, bool)>,
) {
    let widths = tone_widths(shape);
    let mix = Mix::of(shape, state, &[], |_| unreachable!("no channel"));
    let tones = tone_row..tone_row + widths.iter().sum::<usize>();
    let rows: Vec<usize> = tones.clone().collect();
    let mut g = Vec::new();
    for (id, width, first, meter) in columns {
        let ew = mix.tone(id, meter, &widths, &mut g);
        if !toned(state, id) {
            put(m, &rows, first, width, ew, &g);
            continue;
        }
        let generator = generator_row(&widths, ew);
        for (row, r) in m[tones.clone()].iter_mut().enumerate() {
            for leg in 0..width {
                r[first + leg] = f32::from(u8::from(row == generator + leg));
            }
        }
    }
}

/// The element each of the matrix's columns carries, its width and
/// whether it is an engine meter's: the outputs, then the meter columns
/// ([`Shape::meter_columns`]). A monitor output's meter columns carry it
/// before its level, dim, cut, mono and speaker mutes and solos
/// ([`Mix::monitor`]); any other element's carry what its output would.
pub(crate) fn columns(shape: &Shape) -> Vec<(&str, usize, bool)> {
    let outputs = (shape.outputs.iter()).map(|o| (o.source.as_str(), o.width, false));
    let meters = (shape.meter_columns().into_iter()).map(|(id, w, _)| (id, w, true));
    outputs.chain(meters).collect()
}

/// Writes the gains `g` of an element `ew` legs wide ([`Mix::element`])
/// into the columns from `first` on of an output `width` legs wide, its
/// rows into rows `rows` of `m`. An output as wide as the element takes
/// its legs; a narrower one folds them down as [`pan`] does a channel,
/// the sum of each leg's gain times its fold coefficient; legs the
/// element lacks are 0.
fn put(m: &mut [Vec<f32>], rows: &[usize], first: usize, width: usize, ew: usize, g: &[f64]) {
    for (i, &row) in rows.iter().enumerate() {
        for leg in 0..width {
            let v = if width < ew {
                (0..ew)
                    .map(|k| g[i * ew + k] * pan(ew, width, k, leg, 0.0))
                    .sum()
            } else if leg < ew {
                g[i * ew + leg]
            } else {
                0.0
            };
            m[row][first + leg] = v as f32;
        }
    }
}

/// One channel's input as the MXL configuration wires it: the flow of
/// input a, the flow of input b when the channel has one, and the domain
/// both are in. A tone channel's names no flow and no domain, both empty:
/// it reads none ([`selected`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Wiring {
    pub a: String,
    pub b: Option<String>,
    pub domain: String,
}

/// The flow each channel's selected input reads: input b's when the
/// state selects b and the channel has one, else input a's; none for a
/// tone channel, which reads none.
pub fn selected(shape: &Shape, wiring: &[Wiring], state: &Value) -> Vec<Option<String>> {
    (shape.channels.iter())
        .zip(wiring)
        .map(|(c, w)| (c.tone.is_none()).then(|| flow(state, &c.id, w).to_owned()))
        .collect()
}

/// The flow channel `id`, wired as `w`, reads.
pub(crate) fn flow<'w>(state: &Value, id: &str, w: &'w Wiring) -> &'w str {
    match (state["channels"][id]["input"].as_str(), &w.b) {
        (Some("b"), Some(b)) => b,
        _ => &w.a,
    }
}

/// Everything the engine of `shape` plays from `state`: the gain matrix
/// ([`matrix`]), each channel's strip ([`strip::params`]) and input
/// delay ([`strip::delay`]), each tone channel's generator ([`strip::tone`]), each channel's pre-strip meter ([`pre_strip_meters`]), each output's strip ([`strip::output_strips`]), delay ([`strip::output_delays`]) and limiter ([`strip::limiters`]), the duck groups ([`duck::targets`])
/// and masks ([`duck::masks`]) and the flow its selected input reads
/// ([`selected`]), one slot per channel; then a slot per line-up tone
/// generator ([`tone_widths`]), which reads no flow, has no strip and
/// plays [`line_up`]. Every output is enabled.
pub fn targets(shape: &Shape, wiring: &[Wiring], state: &Value) -> Targets {
    Derived::new(shape, wiring, state).targets
}

/// Sets each loudness measurement's run flag and reset epoch in `t` from
/// `/loudness/<id>/running` and `/loudness/<id>/resets` in `state`, by
/// the engine meter that measures it ([`Layout::loudness_meter`]): the
/// epoch is the reset count, wrapping, so a reset counted moves it
/// forward and the engine restarts the measurement where it applies.
/// Every meter up to the last one measuring a bus, aux or mix-minus
/// output is listed, one measuring none running at epoch 0.
pub fn loudness(shape: &Shape, state: &Value, t: &mut Targets) {
    let layout = Layout::of(shape);
    let ids = (shape.buses.iter().map(|b| &b.id))
        .chain(shape.auxes.iter().map(|a| &a.id))
        .chain(shape.mix_minus.iter().map(|m| &m.id));
    let (mut running, mut resets) = (Vec::new(), Vec::new());
    for id in ids {
        let Some(meter) = layout.loudness_meter(id) else {
            continue;
        };
        if running.len() <= meter {
            running.resize(meter + 1, true);
            resets.resize(meter + 1, 0);
        }
        let m = &state["loudness"][id];
        running[meter] = m["running"].as_bool().unwrap_or(true);
        resets[meter] = m["resets"].as_u64().unwrap_or(0) as u32;
    }
    (t.loudness_running, t.loudness_resets) = (running, resets);
}

/// What IS-05 activations set beside the state ([`crate::state::Mixer::engine_view`]):
/// each channel's flows with the leg flows activations named, the
/// channels whose receiver is disabled, and each output's enable.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EngineView {
    pub wiring: Vec<Wiring>,
    pub detached: Vec<bool>,
    pub outputs: Vec<bool>,
}

impl EngineView {
    /// [`targets`] of `state` read through this view, with the loudness
    /// measurements' run flags and reset epochs ([`loudness`]).
    pub fn targets(&self, shape: &Shape, state: &Value) -> Targets {
        let mut t = targets(shape, &self.wiring, state);
        self.overlay(&mut t);
        loudness(shape, state, &mut t);
        t
    }

    /// Detaches the disabled channels and sets each output's enable.
    pub fn overlay(&self, t: &mut Targets) {
        for (flow, detached) in t.flows.iter_mut().zip(&self.detached) {
            if *detached {
                *flow = None;
            }
        }
        t.outputs.clone_from(&self.outputs);
    }
}

/// [`targets`] kept from one write to the next: a write derives again
/// only what the leaves it changed move, with the same operations as
/// [`targets`], so the bits are those ([`Derived::update`]).
pub struct Derived {
    targets: Targets,
    /// Each channel's [`Factors`].
    factors: Vec<Factors>,
    /// Each channel's first row.
    first: Vec<usize>,
    /// The element each column carries ([`columns`]), its width, its
    /// first column and whether it is an engine meter's.
    columns: Vec<(String, usize, usize, bool)>,
    /// The line-up tone generators' first row, after every channel's.
    tone_row: usize,
}

/// What a write moves in [`targets`].
#[derive(Default)]
struct Moved {
    /// Every target.
    all: bool,
    /// The rows of these channel slots.
    rows: BTreeSet<usize>,
    /// The columns of these buses, auxes and mix-minus outputs.
    elements: BTreeSet<String>,
    /// The columns of the monitor outputs, their meters' included.
    monitor: bool,
    /// The strips, delays, tone generators, flows and pre-strip meters of
    /// these slots.
    strips: BTreeSet<usize>,
    delays: BTreeSet<usize>,
    tones: BTreeSet<usize>,
    flows: BTreeSet<usize>,
    meters: BTreeSet<usize>,
    /// The line-up tone's generators ([`line_up`]).
    line_up: bool,
    /// The duck masks: a group's targets or destinations, or the
    /// monitor's source.
    masks: bool,
}

impl Derived {
    pub fn new(shape: &Shape, wiring: &[Wiring], state: &Value) -> Derived {
        let factors: Vec<Factors> = (shape.channels.iter())
            .map(|c| Factors::of(state, &c.id))
            .collect();
        let mut row = 0;
        let first = (shape.channels.iter())
            .map(|c| {
                row += c.width;
                row - c.width
            })
            .collect();
        let generators = tone_widths(shape);
        let mut col = 0;
        let columns = (columns(shape).into_iter())
            .map(|(id, width, meter)| {
                col += width;
                (id.to_owned(), width, col - width, meter)
            })
            .collect();
        Derived {
            targets: Targets {
                gains: gains(shape, state, &factors),
                flows: (selected(shape, wiring, state).into_iter())
                    .chain(generators.iter().map(|_| None))
                    .collect(),
                strips: (shape.channels.iter())
                    .map(|c| strip::params(&state["channels"][&c.id], c.width))
                    .chain(generators.iter().map(|_| Vec::new()))
                    .collect(),
                delays: (shape.channels.iter())
                    .map(|c| strip::delay(&state["channels"][&c.id]))
                    .collect(),
                pre_strip_meters: pre_strip_meters(shape, state),
                tones: (shape.channels.iter())
                    .map(|c| (c.tone.is_some()).then(|| strip::tone(&state["channels"][&c.id])))
                    .chain(generators.iter().map(|_| Some(line_up(state))))
                    .collect(),
                output_strips: strip::output_strips(shape, state),
                output_delays: strip::output_delays(shape, state),
                limiters: strip::limiters(shape, state),
                ducks: duck::targets(shape, state),
                masks: duck::masks(shape, state),
                ..Targets::default()
            },
            factors,
            first,
            columns,
            tone_row: row,
        }
    }

    /// [`targets`] of the state last derived.
    pub fn targets(&self) -> &Targets {
        &self.targets
    }

    /// Derives again what a write of the leaves `paths` moved in
    /// `state`, the state after it; `vcas` is `/vcas` before it, whose
    /// former members a membership change moves.
    pub fn update<'p>(
        &mut self,
        shape: &Shape,
        wiring: &[Wiring],
        state: &Value,
        vcas: &Value,
        paths: impl IntoIterator<Item = &'p str>,
    ) {
        let moved = Moved::of(shape, state, vcas, paths);
        // Past half the channels, recomputing their rows and then the
        // columns besides costs more than the whole.
        if moved.all || moved.rows.len() * 2 > shape.channels.len() {
            *self = Derived::new(shape, wiring, state);
            return;
        }
        let mut g = Vec::new();
        if !moved.rows.is_empty() {
            let slots: Vec<usize> = moved.rows.into_iter().collect();
            for &slot in &slots {
                self.factors[slot] = Factors::of(state, &shape.channels[slot].id);
            }
            let mix = Mix::of(shape, state, &slots, |slot| self.factors[slot]);
            let rows: Vec<usize> = (slots.iter())
                .flat_map(|&slot| self.first[slot]..self.first[slot] + shape.channels[slot].width)
                .collect();
            for (id, width, first, meter) in &self.columns {
                let ew = mix.element(id, *meter, &mut g);
                put(&mut self.targets.gains, &rows, *first, *width, ew, &g);
            }
        }
        let source = state["monitor"]["source"].as_str().unwrap_or_default();
        let monitor = moved.monitor || moved.elements.contains(source);
        let moved_columns: Vec<&(String, usize, usize, bool)> = (self.columns.iter())
            .filter(|(id, ..)| {
                moved.elements.contains(id)
                    || monitor && shape.monitor_outputs.iter().any(|m| m.id == *id)
            })
            .collect();
        if !moved_columns.is_empty() {
            let all: Vec<usize> = (0..shape.channels.len()).collect();
            let mix = Mix::of(shape, state, &all, |slot| self.factors[slot]);
            let rows: Vec<usize> = (0..mix.rows).collect();
            for (id, width, first, meter) in moved_columns {
                let ew = mix.element(id, *meter, &mut g);
                put(&mut self.targets.gains, &rows, *first, *width, ew, &g);
            }
        }
        // The rows and columns written above hold the mix in place of the
        // tone again, and a direct out's tap moves it between the
        // channel's own rows and its pre-strip rows.
        let placed = (self.columns.iter())
            .map(|(id, width, first, meter)| (id.as_str(), *width, *first, *meter));
        put_tone(
            &mut self.targets.gains,
            self.tone_row,
            shape,
            state,
            placed.clone(),
        );
        let placed = placed.map(|(id, width, first, _)| (id, width, first));
        put_pre(&mut self.targets.gains, shape, state, placed);
        for slot in moved.strips {
            let c = &shape.channels[slot];
            self.targets.strips[slot] = strip::params(&state["channels"][&c.id], c.width);
        }
        for slot in moved.delays {
            self.targets.delays[slot] = strip::delay(&state["channels"][&shape.channels[slot].id]);
        }
        for slot in moved.meters {
            self.targets.pre_strip_meters[slot] =
                metered_at_input(&state["channels"][&shape.channels[slot].id]);
        }
        for slot in moved.tones {
            let c = &shape.channels[slot];
            self.targets.tones[slot] =
                (c.tone.is_some()).then(|| strip::tone(&state["channels"][&c.id]));
        }
        for slot in moved.flows {
            let c = &shape.channels[slot];
            self.targets.flows[slot] =
                (c.tone.is_none()).then(|| flow(state, &c.id, &wiring[slot]).to_owned());
        }
        if moved.line_up {
            for tone in &mut self.targets.tones[shape.channels.len()..] {
                *tone = Some(line_up(state));
            }
        }
        // A bus, aux or mix-minus write may move an output's strip, delay
        // or limiter: one per output, each read in full.
        if !moved.elements.is_empty() {
            self.targets.output_strips = strip::output_strips(shape, state);
            self.targets.output_delays = strip::output_delays(shape, state);
            self.targets.limiters = strip::limiters(shape, state);
        }
        // A handful of groups, each read in full: a key gain follows its
        // channel's fader, mute and VCAs, which any write can move.
        if !shape.ducks.is_empty() {
            self.targets.ducks = duck::targets(shape, state);
        }
        if moved.masks {
            self.targets.masks = duck::masks(shape, state);
        }
    }
}

impl Moved {
    /// What a write of the leaves `paths` moves, `vcas` being `/vcas`
    /// before it. [`targets`] reads the channels, AFV stages, VCAs,
    /// buses, auxes, mix-minus outputs and the monitor; a leaf of
    /// another group it is known not to read moves nothing, any other
    /// moves everything.
    fn of<'p>(
        shape: &Shape,
        state: &Value,
        vcas: &Value,
        paths: impl IntoIterator<Item = &'p str>,
    ) -> Moved {
        let slot = |id: &str| shape.channels.iter().position(|c| c.id == id);
        let mut moved = Moved::default();
        for path in paths {
            let t = tokens(path);
            let group = t.first().map(String::as_str);
            let id = t.get(1).map(String::as_str);
            let key = t.get(2).map(String::as_str);
            match (group, id) {
                (Some("channels"), Some(id)) => {
                    let Some(s) = slot(id) else {
                        moved.all = true;
                        continue;
                    };
                    match key {
                        Some(
                            "fader" | "mute" | "routes" | "sends" | "pan" | "image" | "directOut",
                        ) => {
                            moved.rows.insert(s);
                        }
                        Some(
                            "trim" | "legTrim" | "polarity" | "hpf" | "eq" | "gate" | "comp"
                            | "dynamicsBeforeEq",
                        ) => {
                            moved.strips.insert(s);
                        }
                        Some("input") => {
                            moved.flows.insert(s);
                        }
                        Some("delay") => {
                            moved.delays.insert(s);
                        }
                        Some("meterTap") => {
                            moved.meters.insert(s);
                        }
                        Some("tone") => {
                            moved.tones.insert(s);
                        }
                        // Whether anything solos, and in which mode, moves
                        // every row of the monitor ([`Mix::monitor`]) and
                        // its duck masks.
                        Some("solo") => {
                            moved.monitor = true;
                            moved.masks = true;
                        }
                        // A write of the whole channel may carry a solo.
                        None => {
                            moved.rows.insert(s);
                            moved.strips.insert(s);
                            moved.delays.insert(s);
                            moved.tones.insert(s);
                            moved.flows.insert(s);
                            moved.meters.insert(s);
                            moved.monitor = true;
                            moved.masks = true;
                        }
                        _ => {
                            moved.rows.insert(s);
                            moved.strips.insert(s);
                            moved.delays.insert(s);
                            moved.tones.insert(s);
                            moved.flows.insert(s);
                            moved.meters.insert(s);
                        }
                    }
                }
                (Some("afv"), Some(id)) => match slot(id) {
                    Some(s) => {
                        moved.rows.insert(s);
                    }
                    None => moved.all = true,
                },
                // A VCA moves its members, those it had and those it has,
                // and its solo every row of the monitor and its masks.
                (Some("vcas"), _) => {
                    let solo = matches!(key, None | Some("solo"));
                    moved.monitor |= solo;
                    moved.masks |= solo;
                    for vcas in [vcas, &state["vcas"]] {
                        let touched: Vec<&Value> = match id {
                            Some(v) => vec![&vcas[v]],
                            None => vcas
                                .as_object()
                                .into_iter()
                                .flat_map(|v| v.values())
                                .collect(),
                        };
                        let members = (touched.into_iter())
                            .filter_map(|v| v["members"].as_array())
                            .flatten();
                        moved.rows.extend(members.filter_map(|m| slot(m.as_str()?)));
                    }
                }
                (Some("buses" | "auxes" | "mixMinus"), Some(id)) => {
                    moved.elements.insert(id.to_owned());
                    // An interrupt dims the monitor while talkbackDim is on.
                    let interrupt =
                        group == Some("mixMinus") && matches!(key, None | Some("interrupt"));
                    moved.monitor |= interrupt && on(&state["monitor"]["talkbackDim"]);
                    // And lifts the masks of the mix-minus while it lasts.
                    moved.masks |= interrupt;
                }
                (Some("monitor"), _) => {
                    moved.monitor = true;
                    moved.masks |= matches!(id, None | Some("source"));
                }
                // A group, or a group's triggers, targets or destinations.
                (Some("ducks"), _) => {
                    moved.masks |= t.len() < 4 || duck::routing_of(path).is_some();
                }
                // The line-up tone's level and ident move its generator
                // alone; whether it is on and what it replaces move
                // everything, as any other leaf does (below).
                (Some("tone"), Some("level" | "ident")) => moved.line_up = true,
                (
                    Some(
                        "tally" | "loudness" | "recallSafe" | "scenes" | "consoles" | "metering"
                        | "sceneRecalled",
                    ),
                    _,
                ) => {}
                _ => moved.all = true,
            }
        }
        moved
    }
}

/// Where a channel's meter reads: which of its input's meters, and the
/// gain after it, the same on every leg.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeterTap {
    /// At its `meterTap`.
    pub meter: f64,
    /// Whether it reads the input's meter before the strip rather than
    /// the one after it.
    pub pre_strip: bool,
}

/// Each channel's [`MeterTap`]: an input tap reads the engine's meter of
/// the input before its strip, which [`pre_strip_meters`] switches on, a
/// pre-fader tap its meter after the strip, and a post-fader tap adds the
/// fader, mute and VCAs to that.
pub fn meter_taps(shape: &Shape, state: &Value) -> Vec<MeterTap> {
    let tap = |v: &Value| match v.as_str() {
        Some("input") => Tap::Input,
        Some("postFader") => Tap::PostFader,
        _ => Tap::PreFader,
    };
    shape
        .channels
        .iter()
        .map(|c| {
            let tap = tap(&state["channels"][&c.id]["meterTap"]);
            MeterTap {
                meter: channel(state, &c.id, tap),
                pre_strip: tap == Tap::Input,
            }
        })
        .collect()
}

/// Whether each channel's pre-strip meter runs, per input slot
/// (samplez `PreStripMeter`): exactly while its `meterTap` is input, the
/// one tap that reads it. The engine copies and meters an input before
/// its strip only while it runs, so a desk metering after the strip pays
/// nothing for it.
pub fn pre_strip_meters(shape: &Shape, state: &Value) -> Vec<bool> {
    (shape.channels.iter())
        .map(|c| metered_at_input(&state["channels"][&c.id]))
        .collect()
}

/// Whether a channel's `meterTap` is input.
fn metered_at_input(channel: &Value) -> bool {
    channel["meterTap"] == "input"
}

/// A channel's gain at [`Tap::PostFader`] and at [`Tap::Mix`]
/// ([`channel`]), the same on every leg.
#[derive(Clone, Copy)]
struct Factors {
    post: f64,
    mix: f64,
}

impl Factors {
    fn of(state: &Value, id: &str) -> Factors {
        Factors {
            post: channel(state, id, Tap::PostFader),
            mix: channel(state, id, Tap::Mix),
        }
    }
}

/// One channel as the matrix reads it.
struct Chan<'a> {
    id: &'a str,
    state: &'a Value,
    routes: &'a Value,
    sends: &'a Value,
    /// Its pan.
    place: Place,
    /// Its stereo image.
    image: Image,
    /// The row of its first leg.
    row: usize,
    width: usize,
    factors: Factors,
}

impl Chan<'_> {
    fn at(&self, tap: Tap) -> f64 {
        match tap {
            Tap::Input | Tap::PreFader => 1.0,
            Tap::PostFader => self.factors.post,
            Tap::Mix => self.factors.mix,
        }
    }
}

/// What an element is, which says how a channel feeds it.
enum Kind<'a> {
    Channel,
    Bus,
    Aux,
    /// Its bus, the channels it excludes and the channel its talkback
    /// interrupt carries in their place ([`talkback`]).
    MixMinus(&'a str, &'a Value, Option<&'a str>),
    Monitor,
    Other,
}

/// The state's matrix over some of its channels, each channel's tap
/// gains taken once.
struct Mix<'a> {
    shape: &'a Shape,
    state: &'a Value,
    rows: usize,
    chans: Vec<Chan<'a>>,
}

impl<'a> Mix<'a> {
    /// The channels at `slots` with the gains `factors` gives each slot,
    /// their rows counted from 0 in that order. A coefficient takes only
    /// its own channel's state and its element's, so a channel's rows
    /// are the same bits whichever others are with it.
    fn of(
        shape: &'a Shape,
        state: &'a Value,
        slots: &[usize],
        factors: impl Fn(usize) -> Factors,
    ) -> Mix<'a> {
        let mut row = 0;
        let chans = (slots.iter())
            .map(|&slot| {
                let c = &shape.channels[slot];
                let s = &state["channels"][&c.id];
                let chan = Chan {
                    id: &c.id,
                    state: s,
                    routes: &s["routes"],
                    sends: &s["sends"],
                    place: Place::of(&s["pan"]),
                    image: Image::of(&s["image"]),
                    row,
                    width: c.width,
                    factors: factors(slot),
                };
                row += c.width;
                chan
            })
            .collect();
        Mix {
            shape,
            state,
            rows: row,
            chans,
        }
    }

    fn kind(&self, id: &str) -> Kind<'a> {
        let shape = self.shape;
        if shape.channels.iter().any(|c| c.id == id) {
            Kind::Channel
        } else if shape.buses.iter().any(|b| b.id == id) {
            Kind::Bus
        } else if shape.auxes.iter().any(|a| a.id == id) {
            Kind::Aux
        } else if let Some(m) = shape.mix_minus.iter().find(|m| m.id == id) {
            Kind::MixMinus(
                &m.bus,
                &self.state["mixMinus"][id]["exclude"],
                talkback(self.state, id),
            )
        } else if shape.monitor_outputs.iter().any(|m| m.id == id) {
            Kind::Monitor
        } else {
            Kind::Other
        }
    }

    /// The gains of element `id` from every input channel into `g`, row
    /// by row, `[input * width + leg]`; returns its width. `meter` takes
    /// a monitor output's meter columns ([`columns`]).
    fn element(&self, id: &str, meter: bool, g: &mut Vec<f64>) -> usize {
        let width = width_of(self.shape, id);
        g.clear();
        g.resize(self.rows * width, 0.0);
        // The line-up tone in place of its mix: no channel ([`put_tone`]).
        if toned(self.state, id) {
            return width;
        }
        let kind = self.kind(id);
        if let Kind::Monitor = kind {
            self.monitor(width, meter, g);
            return width;
        }
        for c in &self.chans {
            let Some((tap, level, p)) = feed(c, &kind, id) else {
                continue;
            };
            let at = c.at(tap) * level;
            // A direct out is the channel itself, whatever its image.
            let image = if matches!(kind, Kind::Channel) {
                Image::Normal
            } else {
                c.image
            };
            for i in 0..c.width {
                let row = &mut g[(c.row + i) * width..][..width];
                for (j, out) in row.iter_mut().enumerate() {
                    *out = at * pan_image(c.width, width, i, j, &p, image);
                }
            }
        }
        match kind {
            Kind::Bus => scale(g, width, &self.state["buses"][id], true),
            Kind::Aux => scale(g, width, &self.state["auxes"][id], true),
            Kind::MixMinus(..) => scale(g, width, &self.state["mixMinus"][id], false),
            _ => {}
        }
        width
    }

    /// The monitor output: while any channel or VCA solos the listen sum
    /// ([`solos`]), else its source; that signal's legs onto the
    /// monitor's, folded down by `downmix` when it is wider, then level,
    /// dim (also while a mix-minus carries its talkback and `talkbackDim`
    /// is on), cut, mono (the sum of the folded legs) and the speaker
    /// mutes and solos. Its `meter` columns take none of those five
    /// (model `MonitorMeter`).
    ///
    /// The listen sum takes each listening channel's legs at their own
    /// width. PFL is the channel after its strip, before fader, mute and
    /// pan: [`Tap::PreFader`], centred, so a mono channel plays on every
    /// leg and a stereo one leg to leg, as its image sets them. AFL is [`Tap::PostFader`], after
    /// fader, mute and VCAs and before AFV, at the channel's pan.
    fn monitor(&self, width: usize, meter: bool, g: &mut [f64]) {
        let play = self.monitor_play(width, meter);
        let (soloing, pfl) = solos(self.state);
        if !soloing {
            // A source the line-up tone replaces gives every row 0
            // ([`Mix::element`]); [`Mix::tone`] plays the tone.
            let source = self.state["monitor"]["source"].as_str().unwrap_or_default();
            let mut src = Vec::new();
            let from = self.element(source, false, &mut src);
            for row in 0..self.rows {
                play(
                    &src[row * from..][..from],
                    from,
                    &Place::FRONT,
                    Image::Normal,
                    &mut g[row * width..][..width],
                );
            }
            return;
        }
        let mut legs = Vec::new();
        for c in &self.chans {
            let (p, a) = listens(self.state, c.id);
            let heard = if pfl {
                p.then_some((Tap::PreFader, Place::FRONT))
            } else {
                a.then_some((Tap::PostFader, c.place))
            };
            let Some((tap, p)) = heard else {
                continue;
            };
            for i in 0..c.width {
                legs.clear();
                legs.resize(c.width, 0.0);
                legs[i] = c.at(tap);
                let row = c.row + i;
                play(&legs, c.width, &p, c.image, &mut g[row * width..][..width]);
            }
        }
    }

    /// The gains of the line-up tones' rows into element `id`, the
    /// generators of [`tone_widths`] `widths` in order, `[tone row * width
    /// + leg]` as [`Mix::element`] gives the channels'; returns its width.
    /// A monitor whose source the tone replaces ([`toned`]) plays that
    /// source's generator, as wide as the source, as it plays the source,
    /// while nothing solos, its `meter` columns ([`columns`]) as
    /// [`Mix::monitor`] gives them; any other element has none. The
    /// columns of an element the tone replaces are [`put_tone`]'s.
    fn tone(&self, id: &str, meter: bool, widths: &[usize], g: &mut Vec<f64>) -> usize {
        let width = width_of(self.shape, id);
        g.clear();
        g.resize(widths.iter().sum::<usize>() * width, 0.0);
        let source = self.state["monitor"]["source"].as_str().unwrap_or_default();
        let monitor = self.shape.monitor_outputs.iter().any(|m| m.id == id);
        if !monitor || solos(self.state).0 || !toned(self.state, source) {
            return width;
        }
        let from = width_of(self.shape, source);
        let first = generator_row(widths, from);
        let play = self.monitor_play(width, meter);
        let mut legs = vec![0.0; from];
        for leg in 0..from {
            legs.fill(0.0);
            legs[leg] = 1.0;
            let row = first + leg;
            play(
                &legs,
                from,
                &Place::FRONT,
                Image::Normal,
                &mut g[row * width..][..width],
            );
        }
        width
    }

    /// The monitor output `width` legs wide as it plays a signal: writes
    /// into `out` its legs for the `from` legs `legs` of a signal panned
    /// by `p` with stereo image `image` ([`Mix::monitor`]); for its
    /// `meter` columns, at unity on every leg and not summed to mono.
    fn monitor_play(
        &self,
        width: usize,
        meter: bool,
    ) -> impl Fn(&[f64], usize, &Place, Image, &mut [f64]) {
        let m = &self.state["monitor"];
        let mut level = gain(num(&m["level"]));
        if on(&m["dim"]) || on(&m["talkbackDim"]) && talking(self.shape, self.state) {
            level *= gain(-num(&m["dimDepth"]));
        }
        if on(&m["cut"]) {
            level = 0.0;
        }
        let lr = m["downmix"] == "lr";
        let mono = on(&m["mono"]) && !meter;
        if mono {
            level *= gain(-num(&m["monoCompensation"]));
        }
        // The `from` legs of a signal panned by `p` onto monitor leg `j`,
        // folded by the downmix: lr plays only its L and R legs.
        let folded = move |legs: &[f64], from: usize, p: &Place, image: Image, j: usize| -> f64 {
            if lr && from > width {
                legs.get(j).copied().filter(|_| j < 2).unwrap_or(0.0)
            } else {
                (0..from)
                    .map(|i| legs[i] * pan_image(from, width, i, j, p, image))
                    .sum()
            }
        };
        // The monitor's image takes its stereo legs after the fold.
        let monitor_image = if width == 2 {
            Image::of(&m["image"])
        } else {
            Image::Normal
        };
        let shaped = move |legs: &[f64], from: usize, p: &Place, image: Image, j: usize| -> f64 {
            if monitor_image == Image::Normal {
                return folded(legs, from, p, image, j);
            }
            (0..width)
                .map(|k| monitor_image.share(j, k) * folded(legs, from, p, image, k))
                .sum()
        };
        let speaker_solo = m["speakerSolo"]
            .as_array()
            .is_some_and(|s| s.iter().any(on));
        let leg_gain: Vec<f64> = (0..width)
            .map(|j| {
                let audible =
                    !on(&m["speakerMute"][j]) && (!speaker_solo || on(&m["speakerSolo"][j]));
                if meter {
                    1.0
                } else if audible {
                    level
                } else {
                    0.0
                }
            })
            .collect();
        move |legs: &[f64], from: usize, p: &Place, image: Image, out: &mut [f64]| {
            for (j, out) in out.iter_mut().enumerate() {
                let v: f64 = if mono {
                    (0..width).map(|k| shaped(legs, from, p, image, k)).sum()
                } else {
                    shaped(legs, from, p, image, j)
                };
                *out = v * leg_gain[j];
            }
        }
    }
}

/// How channel `c` feeds element `id` of `kind`: tap, level and pan, or
/// `None` when it does not.
fn feed(c: &Chan<'_>, kind: &Kind<'_>, id: &str) -> Option<(Tap, f64, Place)> {
    match kind {
        // At the input tap its pre-strip rows carry it ([`put_pre`]).
        Kind::Channel if c.id == id => {
            let tap = direct_tap(c.state);
            (tap != Tap::Input).then_some((tap, 1.0, Place::FRONT))
        }
        Kind::Bus => on(&c.routes[id]).then_some((Tap::Mix, 1.0, c.place)),
        Kind::Aux => {
            let send = &c.sends[id];
            if !on(&send["on"]) {
                return None;
            }
            let tap = if on(&send["pre"]) {
                Tap::PreFader
            } else {
                Tap::Mix
            };
            Some((tap, gain(num(&send["level"])), Place::lr(num(&send["pan"]))))
        }
        // The talkback alone, after its strip and before its fader.
        Kind::MixMinus(_, _, Some(talk)) => {
            (c.id == *talk).then_some((Tap::PreFader, 1.0, c.place))
        }
        Kind::MixMinus(bus, exclude, None) => {
            let excluded = exclude
                .as_array()
                .is_some_and(|e| e.iter().any(|e| e == c.id));
            (on(&c.routes[*bus]) && !excluded).then_some((Tap::Mix, 1.0, c.place))
        }
        _ => None,
    }
}

/// Applies an element's fader, mute and, when it has one, polarity to
/// `g`, rows of `width` legs.
fn scale(g: &mut [f64], width: usize, state: &Value, polarity: bool) {
    let level = if on(&state["mute"]) {
        0.0
    } else {
        gain(num(&state["fader"]))
    };
    let legs: Vec<f64> = (0..width)
        .map(|leg| {
            let sign = if polarity && on(&state["polarity"][leg]) {
                -1.0
            } else {
                1.0
            };
            sign * level
        })
        .collect();
    for row in g.chunks_mut(width.max(1)) {
        for (v, l) in row.iter_mut().zip(&legs) {
            *v *= l;
        }
    }
}

fn width_of(shape: &Shape, id: &str) -> usize {
    shape
        .channels
        .iter()
        .map(|e| (&e.id, e.width))
        .chain(shape.buses.iter().map(|e| (&e.id, e.width)))
        .chain(shape.auxes.iter().map(|e| (&e.id, e.width)))
        .chain(shape.mix_minus.iter().map(|e| (&e.id, e.width)))
        .chain(shape.monitor_outputs.iter().map(|e| (&e.id, e.width)))
        .find(|(e, _)| *e == id)
        .map_or(0, |(_, w)| w)
}

#[cfg(test)]
mod tests;
