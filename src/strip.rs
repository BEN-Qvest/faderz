//! A channel's processing in the live state as the parameters of its
//! input's strip in the engine (`samplez_dsp::strip`): trim, leg trim,
//! polarity, high-pass, the four EQ bands, gate, compressor and their
//! order. The model's ranges are the strip's. Also each channel's input
//! delay and a tone channel's generator, and each output's strip and
//! limiter, from the bus, aux or mix-minus output it carries.

use samplez_dsp::{
    BandType, CompParams, EqBand, GateParams, LimiterParams, StripParam, ToneParams,
};
use serde_json::Value;

use crate::shape::Shape;

/// faderz has no compressor knee control: a hard knee.
const KNEE_DB: f32 = 0.0;

fn num(v: &Value) -> f32 {
    v.as_f64().unwrap_or(0.0) as f32
}

fn on(v: &Value) -> bool {
    v.as_bool().unwrap_or(false)
}

/// The strip parameters of channel state `c` (model `ChannelState`) of a
/// channel `width` legs wide, always the same parameters in the same
/// order, so two lists compare entry by entry.
pub fn params(c: &Value, width: usize) -> Vec<StripParam> {
    let mut p = vec![StripParam::Trim {
        db: num(&c["trim"]),
    }];
    p.extend((0..width).map(|leg| StripParam::LegTrim {
        leg,
        db: num(&c["legTrim"][leg]),
    }));
    p.extend((0..width).map(|leg| StripParam::Polarity {
        leg,
        inverted: on(&c["polarity"][leg]),
    }));
    p.push(StripParam::Hpf {
        enabled: on(&c["hpf"]["enabled"]),
        freq: num(&c["hpf"]["freq"]),
    });
    p.extend(processing(c));
    p
}

/// The EQ, gate, compressor and their order of `e`, a channel's state
/// or the state of a bus, aux or mix-minus output (model `BusState`,
/// `MixMinusState`), always the same parameters in the same order.
fn processing(e: &Value) -> Vec<StripParam> {
    let mut p = vec![StripParam::Eq {
        enabled: on(&e["eq"]["enabled"]),
    }];
    for band in 0..samplez_dsp::strip::EQ_BANDS {
        let b = &e["eq"]["bands"][band];
        let kind = match b["type"].as_str() {
            Some("lowShelf") => BandType::LowShelf,
            Some("highShelf") => BandType::HighShelf,
            _ => BandType::Peak,
        };
        p.push(StripParam::EqBand {
            band,
            value: EqBand {
                kind,
                freq: num(&b["freq"]),
                gain_db: num(&b["gain"]),
                q: num(&b["q"]),
            },
        });
    }
    let g = &e["gate"];
    p.push(StripParam::Gate(GateParams {
        enabled: on(&g["enabled"]),
        threshold_db: num(&g["threshold"]),
        range_db: num(&g["range"]),
        attack_ms: num(&g["attack"]),
        release_ms: num(&g["release"]),
    }));
    let k = &e["comp"];
    p.push(StripParam::Comp(CompParams {
        enabled: on(&k["enabled"]),
        threshold_db: num(&k["threshold"]),
        ratio: num(&k["ratio"]),
        attack_ms: num(&k["attack"]),
        release_ms: num(&k["release"]),
        knee_db: KNEE_DB,
        makeup_db: num(&k["makeup"]),
        // Percent; a state saved before the mix holds none: the compressor alone.
        mix: (k["mix"].as_f64().unwrap_or(100.0) / 100.0) as f32,
    }));
    p.push(StripParam::DynamicsBeforeEq {
        enabled: on(&e["dynamicsBeforeEq"]),
    });
    p
}

/// The delay of channel state `c` (model `ChannelState`) or of a bus's
/// or aux's state (model `BusState`), in samples. A write is refused past the channel's delay line
/// ([`crate::shape::ChannelShape::max_delay`]), so the engine takes it.
pub fn delay(c: &Value) -> u32 {
    c["delay"].as_u64().unwrap_or(0) as u32
}

/// The tone generator parameters of a tone channel's state `c` (model
/// `ChannelState`), whose `tone` the contract keeps within samplez's
/// ranges, so the engine takes them.
pub fn tone(c: &Value) -> ToneParams {
    let t = &c["tone"];
    ToneParams {
        frequency_hz: t["frequency"].as_f64().unwrap_or(0.0) as u32,
        level_db: num(&t["level"]),
        ident: on(&t["ident"]),
    }
}

/// The state of the bus, aux or mix-minus output `id` in `state`; null
/// for any other element.
fn element<'s>(state: &'s Value, id: &str) -> &'s Value {
    (["buses", "auxes", "mixMinus"].into_iter())
        .map(|group| &state[group][id])
        .find(|e| !e.is_null())
        .unwrap_or(&Value::Null)
}

/// Per output of `shape`, the strip of the bus, aux or mix-minus output
/// it carries in `state` ([`processing`]): EQ, gate, compressor and
/// their order, after the element's fader and before the output's
/// limiter. Empty for an output carrying anything else, which has no
/// strip in the engine ([`Shape::has_strip`]). A bus's polarity is not
/// in it: the gain matrix applies it per leg of the bus, before an
/// output folds the bus down.
pub fn output_strips(shape: &Shape, state: &Value) -> Vec<Vec<StripParam>> {
    (shape.outputs.iter())
        .map(|o| {
            if shape.has_strip(&o.source) {
                processing(element(state, &o.source))
            } else {
                Vec::new()
            }
        })
        .collect()
}

/// Per output of `shape`, the delay of the bus or aux it carries in
/// `state`, in samples, after its strip and before its limiter; 0 for an
/// output carrying anything else. A write is refused past the element's
/// delay line ([`Shape::delay_line`]), the line of every output carrying
/// it, so the engine takes it.
pub fn output_delays(shape: &Shape, state: &Value) -> Vec<u32> {
    (shape.outputs.iter())
        .map(|o| {
            if shape.has_limiter(&o.source) {
                delay(element(state, &o.source))
            } else {
                0
            }
        })
        .collect()
}

/// Per output of `shape`, the limiter of the bus or aux it carries in
/// `state`; none for an output carrying anything else, which has no
/// limiter in the engine ([`Shape::has_limiter`]). The model has no release
/// control: the engine's default release.
pub fn limiters(shape: &Shape, state: &Value) -> Vec<Option<LimiterParams>> {
    (shape.outputs.iter())
        .map(|o| {
            shape.has_limiter(&o.source).then(|| {
                let l = &element(state, &o.source)["limiter"];
                LimiterParams {
                    enabled: on(&l["enabled"]),
                    ceiling_db: num(&l["ceiling"]),
                    // Stated, not the engine's default: a default that
                    // moved would change what one state derives.
                    release_ms: 50.0,
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
