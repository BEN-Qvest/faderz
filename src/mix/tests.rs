/// The matrix as it was computed one coefficient at a time, before
/// the per-channel and per-element factors were taken once: the
/// reference the single-replica engine path was sent.
mod before {
    use super::super::{
        Image, Place, Tap, channel, gain, listens, num, on, pan_image, solos, talkback, talking,
        width_of,
    };
    use crate::shape::Shape;
    use serde_json::Value;

    /// The output and meter channels' gains for each input channel:
    /// `[input][column]`.
    pub fn matrix(shape: &Shape, state: &Value) -> Vec<Vec<f32>> {
        let rows: usize = shape.channels.iter().map(|c| c.width).sum();
        let mut m = vec![Vec::new(); rows];
        let outputs = (shape.outputs.iter()).map(|o| (o.source.as_str(), o.width, false));
        let meters = (shape.meter_columns().into_iter()).map(|(id, w, _)| (id, w, true));
        for (source, width, meter) in outputs.chain(meters) {
            let element = element(shape, state, source, meter);
            for leg in 0..width {
                for (row, r) in m.iter_mut().enumerate() {
                    let g = element.get(row).and_then(|g| g.get(leg)).copied();
                    r.push(g.unwrap_or(0.0) as f32);
                }
            }
        }
        m
    }

    /// The gains of element `id` from every input channel: `[input][leg]`;
    /// a monitor output's on its `meter` columns.
    fn element(shape: &Shape, state: &Value, id: &str, meter: bool) -> Vec<Vec<f64>> {
        let rows: usize = shape.channels.iter().map(|c| c.width).sum();
        let width = width_of(shape, id);
        let mut g = vec![vec![0.0; width]; rows];
        // The row of each channel's first leg.
        let mut row = 0;
        for c in &shape.channels {
            let feed = feed(shape, state, &c.id, id);
            if let Some((tap, level, p)) = feed {
                let image = if c.id == id {
                    Image::Normal
                } else {
                    Image::of(&state["channels"][&c.id]["image"])
                };
                for i in 0..c.width {
                    let at = channel(state, &c.id, tap) * level;
                    for (j, out) in g[row + i].iter_mut().enumerate() {
                        *out = at * pan_image(c.width, width, i, j, &p, image);
                    }
                }
            }
            row += c.width;
        }
        if let Some(b) = shape.buses.iter().find(|b| b.id == id) {
            scale(&mut g, &state["buses"][&b.id], true);
        } else if let Some(a) = shape.auxes.iter().find(|a| a.id == id) {
            scale(&mut g, &state["auxes"][&a.id], true);
        } else if shape.mix_minus.iter().any(|m| m.id == id) {
            scale(&mut g, &state["mixMinus"][id], false);
        } else if shape.monitor_outputs.iter().any(|m| m.id == id) {
            return monitor(shape, state, width, meter);
        }
        g
    }

    /// How channel `channel` feeds element `id`: tap, level and pan, or
    /// `None` when it does not.
    fn feed(shape: &Shape, state: &Value, channel: &str, id: &str) -> Option<(Tap, f64, Place)> {
        let c = &state["channels"][channel];
        let pan = Place::of(&c["pan"]);
        if channel == id {
            let tap = match c["directOut"]["tap"].as_str() {
                Some("input") => Tap::Input,
                Some("postFader") => Tap::PostFader,
                _ => Tap::PreFader,
            };
            return Some((tap, 1.0, Place::FRONT));
        }
        if shape.buses.iter().any(|b| b.id == id) {
            return on(&c["routes"][id]).then_some((Tap::Mix, 1.0, pan));
        }
        if shape.auxes.iter().any(|a| a.id == id) {
            let s = &c["sends"][id];
            let tap = if on(&s["pre"]) {
                Tap::PreFader
            } else {
                Tap::Mix
            };
            return on(&s["on"]).then_some((
                tap,
                gain(num(&s["level"])),
                Place::lr(num(&s["pan"])),
            ));
        }
        let m = shape.mix_minus.iter().find(|m| m.id == id)?;
        if let Some(talk) = talkback(state, id) {
            return (channel == talk).then_some((Tap::PreFader, 1.0, pan));
        }
        let excluded = state["mixMinus"][id]["exclude"]
            .as_array()
            .is_some_and(|e| e.iter().any(|e| e == channel));
        (on(&c["routes"][&m.bus]) && !excluded).then_some((Tap::Mix, 1.0, pan))
    }

    /// Applies an element's fader, mute and, when it has one, polarity.
    fn scale(g: &mut [Vec<f64>], state: &Value, polarity: bool) {
        let level = if on(&state["mute"]) {
            0.0
        } else {
            gain(num(&state["fader"]))
        };
        for row in g {
            for (leg, v) in row.iter_mut().enumerate() {
                let sign = if polarity && on(&state["polarity"][leg]) {
                    -1.0
                } else {
                    1.0
                };
                *v *= sign * level;
            }
        }
    }

    /// The monitor output: while anything solos the listen sum, else
    /// its source; that signal's legs onto the monitor's, folded down
    /// by `downmix` when it is wider, then level, dim, cut, mono (the
    /// sum of the folded legs) and the speaker mutes and solos, none of
    /// which its `meter` columns take.
    fn monitor(shape: &Shape, state: &Value, width: usize, meter: bool) -> Vec<Vec<f64>> {
        let m = &state["monitor"];
        let mut level = gain(num(&m["level"]));
        if on(&m["dim"]) || on(&m["talkbackDim"]) && talking(shape, state) {
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
        // The `from` legs of a signal panned by `p` onto monitor leg
        // `j`, folded by the downmix: lr plays only its L and R legs.
        let folded = |legs: &[f64], from: usize, p: &Place, image: Image, j: usize| -> f64 {
            if lr && from > width {
                legs.get(j).copied().filter(|_| j < 2).unwrap_or(0.0)
            } else {
                (0..from)
                    .map(|i| legs[i] * pan_image(from, width, i, j, p, image))
                    .sum()
            }
        };
        let monitor_image = if width == 2 {
            Image::of(&m["image"])
        } else {
            Image::Normal
        };
        let shaped = |legs: &[f64], from: usize, p: &Place, image: Image, j: usize| -> f64 {
            if monitor_image == Image::Normal {
                return folded(legs, from, p, image, j);
            }
            (0..width)
                .map(|k| monitor_image.share(j, k) * folded(legs, from, p, image, k))
                .sum()
        };
        let leg_gain = |j: usize| {
            let solo = m["speakerSolo"]
                .as_array()
                .is_some_and(|s| s.iter().any(on));
            let audible = !on(&m["speakerMute"][j]) && (!solo || on(&m["speakerSolo"][j]));
            if meter {
                1.0
            } else if audible {
                level
            } else {
                0.0
            }
        };
        let play = |legs: &[f64], from: usize, p: &Place, image: Image| -> Vec<f64> {
            (0..width)
                .map(|j| {
                    let v: f64 = if mono {
                        (0..width).map(|k| shaped(legs, from, p, image, k)).sum()
                    } else {
                        shaped(legs, from, p, image, j)
                    };
                    v * leg_gain(j)
                })
                .collect()
        };
        let (soloing, pfl) = solos(state);
        if !soloing {
            let source = m["source"].as_str().unwrap_or_default();
            let src = element(shape, state, source, false);
            let from = width_of(shape, source);
            return src
                .iter()
                .map(|legs| play(legs, from, &Place::FRONT, Image::Normal))
                .collect();
        }
        let mut g = Vec::new();
        for c in &shape.channels {
            let (p, a) = listens(state, &c.id);
            let heard = if pfl {
                p.then_some((Tap::PreFader, Place::FRONT))
            } else {
                a.then(|| (Tap::PostFader, Place::of(&state["channels"][&c.id]["pan"])))
            };
            for i in 0..c.width {
                let row = match heard {
                    Some((tap, p)) => {
                        let mut legs = vec![0.0; c.width];
                        legs[i] = channel(state, &c.id, tap);
                        play(
                            &legs,
                            c.width,
                            &p,
                            Image::of(&state["channels"][&c.id]["image"]),
                        )
                    }
                    None => vec![0.0; width],
                };
                g.push(row);
            }
        }
        g
    }
}
use super::*;
use crate::shape::tests::small;
use serde_json::json;

fn shape() -> Shape {
    let mut s = small();
    s["outputs"] = json!([
        { "id": "o-pgm", "name": "PGM", "source": "pgm", "width": 2, "layout": "stereo" },
        { "id": "o-aux", "name": "IFB", "source": "aux1", "width": 1, "layout": "mono" },
        { "id": "o-mm", "name": "N-1", "source": "mm1", "width": 2, "layout": "stereo" },
        { "id": "o-mon", "name": "Mon", "source": "mon", "width": 2, "layout": "stereo" },
        { "id": "o-ch1", "name": "DO 1", "source": "ch1", "width": 1, "layout": "mono" },
    ]);
    Shape::parse(&s).unwrap()
}

fn close(a: f32, b: f64) -> bool {
    (f64::from(a) - b).abs() < 1e-6
}

// Rows: ch1, ch2, st1 L, st1 R, then the line-up tone's mono and
// stereo generators' legs, then ch1's pre-strip row. Columns: o-pgm
// L R, o-aux, o-mm L R, o-mon L R, o-ch1, then the meters cf L R and
// mon L R.

/// A fresh state is silent everywhere but the direct out, which taps
/// before the closed faders.
#[test]
fn the_initial_state_passes_only_the_direct_out() {
    let shape = shape();
    let m = matrix(&shape, &shape.initial_state());
    assert_eq!(m.len(), 8);
    // The outputs' 8 columns, then cf's 2, which no output carries,
    // and the monitor meter's 2.
    assert!(m.iter().all(|r| r.len() == 12));
    for (row, r) in m.iter().enumerate() {
        for (col, g) in r.iter().enumerate() {
            let want = if row == 0 && col == 7 { 1.0 } else { 0.0 };
            assert!(close(*g, want), "[{row}][{col}] = {g}");
        }
    }
}

/// A direct out at the input tap is the channel before its strip,
/// which only its pre-strip rows carry: at unity, leg to leg and
/// folded onto a narrower output, with its own rows 0 in that column,
/// whatever the fader. At preFader and postFader it is on its own
/// rows and the pre-strip rows are 0. Only a channel with a direct out
/// has them, after every channel's and tone generator's rows, and a
/// tap write derives the same bits as the whole state.
#[test]
fn a_direct_out_at_the_input_tap_takes_the_pre_strip_rows() {
    let mut v = small();
    v["outputs"] = json!([
        { "id": "o-pgm", "name": "PGM", "source": "pgm", "width": 2, "layout": "stereo" },
        { "id": "o-st1", "name": "DO st", "source": "st1", "width": 2, "layout": "stereo" },
        { "id": "o-st1m", "name": "DO st m", "source": "st1", "width": 1, "layout": "mono" },
        { "id": "o-ch2", "name": "DO 2", "source": "ch2", "width": 1, "layout": "mono" },
    ]);
    let shape = Shape::parse(&v).unwrap();
    // After the channels' 4 legs and the tones' 3, ch2's and st1's.
    assert_eq!(pre_rows(&shape), [None, Some(7), Some(8)]);
    assert_eq!(pre_strip_rows(&shape), 3);
    let mut s = shape.initial_state();
    for ch in ["ch2", "st1"] {
        s["channels"][ch]["fader"] = json!(-10);
        s["channels"][ch]["directOut"]["tap"] = json!("input");
    }
    let m = matrix(&shape, &s);
    assert_eq!(m.len(), 10);
    // Columns: o-pgm L R, o-st1 L R, o-st1m, o-ch2, then the meters'.
    let at = |rows: std::ops::Range<usize>, col: usize| -> Vec<f32> {
        rows.map(|r| m[r][col]).collect()
    };
    assert_eq!(at(0..10, 5), [0., 0., 0., 0., 0., 0., 0., 1., 0., 0.]);
    assert_eq!(at(7..10, 2), [0., 1., 0.]);
    assert_eq!(at(7..10, 3), [0., 0., 1.]);
    let h = (pan(2, 1, 0, 0, 0.0) as f32, pan(2, 1, 1, 0, 0.0) as f32);
    assert_eq!(at(7..10, 4), [0., h.0, h.1]);
    for col in 2..6 {
        assert!(
            at(0..7, col).iter().all(|&g| g == 0.0),
            "own rows, col {col}"
        );
    }
    for col in [0, 1, 6, 7] {
        assert!(
            at(7..10, col).iter().all(|&g| g == 0.0),
            "pre rows, col {col}"
        );
    }
    let mut d = Derived::new(&shape, &[], &s);
    for (ch, tap) in [("ch2", "preFader"), ("st1", "postFader")] {
        let path = format!("/channels/{ch}/directOut/tap");
        let vcas = s["vcas"].clone();
        crate::state::set_at(&mut s, &path, json!(tap));
        d.update(&shape, &[], &s, &vcas, [path.as_str()]);
        assert_eq!(d.targets().gains, matrix(&shape, &s), "{ch} at {tap}");
    }
    let m = matrix(&shape, &s);
    assert!(m[7..].iter().flatten().all(|&g| g == 0.0));
    assert_eq!(m[1][5], 1.0, "ch2 pre-fader on its own row");
    assert!(
        close(m[2][2], gain(-10.0)),
        "st1 post-fader on its own rows"
    );
}

/// A channel's pre-strip meter runs exactly while its meterTap is
/// input: a desk metering at preFader and postFader runs none, so the
/// engine copies and meters no input before its strip and nothing is
/// sent, the engine starting with every one off. A meterTap write
/// moves it alone, to the bits of the whole state, and a diff carries
/// it as a PreStripMeter entry.
#[test]
fn pre_strip_meters_run_exactly_at_the_input_tap() {
    use samplez::engine::Entry;
    let shape = shape();
    let mut s = shape.initial_state();
    let off = targets(&shape, &[], &s);
    assert_eq!(off.pre_strip_meters, [false; 3], "every channel postFader");
    s["channels"]["ch1"]["meterTap"] = json!("preFader");
    assert_eq!(targets(&shape, &[], &s).pre_strip_meters, [false; 3]);
    let pre = |t: &Targets| -> Vec<Entry> {
        (Targets::default().diff(t).into_iter())
            .filter(|e| matches!(e, Entry::PreStripMeter { .. }))
            .collect()
    };
    assert!(pre(&off).is_empty());
    let mut d = Derived::new(&shape, &[], &s);
    for (ch, tap, want) in [
        ("st1", "input", [false, false, true]),
        ("ch1", "input", [true, false, true]),
        ("st1", "postFader", [true, false, false]),
        ("ch1", "preFader", [false; 3]),
    ] {
        let path = format!("/channels/{ch}/meterTap");
        let vcas = s["vcas"].clone();
        crate::state::set_at(&mut s, &path, json!(tap));
        let before = d.targets().clone();
        d.update(&shape, &[], &s, &vcas, [path.as_str()]);
        assert_eq!(d.targets().pre_strip_meters, want, "{ch} at {tap}");
        assert_eq!(d.targets().pre_strip_meters, pre_strip_meters(&shape, &s));
        let slot = ["ch1", "ch2", "st1"].iter().position(|c| *c == ch).unwrap();
        let entries = before.diff(d.targets());
        assert_eq!(
            entries,
            [Entry::PreStripMeter {
                slot,
                enabled: tap == "input"
            }]
        );
    }
    s["channels"]["st1"]["meterTap"] = json!("input");
    let on = targets(&shape, &[], &s);
    assert_eq!(
        pre(&on),
        [Entry::PreStripMeter {
            slot: 2,
            enabled: true
        }]
    );
}

/// A bus no output carries gets the columns an output of it would:
/// cf, after the outputs, carries what an output of cf would.
#[test]
fn a_bus_no_output_carries_has_its_own_columns() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["ch1"]["fader"] = json!(0);
    s["channels"]["ch1"]["routes"]["cf"] = json!(true);
    s["buses"]["cf"]["fader"] = json!(-6);
    let m = matrix(&shape, &s);
    let mut with_output = crate::shape::tests::small();
    with_output["outputs"] = json!([
        { "id": "o-cf", "name": "CF", "source": "cf", "width": 2, "layout": "stereo" },
    ]);
    let cf = matrix(&Shape::parse(&with_output).unwrap(), &s);
    // Without o-ch1 the shape has no pre-strip row, m's last.
    assert_eq!(cf.len() + 1, m.len());
    for (row, (r, cf)) in m.iter().zip(&cf).enumerate() {
        assert_eq!(r[8..10], cf[..2], "row {row}");
    }
    assert!(m[0][8] > 0.3);
}

/// A pre-fader meter tap reads the channel after its strip as it is,
/// a post-fader one adds fader, mute and VCAs, and an input tap reads
/// the meter before the strip, whatever the fader.
#[test]
fn a_post_fader_meter_tap_follows_the_fader() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["ch1"]["fader"] = json!(-6);
    s["channels"]["ch1"]["meterTap"] = json!("preFader");
    s["channels"]["ch2"]["fader"] = json!(-6);
    s["channels"]["ch2"]["meterTap"] = json!("postFader");
    s["channels"]["st1"]["meterTap"] = json!("postFader");
    s["channels"]["st1"]["fader"] = json!(0);
    s["channels"]["st1"]["mute"] = json!(true);
    let t = meter_taps(&shape, &s);
    let tap = |meter| MeterTap {
        meter,
        pre_strip: false,
    };
    assert_eq!(t, [tap(1.0), tap(gain(-6.0)), tap(0.0)]);
    s["channels"]["ch2"]["meterTap"] = json!("input");
    let input = MeterTap {
        meter: 1.0,
        pre_strip: true,
    };
    assert_eq!(meter_taps(&shape, &s)[1], input);
}

#[test]
fn a_mono_channel_pans_at_constant_power_onto_a_stereo_bus() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["ch1"]["fader"] = json!(0);
    let m = matrix(&shape, &s);
    let h = std::f64::consts::FRAC_1_SQRT_2;
    assert!(close(m[0][0], h) && close(m[0][1], h));
    s["channels"]["ch1"]["pan"]["lr"] = json!(-1);
    let m = matrix(&shape, &s);
    assert!(close(m[0][0], 1.0) && close(m[0][1], 0.0));
}

/// Fader, mute, VCA and bus fader multiply into one coefficient;
/// trim, leg trim and polarity are the strip's, so not in it.
#[test]
fn the_gain_stages_multiply() {
    let shape = shape();
    let mut s = shape.initial_state();
    let c = &mut s["channels"]["st1"];
    c["fader"] = json!(-6);
    c["trim"] = json!(3);
    c["legTrim"] = json!([0, -3]);
    c["polarity"] = json!([false, true]);
    s["buses"]["pgm"]["fader"] = json!(-10);
    s["vcas"]["vca1"]["members"] = json!(["st1"]);
    s["vcas"]["vca1"]["offset"] = json!(1);
    let m = matrix(&shape, &s);
    assert!(close(m[2][0], gain(-6.0 + 1.0 - 10.0)));
    assert!(close(m[2][1], 0.0));
    assert!(close(m[3][1], gain(-6.0 + 1.0 - 10.0)));
    s["vcas"]["vca1"]["mute"] = json!(true);
    assert!(matrix(&shape, &s)[2].iter().take(2).all(|g| *g == 0.0));
    s["vcas"]["vca1"]["mute"] = json!(false);
    s["channels"]["st1"]["mute"] = json!(true);
    assert!(matrix(&shape, &s)[2].iter().take(2).all(|g| *g == 0.0));
}

#[test]
fn a_route_off_or_a_closed_afv_takes_the_channel_off_the_bus() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["ch2"]["fader"] = json!(0);
    assert!(matrix(&shape, &s)[1][0] > 0.0);
    s["afv"]["ch2"]["enabled"] = json!(true);
    s["afv"]["ch2"]["open"] = json!(false);
    s["afv"]["ch2"]["offLevel"] = json!(-20);
    let h = std::f64::consts::FRAC_1_SQRT_2;
    assert!(close(matrix(&shape, &s)[1][0], h * gain(-20.0)));
    s["channels"]["ch2"]["routes"]["pgm"] = json!(false);
    assert_eq!(matrix(&shape, &s)[1][0], 0.0);
}

/// A send taps pre or post fader at its level; the mix-minus is the
/// bus without its excluded channels and without the bus fader.
#[test]
fn sends_and_mix_minus() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["ch1"]["sends"]["aux1"] =
        json!({ "on": true, "level": -6, "pan": 0, "pre": true });
    assert!(close(matrix(&shape, &s)[0][2], gain(-6.0)));
    s["channels"]["ch1"]["sends"]["aux1"]["pre"] = json!(false);
    assert_eq!(matrix(&shape, &s)[0][2], 0.0);
    s["channels"]["ch1"]["fader"] = json!(0);
    s["channels"]["ch2"]["fader"] = json!(0);
    s["buses"]["pgm"]["fader"] = json!(-20);
    s["mixMinus"]["mm1"]["exclude"] = json!(["ch1"]);
    let m = matrix(&shape, &s);
    let h = std::f64::consts::FRAC_1_SQRT_2;
    assert_eq!(m[0][3], 0.0);
    assert!(close(m[1][3], h));
}

/// Model `Interrupt`: while active, the mix-minus output carries its
/// source channel alone, after its strip and before its fader, panned
/// as the channel is, at the mix-minus fader and mute.
#[test]
fn an_interrupt_replaces_the_mix_minus_with_its_source_before_the_fader() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["ch2"]["fader"] = json!(0);
    s["channels"]["st1"]["fader"] = json!(0);
    s["mixMinus"]["mm1"]["interrupt"] = json!({ "source": "ch1", "active": true });
    let h = std::f64::consts::FRAC_1_SQRT_2;
    let m = matrix(&shape, &s);
    assert!(close(m[0][3], h) && close(m[0][4], h), "{m:?}");
    assert!(
        (1..4).all(|row| close(m[row][3], 0.0) && close(m[row][4], 0.0)),
        "{m:?}"
    );
    // The programme and the monitor still carry the mix.
    assert!(close(m[1][0], h) && close(m[0][0], 0.0), "{m:?}");
    s["channels"]["ch1"]["pan"]["lr"] = json!(-1);
    s["mixMinus"]["mm1"]["fader"] = json!(-6);
    let m = matrix(&shape, &s);
    assert!(close(m[0][3], gain(-6.0)) && close(m[0][4], 0.0), "{m:?}");
    s["mixMinus"]["mm1"]["mute"] = json!(true);
    assert!(
        matrix(&shape, &s)
            .iter()
            .all(|r| close(r[3], 0.0) && close(r[4], 0.0))
    );
}

/// A wider source, or a narrower mix-minus, folds as the mix path
/// folds it: with the source alone open at 0 dB, the interrupted
/// column is the one the mix carries.
#[test]
fn an_interrupt_folds_its_source_as_the_mix_does() {
    for (source, width, layout) in [("st1", 2, "stereo"), ("st1", 1, "mono"), ("ch1", 1, "mono")] {
        let mut s = small();
        s["mixMinus"][0]["width"] = json!(width);
        s["mixMinus"][0]["layout"] = json!(layout);
        s["outputs"] = json!([
            { "id": "o-mm", "name": "N-1", "source": "mm1", "width": width, "layout": layout },
        ]);
        let shape = Shape::parse(&s).unwrap();
        let mut state = shape.initial_state();
        state["channels"][source]["fader"] = json!(0);
        let mix = matrix(&shape, &state);
        state["mixMinus"]["mm1"]["interrupt"] = json!({ "source": source, "active": true });
        let talk = matrix(&shape, &state);
        // The output's own columns; the monitor, dimmed while
        // talking, follows later.
        let own =
            |m: &[Vec<f32>]| -> Vec<Vec<f32>> { m.iter().map(|r| r[..width].to_vec()).collect() };
        assert_eq!(own(&talk), own(&mix), "{source} onto {layout}");
        assert!(
            own(&talk).iter().flatten().any(|&g| g > 0.1),
            "{source}: {talk:?}"
        );
    }
    // Stereo onto stereo: each leg onto its own.
    let shape = shape();
    let mut s = shape.initial_state();
    s["mixMinus"]["mm1"]["interrupt"] = json!({ "source": "st1", "active": true });
    let m = matrix(&shape, &s);
    assert!(close(m[2][3], 1.0) && close(m[3][4], 1.0), "{m:?}");
    assert!(close(m[2][4], 0.0) && close(m[3][3], 0.0), "{m:?}");
    assert!(close(m[0][3], 0.0) && close(m[1][3], 0.0), "{m:?}");
}

/// A write to an interrupt, or to the whole mix-minus, moves the
/// monitor while talkbackDim is on, and not with it off; the leaf
/// writes the contract takes and the objects above them alike.
#[test]
fn an_interrupt_write_moves_the_monitor_only_with_talkback_dim() {
    let shape = shape();
    let mut s = shape.initial_state();
    for path in [
        "/mixMinus/mm1",
        "/mixMinus/mm1/interrupt",
        "/mixMinus/mm1/interrupt/source",
        "/mixMinus/mm1/interrupt/active",
    ] {
        let of = |s: &Value| Moved::of(&shape, s, &Value::Null, [path]);
        assert!(of(&s).monitor, "{path}");
        assert!(of(&s).elements.contains("mm1"), "{path}");
        s["monitor"]["talkbackDim"] = json!(false);
        assert!(!of(&s).monitor, "{path} with talkbackDim off");
        s["monitor"]["talkbackDim"] = json!(true);
    }
    assert!(!Moved::of(&shape, &s, &Value::Null, ["/mixMinus/mm1/fader"]).monitor);
    // The masks follow an interrupt, whatever talkbackDim says.
    for path in ["/mixMinus/mm1", "/mixMinus/mm1/interrupt/active"] {
        assert!(Moved::of(&shape, &s, &Value::Null, [path]).masks, "{path}");
    }
    assert!(!Moved::of(&shape, &s, &Value::Null, ["/mixMinus/mm1/fader"]).masks);
}

/// A closed or muted fader does not silence the talkback: the
/// interrupt taps before the fader.
#[test]
fn an_interrupt_carries_its_source_with_the_fader_closed() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["ch1"]["mute"] = json!(true);
    s["mixMinus"]["mm1"]["interrupt"] = json!({ "source": "ch1", "active": true });
    let h = std::f64::consts::FRAC_1_SQRT_2;
    let m = matrix(&shape, &s);
    assert!(close(m[0][3], h) && close(m[0][4], h), "{m:?}");
}

/// Released, or active with no source, the mix-minus carries its mix
/// again, bit for bit.
#[test]
fn a_released_interrupt_restores_the_mix() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["ch1"]["fader"] = json!(-6);
    s["channels"]["ch2"]["fader"] = json!(0);
    s["mixMinus"]["mm1"]["exclude"] = json!(["ch2"]);
    let mix = matrix(&shape, &s);
    assert!(close(
        mix[0][3],
        gain(-6.0) * std::f64::consts::FRAC_1_SQRT_2
    ));
    s["mixMinus"]["mm1"]["interrupt"] = json!({ "source": "ch2", "active": true });
    let talk = matrix(&shape, &s);
    assert!(close(talk[0][3], 0.0) && talk[1][3] > 0.5, "{talk:?}");
    s["mixMinus"]["mm1"]["interrupt"]["active"] = json!(false);
    assert_eq!(matrix(&shape, &s), mix);
    s["mixMinus"]["mm1"]["interrupt"] = json!({ "source": null, "active": true });
    assert_eq!(matrix(&shape, &s), mix);
}

/// Model `MonitorState.talkbackDim`: the monitor dims by its dim depth
/// while a mix-minus carries its talkback, once with dim on as well,
/// and not with talkbackDim off.
#[test]
fn talkback_dims_the_monitor_while_an_interrupt_is_active() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["st1"]["fader"] = json!(0);
    s["monitor"]["level"] = json!(0);
    assert_eq!(s["monitor"]["talkbackDim"], json!(true));
    assert!(close(matrix(&shape, &s)[2][5], 1.0));
    s["mixMinus"]["mm1"]["interrupt"]["active"] = json!(true);
    assert!(
        close(matrix(&shape, &s)[2][5], 1.0),
        "no source, no talkback"
    );
    s["mixMinus"]["mm1"]["interrupt"]["source"] = json!("ch1");
    assert!(close(matrix(&shape, &s)[2][5], gain(-20.0)));
    s["monitor"]["dim"] = json!(true);
    assert!(close(matrix(&shape, &s)[2][5], gain(-20.0)));
    s["monitor"]["dim"] = json!(false);
    s["monitor"]["talkbackDim"] = json!(false);
    assert!(close(matrix(&shape, &s)[2][5], 1.0));
    s["monitor"]["talkbackDim"] = json!(true);
    s["mixMinus"]["mm1"]["interrupt"]["active"] = json!(false);
    assert!(close(matrix(&shape, &s)[2][5], 1.0));
}

/// A listen and a talkback together: PFL on the monitor is dimmed by
/// the talkback dim once, also with dim on, and the monitor on an
/// interrupted mix-minus plays the talkback, dimmed.
#[test]
fn a_listen_and_a_talkback_share_one_dim_on_the_monitor() {
    let shape = shape();
    let mut s = shape.initial_state();
    let h = std::f64::consts::FRAC_1_SQRT_2;
    s["monitor"]["level"] = json!(0);
    s["channels"]["ch1"]["solo"] = json!("pfl");
    s["mixMinus"]["mm1"]["interrupt"] = json!({ "source": "ch2", "active": true });
    // PFL on ch1 while ch2 talks: dimmed, ch2 not heard.
    let m = matrix(&shape, &s);
    assert!(close(m[0][5], h * gain(-20.0)), "{m:?}");
    assert!(close(m[1][5], 0.0), "{m:?}");
    s["monitor"]["dim"] = json!(true);
    assert!(
        close(matrix(&shape, &s)[0][5], h * gain(-20.0)),
        "dimmed once"
    );
    s["monitor"]["dim"] = json!(false);
    // Released: the PFL plays at full level again.
    s["mixMinus"]["mm1"]["interrupt"]["active"] = json!(false);
    assert!(close(matrix(&shape, &s)[0][5], h));
    // No solo, the monitor on the interrupted mix-minus: the talkback.
    s["channels"]["ch1"]["solo"] = json!("off");
    s["monitor"]["source"] = json!("mm1");
    s["mixMinus"]["mm1"]["interrupt"]["active"] = json!(true);
    let m = matrix(&shape, &s);
    assert!(close(m[1][5], h * gain(-20.0)), "{m:?}");
    assert!(close(m[0][5], 0.0), "{m:?}");
}

/// A stereo channel's pan balances it: the opposite leg lowers and
/// the near one stays, on a stereo bus; a mono channel's pan still
/// places it at constant power.
#[test]
fn a_stereo_channels_pan_balances_its_legs() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["st1"]["fader"] = json!(0);
    s["channels"]["st1"]["pan"]["lr"] = json!(-0.5);
    // Rows: ch1, ch2, st1 L, st1 R; columns o-pgm L R.
    let m = matrix(&shape, &s);
    assert!(close(m[2][0], 1.0) && close(m[2][1], 0.0), "{m:?}");
    assert!(close(m[3][1], 0.5) && close(m[3][0], 0.0), "{m:?}");
    s["channels"]["st1"]["pan"]["lr"] = json!(0.5);
    let m = matrix(&shape, &s);
    assert!(close(m[2][0], 0.5) && close(m[3][1], 1.0), "{m:?}");
}

/// A stereo channel's image feeds one leg to both, or swaps them,
/// before the balance, on every bus, aux and mix-minus it feeds, and
/// not on its direct out.
#[test]
fn a_stereo_images_use_of_its_legs_reaches_what_it_feeds() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["st1"]["fader"] = json!(0);
    let at = |s: &Value| matrix(&shape, s);
    // (image, [L->L, L->R, R->L, R->R]) onto the programme.
    for (image, want) in [
        ("normal", [1.0, 0.0, 0.0, 1.0]),
        ("leftToBoth", [1.0, 1.0, 0.0, 0.0]),
        ("rightToBoth", [0.0, 0.0, 1.0, 1.0]),
        ("swap", [0.0, 1.0, 1.0, 0.0]),
    ] {
        s["channels"]["st1"]["image"] = json!(image);
        let m = at(&s);
        let got = [m[2][0], m[2][1], m[3][0], m[3][1]];
        assert!(
            got.iter().zip(want).all(|(g, w)| close(*g, w)),
            "{image}: {m:?}"
        );
    }
    // The image comes before the balance: the left leg to both, the
    // right one lowered by a pan of -1 on the way.
    s["channels"]["st1"]["image"] = json!("leftToBoth");
    s["channels"]["st1"]["pan"]["lr"] = json!(-1);
    let m = at(&s);
    assert!(close(m[2][0], 1.0) && close(m[2][1], 0.0), "{m:?}");
    // The mix-minus follows the bus, imaged alike.
    s["channels"]["st1"]["pan"]["lr"] = json!(0);
    s["channels"]["st1"]["image"] = json!("swap");
    let m = at(&s);
    assert!(close(m[2][3], 0.0) && close(m[2][4], 1.0), "{m:?}");
}

/// The image reaches an aux send and the AFL listen as it reaches a
/// bus, with the send's and the channel's pan, and a stereo direct
/// out stays as the channel is.
#[test]
fn the_image_reaches_sends_and_listens_but_not_the_direct_out() {
    let mut v = small();
    v["outputs"] = json!([
        { "id": "o-aux", "name": "IFB", "source": "aux1", "width": 1, "layout": "mono" },
        { "id": "o-mon", "name": "Mon", "source": "mon", "width": 2, "layout": "stereo" },
        { "id": "o-st1", "name": "DO", "source": "st1", "width": 2, "layout": "stereo" },
    ]);
    let shape = Shape::parse(&v).unwrap();
    let mut s = shape.initial_state();
    s["channels"]["st1"]["fader"] = json!(0);
    s["channels"]["st1"]["image"] = json!("leftToBoth");
    s["channels"]["st1"]["sends"]["aux1"] =
        json!({ "on": true, "level": 0, "pan": 0, "pre": false });
    s["monitor"]["level"] = json!(0);
    // Rows: ch1, ch2, st1 L, st1 R. Columns: o-aux, o-mon L R, o-st1 L R.
    let m = matrix(&shape, &s);
    // Both legs carry the left one onto the mono aux, -6 dB each.
    assert!(close(m[2][0], 1.0) && close(m[3][0], 0.0), "{m:?}");
    // The direct out is the channel: leg to leg, image or not.
    assert!(close(m[2][3], 1.0) && close(m[3][4], 1.0), "{m:?}");
    assert!(close(m[2][4], 0.0) && close(m[3][3], 0.0), "{m:?}");
    // AFL at the channel's pan and image: the left leg to both, the
    // right one of them lowered by a pan of -1.
    s["channels"]["st1"]["solo"] = json!("afl");
    s["channels"]["st1"]["pan"]["lr"] = json!(-1);
    let m = matrix(&shape, &s);
    assert!(close(m[2][1], 1.0) && close(m[2][2], 0.0), "{m:?}");
    assert!(close(m[3][1], 0.0) && close(m[3][2], 0.0), "{m:?}");
}

/// The monitor's image takes its stereo legs after the fold, then
/// the speaker mutes: left to both, right to both, or swapped.
#[test]
fn the_monitors_image_uses_the_legs_it_plays() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["st1"]["fader"] = json!(0);
    s["channels"]["st1"]["pan"]["lr"] = json!(0);
    s["monitor"]["level"] = json!(0);
    for (image, want) in [
        ("normal", [1.0, 0.0, 0.0, 1.0]),
        ("leftToBoth", [1.0, 1.0, 0.0, 0.0]),
        ("rightToBoth", [0.0, 0.0, 1.0, 1.0]),
        ("swap", [0.0, 1.0, 1.0, 0.0]),
    ] {
        s["monitor"]["image"] = json!(image);
        let m = matrix(&shape, &s);
        let got = [m[2][5], m[2][6], m[3][5], m[3][6]];
        assert!(
            got.iter().zip(want).all(|(g, w)| close(*g, w)),
            "{image}: {m:?}"
        );
    }
}

fn field(lr: f64, fr: f64, divergence: f64) -> Place {
    Place {
        lr,
        fr,
        divergence,
        lfe: -90.0,
    }
}

fn legs_of(to: usize, p: &Place) -> Vec<f64> {
    (0..to).map(|j| field_gain(to, j, p)).collect()
}

/// The surround panner's law points on 5.1: the front of the field
/// pans pairwise at constant power, the centre is the C leg alone,
/// divergence moves its power to L and R, fr crossfades front and
/// rear and the rear pans between the surrounds.
#[test]
fn the_surround_panner_places_a_mono_source_in_the_field() {
    let h = std::f64::consts::FRAC_1_SQRT_2;
    let is = |got: Vec<f64>, want: [f64; 6]| {
        assert!(
            got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1e-12),
            "{got:?} vs {want:?}"
        );
    };
    // L R C LFE Ls Rs.
    is(
        legs_of(6, &field(0.0, -1.0, 0.0)),
        [0.0, 0.0, 1.0, 0.0, 0.0, 0.0],
    );
    is(
        legs_of(6, &field(-1.0, -1.0, 0.0)),
        [1.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    );
    is(
        legs_of(6, &field(1.0, -1.0, 0.0)),
        [0.0, 1.0, 0.0, 0.0, 0.0, 0.0],
    );
    is(
        legs_of(6, &field(-0.5, -1.0, 0.0)),
        [h, 0.0, h, 0.0, 0.0, 0.0],
    );
    is(
        legs_of(6, &field(0.5, -1.0, 0.0)),
        [0.0, h, h, 0.0, 0.0, 0.0],
    );
    // Full divergence is a plain constant-power pan between L and R,
    // as a mono channel was panned onto a surround bus before the
    // field.
    let quarter = std::f64::consts::FRAC_PI_4;
    for lr in [-1.0, -0.5, 0.0, 0.3, 1.0] {
        let theta = (lr + 1.0) * quarter;
        is(
            legs_of(6, &field(lr, -1.0, 1.0)),
            [libm::cos(theta), libm::sin(theta), 0.0, 0.0, 0.0, 0.0],
        );
    }
    // Between, the centre's power goes to L and R.
    is(
        legs_of(6, &field(0.0, -1.0, 1.0)),
        [h, h, 0.0, 0.0, 0.0, 0.0],
    );
    let (c, l) = (libm::cos(0.25 * std::f64::consts::FRAC_PI_2), 0.0);
    let half = libm::sin(0.25 * std::f64::consts::FRAC_PI_2) * h;
    is(
        legs_of(6, &field(0.0, -1.0, 0.25)),
        [half, half, c, l, 0.0, 0.0],
    );
    // The rear: the surrounds alone, panned between them.
    is(
        legs_of(6, &field(0.0, 1.0, 0.0)),
        [0.0, 0.0, 0.0, 0.0, h, h],
    );
    is(
        legs_of(6, &field(-1.0, 1.0, 0.0)),
        [0.0, 0.0, 0.0, 0.0, 1.0, 0.0],
    );
    is(
        legs_of(6, &field(1.0, 1.0, 0.0)),
        [0.0, 0.0, 0.0, 0.0, 0.0, 1.0],
    );
    // fr crossfades at constant power: half the power each.
    is(
        legs_of(6, &field(0.0, 0.0, 0.0)),
        [0.0, 0.0, h, 0.0, 0.5, 0.5],
    );
}

/// The LFE leg takes its send as a gain of its own, whatever the pan
/// and divergence, and is silent at -90 dB; 7.1 shares the rear
/// between the side and the rear surrounds.
#[test]
fn the_lfe_send_is_a_gain_of_its_own_and_7_1_splits_its_rear() {
    let h = std::f64::consts::FRAC_1_SQRT_2;
    for (lr, fr, d) in [(0.0, -1.0, 0.0), (-1.0, 1.0, 1.0), (0.3, 0.2, 0.6)] {
        let p = Place {
            lfe: -6.0,
            ..field(lr, fr, d)
        };
        assert!((field_gain(6, 3, &p) - gain(-6.0)).abs() < 1e-12);
        assert!((field_gain(8, 3, &p) - gain(-6.0)).abs() < 1e-12);
        assert_eq!(field_gain(6, 3, &field(lr, fr, d)), 0.0, "-90 is off");
    }
    // L R C LFE Ls Rs Lrs Rrs.
    let got = legs_of(8, &field(-1.0, 1.0, 0.0));
    let want = [0.0, 0.0, 0.0, 0.0, h, 0.0, h, 0.0];
    assert!(
        got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1e-12),
        "{got:?}"
    );
    let got = legs_of(8, &field(0.0, 1.0, 0.0));
    assert!(
        (got[4] - 0.5).abs() < 1e-12 && (got[7] - 0.5).abs() < 1e-12,
        "{got:?}"
    );
}

/// Every position keeps the power of the source: the legs but the
/// LFE sum to unity over the field, on 5.1 and on 7.1.
#[test]
fn the_surround_panner_keeps_constant_power_over_the_field() {
    for to in [6, 8] {
        for lr in (-10..=10).map(|n| f64::from(n) / 10.0) {
            for fr in (-10..=10).map(|n| f64::from(n) / 10.0) {
                for d in (0..=10).map(|n| f64::from(n) / 10.0) {
                    let power: f64 = (0..to)
                        .filter(|&j| j != 3)
                        .map(|j| field_gain(to, j, &field(lr, fr, d)).powi(2))
                        .sum();
                    assert!(
                        (power - 1.0).abs() < 1e-9,
                        "{to} legs at {lr} {fr} {d}: {power}"
                    );
                }
            }
        }
    }
}

/// The matrix reads a mono channel's whole pan onto a 5.1 bus, and
/// its fold-down onto the stereo output follows from the legs.
#[test]
fn a_mono_channels_pan_reaches_a_surround_bus() {
    let h = std::f64::consts::FRAC_1_SQRT_2;
    let shape = Shape::parse(&crate::shape::tests::surround()).unwrap();
    let mut s = shape.initial_state();
    s["channels"]["ch1"]["fader"] = json!(0);
    s["channels"]["ch1"]["routes"]["pgm"] = json!(true);
    // Rows ch1, ch2, st1 L R, sur x6; columns out-51 x6, out-lr x2.
    // A new mono channel is the L/R pair of the pan before the field.
    let m = matrix(&shape, &s);
    let row: Vec<f64> = m[0][..6].iter().map(|g| f64::from(*g)).collect();
    assert!(
        (row[0] - h).abs() < 1e-6 && (row[1] - h).abs() < 1e-6 && row[2].abs() < 1e-6,
        "{row:?}"
    );
    // No divergence: the centre leg alone, the fold onto Lo and Ro
    // taking it at -3 dB.
    s["channels"]["ch1"]["pan"]["spread"] = json!(0);
    let m = matrix(&shape, &s);
    let row: Vec<f64> = m[0][..6].iter().map(|g| f64::from(*g)).collect();
    assert!(
        (row[2] - 1.0).abs() < 1e-6 && row[0].abs() < 1e-6 && row[1].abs() < 1e-6,
        "{row:?}"
    );
    assert!(close(m[0][6], h) && close(m[0][7], h), "{m:?}");
    s["channels"]["ch1"]["pan"] = json!({ "lr": 0, "frontRear": 1, "spread": 0, "lfeSend": -6 });
    let m = matrix(&shape, &s);
    let row: Vec<f64> = m[0][..6].iter().map(|g| f64::from(*g)).collect();
    assert!(
        row[2].abs() < 1e-6 && (row[4] - h).abs() < 1e-6 && (row[5] - h).abs() < 1e-6,
        "{row:?}"
    );
    assert!((row[3] - gain(-6.0)).abs() < 1e-6, "the LFE send: {row:?}");
}

/// A mono channel's send, its PFL and its AFL on a surround element
/// keep the L/R pair they were before the field: a send and a PFL
/// by the position they have, the AFL by the channel's own place.
#[test]
fn sends_and_listens_of_a_mono_channel_stay_the_lr_pair_on_a_surround_element() {
    let mut v = crate::shape::tests::surround();
    v["auxes"] = json!([{ "id": "aux1", "name": "IFB", "width": 6, "layout": "5.1" }]);
    v["monitorOutputs"] = json!([{ "id": "mon", "name": "Mon", "width": 6, "layout": "5.1" }]);
    v["outputs"] = json!([
        { "id": "o-aux", "name": "IFB", "source": "aux1", "width": 6, "layout": "5.1" },
        { "id": "o-mon", "name": "Mon", "source": "mon", "width": 6, "layout": "5.1" },
    ]);
    let shape = Shape::parse(&v).unwrap();
    let mut s = shape.initial_state();
    s["channels"]["ch1"]["fader"] = json!(0);
    s["channels"]["ch1"]["sends"]["aux1"] =
        json!({ "on": true, "level": 0, "pan": 0.3, "pre": false });
    s["monitor"]["level"] = json!(0);
    let pair = |lr: f64| {
        let theta = (lr + 1.0) * std::f64::consts::FRAC_PI_4;
        [libm::cos(theta), libm::sin(theta), 0.0, 0.0, 0.0, 0.0]
    };
    let legs = |m: &[Vec<f32>], first: usize| -> Vec<f64> {
        m[0][first..first + 6]
            .iter()
            .map(|g| f64::from(*g))
            .collect()
    };
    let is = |got: Vec<f64>, want: [f64; 6]| {
        assert!(
            got.iter().zip(want).all(|(g, w)| (g - w).abs() < 1e-6),
            "{got:?} vs {want:?}"
        );
    };
    // Columns: o-aux x6, o-mon x6.
    is(legs(&matrix(&shape, &s), 0), pair(0.3));
    // PFL: centred whatever the channel's pan.
    s["channels"]["ch1"]["pan"]["lr"] = json!(-0.5);
    s["channels"]["ch1"]["solo"] = json!("pfl");
    is(legs(&matrix(&shape, &s), 6), pair(0.0));
    // AFL: at the channel's place; with its divergence lowered the
    // centre leg takes it.
    s["channels"]["ch1"]["solo"] = json!("afl");
    is(legs(&matrix(&shape, &s), 6), pair(-0.5));
    s["channels"]["ch1"]["pan"]["spread"] = json!(0);
    s["channels"]["ch1"]["pan"]["lr"] = json!(0);
    let got = legs(&matrix(&shape, &s), 6);
    is(got, [0.0, 0.0, 1.0, 0.0, 0.0, 0.0]);
}

#[test]
fn the_monitor_follows_its_source_and_its_own_controls() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["st1"]["fader"] = json!(0);
    s["monitor"]["level"] = json!(0);
    let m = matrix(&shape, &s);
    assert!(close(m[2][5], 1.0) && close(m[3][6], 1.0) && m[2][6] == 0.0);
    s["monitor"]["dim"] = json!(true);
    assert!(close(matrix(&shape, &s)[2][5], gain(-20.0)));
    s["monitor"]["dim"] = json!(false);
    s["monitor"]["mono"] = json!(true);
    let m = matrix(&shape, &s);
    assert!(close(m[3][5], gain(-3.0)) && close(m[2][6], gain(-3.0)));
    s["monitor"]["speakerMute"] = json!([true, false]);
    assert_eq!(matrix(&shape, &s)[2][5], 0.0);
    s["monitor"]["cut"] = json!(true);
    assert!(
        matrix(&shape, &s)
            .iter()
            .all(|r| r[5] == 0.0 && r[6] == 0.0)
    );
}

/// MonitorMeter: the monitor's meter columns carry what it plays
/// before its level, dim, talkback dim, cut, mono and speaker mutes and
/// solos, each of which moves its output's columns alone, and a write
/// of any of them derives the bits of the whole state. The listen sum,
/// PFL over AFL, reaches the meter while the output is cut.
#[test]
fn the_monitor_meter_reads_before_level_dim_cut_mono_and_speaker_mutes() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["st1"]["fader"] = json!(0);
    s["monitor"]["level"] = json!(0);
    // The channels' rows of two columns from `first` on: o-mon's from
    // 5, the monitor meter's from 10.
    let lr = |s: &Value, first: usize| -> Vec<[f32; 2]> {
        let m = matrix(&shape, s);
        m[..4].iter().map(|r| [r[first], r[first + 1]]).collect()
    };
    let source = lr(&s, 5);
    assert_eq!(lr(&s, 10), source, "at 0 dB the meter is the output");
    assert!(close(source[2][0], 1.0) && close(source[3][1], 1.0));
    for (path, value) in [
        ("/monitor/level", json!(-6)),
        ("/monitor/dim", json!(true)),
        (
            "/mixMinus/mm1/interrupt",
            json!({ "source": "ch1", "active": true }),
        ),
        ("/monitor/cut", json!(true)),
        ("/monitor/mono", json!(true)),
        ("/monitor/speakerMute/1", json!(true)),
        ("/monitor/speakerSolo/0", json!(true)),
    ] {
        let mut s = s.clone();
        let mut d = Derived::new(&shape, &[], &s);
        let vcas = s["vcas"].clone();
        crate::state::set_at(&mut s, path, value);
        d.update(&shape, &[], &s, &vcas, [path]);
        assert_eq!(d.targets().gains, matrix(&shape, &s), "{path}");
        assert_ne!(lr(&s, 5), source, "the output with {path}");
        assert_eq!(lr(&s, 10), source, "the meter with {path}");
    }
    s["monitor"]["cut"] = json!(true);
    s["channels"]["ch2"]["fader"] = json!(0);
    s["channels"]["ch2"]["solo"] = json!("afl");
    s["channels"]["ch1"]["solo"] = json!("pfl");
    let h = std::f64::consts::FRAC_1_SQRT_2;
    let meter = lr(&s, 10);
    assert!(close(meter[0][0], h) && close(meter[0][1], h), "{meter:?}");
    assert!(meter[1..].iter().flatten().all(|g| *g == 0.0), "{meter:?}");
    assert!(lr(&s, 5).iter().flatten().all(|g| *g == 0.0), "cut");
    // Solos, the monitor's image and its source move the meter's
    // columns while the output is cut: a write of each derives the
    // bits of the whole state there.
    let mut d = Derived::new(&shape, &[], &s);
    for (path, value) in [
        ("/channels/ch1/solo", json!("off")),
        ("/channels/ch2/solo", json!("off")),
        ("/monitor/image", json!("swap")),
        ("/monitor/source", json!("aux1")),
        ("/channels/st1/solo", json!("pfl")),
    ] {
        let before = lr(&s, 10);
        let vcas = s["vcas"].clone();
        crate::state::set_at(&mut s, path, value);
        d.update(&shape, &[], &s, &vcas, [path]);
        assert_eq!(d.targets().gains, matrix(&shape, &s), "{path}");
        assert_ne!(lr(&s, 10), before, "the meter with {path}");
    }
}

/// PFL listens after the strip and before the fader and pan: a
/// channel with its fader closed is heard on the monitor, and the
/// source is not. A hard-panned mono channel is heard centred.
#[test]
fn a_pfl_is_heard_on_the_monitor_with_its_fader_closed() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["st1"]["fader"] = json!(0);
    s["channels"]["ch1"]["pan"]["lr"] = json!(-1);
    s["monitor"]["level"] = json!(-6);
    s["channels"]["ch1"]["solo"] = json!("pfl");
    let m = matrix(&shape, &s);
    let h = std::f64::consts::FRAC_1_SQRT_2;
    assert!(close(m[0][5], h * gain(-6.0)) && close(m[0][6], h * gain(-6.0)));
    assert!(m[1..].iter().all(|r| r[5] == 0.0 && r[6] == 0.0));
    // Hard right gives the same.
    s["channels"]["ch1"]["pan"]["lr"] = json!(1);
    let m = matrix(&shape, &s);
    assert!(close(m[0][5], h * gain(-6.0)) && close(m[0][6], h * gain(-6.0)));
    // The monitor's own controls follow.
    s["monitor"]["speakerMute"] = json!([false, true]);
    let m = matrix(&shape, &s);
    assert!(close(m[0][5], h * gain(-6.0)) && m[0][6] == 0.0);
}

/// AFL is the channel after fader, mute and VCAs and before AFV: a
/// channel whose AFV stage is closed plays at its fader gain, not at
/// the off level it adds to a bus.
#[test]
fn an_afl_is_heard_before_the_afv_stage() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["ch1"]["fader"] = json!(-6);
    s["channels"]["ch1"]["pan"]["lr"] = json!(-1);
    s["afv"]["ch1"]["enabled"] = json!(true);
    s["afv"]["ch1"]["open"] = json!(false);
    s["afv"]["ch1"]["offLevel"] = json!(-40);
    s["monitor"]["level"] = json!(0);
    s["channels"]["ch1"]["solo"] = json!("afl");
    let m = matrix(&shape, &s);
    assert!(close(m[0][5], gain(-6.0)) && m[0][6] == 0.0);
    // The bus still takes it at the off level.
    assert!(close(matrix(&shape, &s)[0][0], gain(-6.0 - 40.0)));
}

/// AFL listens to what the channel adds to the mix: nothing while its
/// fader is closed, and at its pan while it is open.
#[test]
fn an_afl_of_a_closed_fader_is_silent() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["st1"]["fader"] = json!(0);
    s["monitor"]["level"] = json!(0);
    s["channels"]["ch1"]["solo"] = json!("afl");
    let m = matrix(&shape, &s);
    assert!(m.iter().all(|r| r[5] == 0.0 && r[6] == 0.0));
    s["channels"]["ch1"]["fader"] = json!(-6);
    s["channels"]["ch1"]["pan"]["lr"] = json!(-1);
    let m = matrix(&shape, &s);
    assert!(close(m[0][5], gain(-6.0)) && close(m[0][6], 0.0));
    assert!(m[1..].iter().all(|r| r[5] == 0.0 && r[6] == 0.0));
}

/// VcaState: a VCA's solo listens to each member as if the member had
/// it set.
#[test]
fn a_vca_solo_solos_its_members() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["ch1"]["fader"] = json!(0);
    s["monitor"]["level"] = json!(0);
    s["vcas"]["vca1"]["members"] = json!(["st1"]);
    s["vcas"]["vca1"]["solo"] = json!("pfl");
    let m = matrix(&shape, &s);
    assert!(close(m[2][5], 1.0) && close(m[3][6], 1.0));
    assert!(m[2][6] == 0.0 && m[3][5] == 0.0);
    assert!(m[..2].iter().all(|r| r[5] == 0.0 && r[6] == 0.0));
    // AFL through the VCA: the member's fader and the VCA's offset.
    s["vcas"]["vca1"]["solo"] = json!("afl");
    s["vcas"]["vca1"]["offset"] = json!(-2);
    s["channels"]["st1"]["fader"] = json!(-4);
    let m = matrix(&shape, &s);
    assert!(close(m[2][5], gain(-6.0)) && close(m[3][6], gain(-6.0)));
    assert!(m[..2].iter().all(|r| r[5] == 0.0 && r[6] == 0.0));
}

/// With every solo off again the monitor plays its source, bit for bit
/// as before any solo.
#[test]
fn all_solos_off_restores_the_monitor_source() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["st1"]["fader"] = json!(0);
    s["monitor"]["level"] = json!(0);
    let source = matrix(&shape, &s);
    assert!(close(source[2][5], 1.0));
    s["channels"]["ch2"]["solo"] = json!("pfl");
    s["vcas"]["vca1"]["solo"] = json!("afl");
    assert!(matrix(&shape, &s)[2][5] == 0.0);
    s["channels"]["ch2"]["solo"] = json!("off");
    s["vcas"]["vca1"]["solo"] = json!("off");
    assert_eq!(matrix(&shape, &s), source);
}

/// MonitorMeter: PFL takes priority over AFL, so while any PFL is on
/// only the PFL listens are heard; a channel listens in PFL when its
/// own solo or a VCA's it is a member of is PFL.
#[test]
fn pfl_takes_priority_over_afl() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["ch1"]["fader"] = json!(0);
    s["channels"]["ch1"]["pan"]["lr"] = json!(-1);
    s["monitor"]["level"] = json!(0);
    s["channels"]["ch1"]["solo"] = json!("afl");
    s["channels"]["ch2"]["solo"] = json!("pfl");
    let h = std::f64::consts::FRAC_1_SQRT_2;
    let m = matrix(&shape, &s);
    assert!(m[0][5] == 0.0 && m[0][6] == 0.0, "the AFL waits");
    assert!(close(m[1][5], h) && close(m[1][6], h));
    s["channels"]["ch2"]["solo"] = json!("off");
    let m = matrix(&shape, &s);
    assert!(close(m[0][5], 1.0) && m[0][6] == 0.0, "the AFL again");
    // A VCA in PFL over a member's own AFL: the member listens in PFL.
    s["vcas"]["vca1"]["members"] = json!(["ch1"]);
    s["vcas"]["vca1"]["solo"] = json!("pfl");
    s["channels"]["ch1"]["fader"] = json!(-90);
    let m = matrix(&shape, &s);
    assert!(close(m[0][5], h) && close(m[0][6], h), "PFL centred");
}

/// PanState: a mono channel pans at constant power, a stereo one on a
/// stereo bus balances its legs without crossing them.
#[test]
fn a_stereo_channel_balances_and_a_mono_one_pans_hard_right() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["st1"]["fader"] = json!(0);
    s["channels"]["ch1"]["fader"] = json!(0);
    for p in [0.5, 1.0, -1.0, 0.0] {
        s["channels"]["st1"]["pan"]["lr"] = json!(p);
        let m = matrix(&shape, &s);
        let (l, r) = ((1.0f64 - p).min(1.0), (1.0f64 + p).min(1.0));
        assert!(close(m[2][0], l) && close(m[2][1], 0.0), "pan {p}");
        assert!(close(m[3][0], 0.0) && close(m[3][1], r), "pan {p}");
    }
    s["channels"]["ch1"]["pan"]["lr"] = json!(1);
    let m = matrix(&shape, &s);
    assert!(close(m[0][0], 0.0) && close(m[0][1], 1.0));
}

/// 5.1 and 7.1 onto stereo by ITU-R BS.775 Lo/Ro, 7.1 onto 5.1 by the
/// surround pairs, both at -3 dB, LFE dropped; narrower onto wider
/// leg to leg.
#[test]
fn wider_signals_fold_down_per_the_contract() {
    let h = std::f64::consts::FRAC_1_SQRT_2;
    // L R C LFE Ls Rs onto Lo.
    let lo: Vec<f64> = (0..6).map(|i| pan(6, 2, i, 0, 0.7)).collect();
    assert_eq!(lo, [1.0, 0.0, h, 0.0, h, 0.0]);
    let ro: Vec<f64> = (0..8).map(|i| pan(8, 2, i, 1, 0.0)).collect();
    assert_eq!(ro, [0.0, 1.0, h, 0.0, 0.0, h, 0.0, h]);
    // Lrs into Ls, Rrs into Rs, the rest leg to leg.
    assert_eq!(pan(8, 6, 4, 4, 0.0), h);
    assert_eq!(pan(8, 6, 5, 5, 0.0), h);
    assert_eq!(pan(8, 6, 6, 4, 0.0), h);
    assert_eq!(pan(8, 6, 7, 5, 0.0), h);
    assert_eq!(pan(8, 6, 2, 2, 0.0), 1.0);
    assert_eq!(pan(8, 6, 6, 0, 0.0), 0.0);
    // Stereo balances onto two legs or more: unity at the centre, the
    // opposite leg lowered linearly, the same leg to leg onto 5.1.
    assert_eq!(pan(2, 6, 1, 1, 0.0), 1.0);
    assert_eq!(pan(2, 6, 1, 1, -1.0), 0.0);
    assert_eq!(pan(2, 6, 0, 0, -1.0), 1.0);
    assert_eq!(pan(2, 6, 1, 1, -0.25), 0.75);
    assert_eq!(pan(2, 2, 0, 0, 0.5), 0.5);
    assert_eq!(pan(2, 2, 1, 1, 0.5), 1.0);
    assert_eq!(pan(2, 2, 0, 1, 0.5), 0.0, "legs stay on their own");
    assert_eq!(pan(2, 6, 1, 2, -1.0), 0.0);
    // Stereo onto mono at -6 dB per leg; 5.1 onto mono through Lo/Ro.
    assert_eq!(pan(2, 1, 0, 0, 0.0), 0.5);
    assert_eq!(pan(6, 1, 2, 0, 0.0), h);
    assert_eq!(pan(6, 1, 3, 0, 0.0), 0.0);
}

/// A 5.1 channel on a 5.1 bus carried by a 6-leg output and by a
/// stereo output: the stereo one is the bus folded by BS.775 Lo/Ro,
/// after the bus fader, and the bus keeps no meter column of its own.
fn surround() -> (Shape, Value) {
    let v = crate::shape::tests::surround();
    let shape = Shape::parse(&v).unwrap();
    let mut s = shape.initial_state();
    s["channels"]["sur"]["fader"] = json!(0);
    s["channels"]["sur"]["routes"]["pgm"] = json!(true);
    s["buses"]["pgm"]["fader"] = json!(-6);
    (shape, s)
}

#[test]
fn an_output_narrower_than_its_source_folds_it_down() {
    let (shape, s) = surround();
    let m = matrix(&shape, &s);
    let (bus, h) = (gain(-6.0), std::f64::consts::FRAC_1_SQRT_2);
    // Rows: ch1, ch2, st1 L, st1 R, then sur L R C LFE Ls Rs.
    // Columns: out-51 0..6, out-lr 6..8, then cf, aux1, mon.
    assert_eq!(m[0].len(), 6 + 2 + 2 + 1 + 2);
    let want_lr = [
        [1.0, 0.0],
        [0.0, 1.0],
        [h, h],
        [0.0, 0.0],
        [h, 0.0],
        [0.0, h],
    ];
    for (leg, lr) in want_lr.iter().enumerate() {
        let row = &m[4 + leg];
        for (j, g) in row[..6].iter().enumerate() {
            assert!(
                close(*g, if j == leg { bus } else { 0.0 }),
                "5.1 [{leg}][{j}]"
            );
        }
        assert!(close(row[6], bus * lr[0]), "Lo of leg {leg}: {}", row[6]);
        assert!(close(row[7], bus * lr[1]), "Ro of leg {leg}: {}", row[7]);
    }
}

/// Only a full-width output is the element's meter; a bus a narrower
/// output alone carries meters on columns of its own.
#[test]
fn a_fold_down_output_leaves_its_source_a_meter_unless_a_full_one_exists() {
    let (shape, _) = surround();
    assert!(shape.meter_columns().iter().all(|m| m.0 != "pgm"));
    let mut v = serde_json::to_value(&shape).unwrap();
    v["outputs"].as_array_mut().unwrap().remove(0);
    let shape = Shape::parse(&v).unwrap();
    let pgm = shape.meter_columns().into_iter().find(|m| m.0 == "pgm");
    assert_eq!(pgm, Some(("pgm", 6, "5.1")));
    let m = matrix(&shape, &surround().1);
    // out-lr, then the meters in shape order: pgm (6 legs) first.
    assert_eq!(m[0].len(), 2 + 6 + 2 + 1 + 2);
    assert!(close(m[4][2], gain(-6.0)) && close(m[4][0], gain(-6.0)));
}

/// An output wider than its source is refused; the engine would have
/// to invent legs.
#[test]
fn an_output_wider_than_its_source_is_refused() {
    let mut v = crate::shape::tests::small();
    v["outputs"] = json!([
        { "id": "o", "name": "x", "source": "aux1", "width": 2, "layout": "stereo" },
    ]);
    let e = Shape::parse(&v).unwrap_err();
    assert!(e.contains("wider than its source"), "{e}");
}

#[test]
fn a_post_fader_send_follows_the_open_fader() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["ch1"]["sends"]["aux1"] =
        json!({ "on": true, "level": -6, "pan": 0, "pre": false });
    s["channels"]["ch1"]["fader"] = json!(-4);
    assert!(close(matrix(&shape, &s)[0][2], gain(-10.0)));
}

#[test]
fn a_muted_bus_is_silent_and_its_mix_minus_is_not() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["ch1"]["fader"] = json!(0);
    s["buses"]["pgm"]["mute"] = json!(true);
    let m = matrix(&shape, &s);
    assert_eq!((m[0][0], m[0][1]), (0.0, 0.0));
    assert!(
        m[0][3] > 0.0,
        "the mix-minus is taken before bus processing"
    );
}

/// VcaState: fader and offsets add up to +20 dB at most.
#[test]
fn vca_offsets_clamp_at_20_db() {
    let shape = shape();
    let mut s = shape.initial_state();
    s["channels"]["ch1"]["fader"] = json!(10);
    s["channels"]["ch1"]["pan"]["lr"] = json!(-1);
    s["vcas"]["vca1"]["members"] = json!(["ch1"]);
    s["vcas"]["vca1"]["offset"] = json!(10);
    assert!(close(matrix(&shape, &s)[0][0], gain(20.0)));
    s["vcas"]["vca1"]["offset"] = json!(5);
    assert!(close(matrix(&shape, &s)[0][0], gain(15.0)));
}

/// MonitorState downmix: lr plays only the L and R legs of a wider
/// source, loRo folds it.
#[test]
fn the_monitor_folds_a_wider_source_by_its_downmix() {
    let mut v = small();
    v["channels"] =
        json!([{ "id": "c51", "name": "5.1", "width": 6, "layout": "5.1", "altInput": false }]);
    v["buses"] = json!([{ "id": "sur", "name": "Surround", "kind": "program", "width": 6, "layout": "5.1" }]);
    v["auxes"] = json!([]);
    v["mixMinus"] = json!([]);
    v["vcas"] = json!([]);
    v["outputs"] =
        json!([{ "id": "o-mon", "name": "Mon", "source": "mon", "width": 2, "layout": "stereo" }]);
    let shape = Shape::parse(&v).unwrap();
    let mut s = shape.initial_state();
    s["channels"]["c51"]["fader"] = json!(0);
    s["monitor"]["source"] = json!("sur");
    s["monitor"]["level"] = json!(0);
    let h = std::f64::consts::FRAC_1_SQRT_2;
    let m = matrix(&shape, &s);
    assert!(close(m[2][0], h) && close(m[4][0], h) && m[3][0] == 0.0);
    s["monitor"]["downmix"] = json!("lr");
    let m = matrix(&shape, &s);
    assert!(close(m[0][0], 1.0) && close(m[1][1], 1.0));
    assert!(m[2..].iter().all(|r| r[0] == 0.0 && r[1] == 0.0));
    // Mono sums the folded legs: LFE stays out, C is in both.
    s["monitor"]["downmix"] = json!("loRo");
    s["monitor"]["mono"] = json!(true);
    s["monitor"]["monoCompensation"] = json!(0);
    let m = matrix(&shape, &s);
    assert_eq!(m[3][0], 0.0, "LFE");
    assert!(close(m[2][0], 2.0 * h) && close(m[0][1], 1.0));
}

/// The matrix taken from per-channel and per-element factors is the
/// one computed a coefficient at a time, bit for bit, over random
/// states touching every leaf it reads: what the engine was sent
/// before.
#[test]
fn the_factored_matrix_is_the_one_before_bit_for_bit() {
    let shape = shape();
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    let channels = ["ch1", "ch2", "st1"];
    for _ in 0..400 {
        let mut s = shape.initial_state();
        for _ in 0..40 {
            let ch = channels[next(3) as usize];
            let db = json!(next(1200) as f64 / 10.0 - 100.0);
            let flag = json!(next(2) == 0);
            let (path, value) = match next(20) {
                0 => (format!("/channels/{ch}/fader"), db),
                1 => (format!("/channels/{ch}/mute"), flag),
                2 => (
                    format!("/channels/{ch}/pan/lr"),
                    json!(next(201) as f64 / 100.0 - 1.0),
                ),
                3 => (
                    format!("/channels/{ch}/routes/{}", ["pgm", "cf"][next(2) as usize]),
                    flag,
                ),
                4 => (format!("/channels/{ch}/sends/aux1/on"), flag),
                5 => (format!("/channels/{ch}/sends/aux1/level"), db),
                6 => (format!("/channels/{ch}/sends/aux1/pre"), flag),
                7 => (
                    format!("/channels/{ch}/directOut/tap"),
                    json!(["input", "preFader", "postFader"][next(3) as usize]),
                ),
                8 => (
                    "/vcas/vca1/members".into(),
                    json!(channels[..next(4) as usize]),
                ),
                9 => (
                    "/vcas/vca1/offset".into(),
                    json!(next(400) as f64 / 10.0 - 30.0),
                ),
                10 => ("/vcas/vca1/mute".into(), flag),
                11 => (
                    format!(
                        "/{}/fader",
                        ["buses/pgm", "auxes/aux1", "mixMinus/mm1"][next(3) as usize]
                    ),
                    db,
                ),
                12 => (format!("/buses/pgm/polarity/{}", next(2)), flag),
                13 => (
                    "/mixMinus/mm1/exclude".into(),
                    json!(channels[..next(4) as usize]),
                ),
                14 => (
                    format!("/afv/{ch}/{}", ["enabled", "open"][next(2) as usize]),
                    flag,
                ),
                15 => (
                    format!(
                        "/{}/solo",
                        ["channels/ch1", "channels/st1", "vcas/vca1"][next(3) as usize]
                    ),
                    json!(["off", "off", "off", "pfl", "afl"][next(5) as usize]),
                ),
                16 => (
                    "/mixMinus/mm1/interrupt/source".into(),
                    if next(2) == 0 { json!(null) } else { json!(ch) },
                ),
                17 => ("/mixMinus/mm1/interrupt/active".into(), flag),
                18 => (
                    if next(2) == 0 {
                        format!("/channels/{ch}/image")
                    } else {
                        "/monitor/image".into()
                    },
                    json!(["normal", "leftToBoth", "rightToBoth", "swap"][next(4) as usize]),
                ),
                _ => match next(9) {
                    0 => (
                        "/monitor/source".into(),
                        json!(["pgm", "cf", "aux1", "mm1"][next(4) as usize]),
                    ),
                    1 => ("/monitor/level".into(), db),
                    2 => ("/monitor/dim".into(), flag),
                    3 => ("/monitor/cut".into(), json!(next(8) == 0)),
                    4 => ("/monitor/mono".into(), flag),
                    5 => (
                        "/monitor/downmix".into(),
                        json!(["lr", "itu"][next(2) as usize]),
                    ),
                    6 => (format!("/monitor/speakerMute/{}", next(2)), flag),
                    7 => ("/monitor/talkbackDim".into(), flag),
                    _ => (format!("/monitor/speakerSolo/{}", next(2)), flag),
                },
            };
            crate::state::set_at(&mut s, &path, value);
        }
        let bits = |m: Vec<Vec<f32>>| -> Vec<Vec<u32>> {
            m.into_iter()
                .map(|r| r.into_iter().map(f32::to_bits).collect())
                .collect()
        };
        // The line-up tone is off: its rows, after the channels', are 0.
        // ch1's direct out, column 7, at the input tap is on its
        // pre-strip row, where the matrix before had it on ch1's own
        // row; the row is 0 in every other column.
        let mut m = matrix(&shape, &s);
        let mut tone = m.split_off(shape.channels.iter().map(|c| c.width).sum());
        let pre = tone.split_off(tone_widths(&shape).iter().sum());
        assert!(tone.iter().flatten().all(|g| g.to_bits() == 0), "{s}");
        assert_eq!(pre.len(), 1);
        for (col, g) in pre[0].iter().enumerate() {
            if col == 7 && s["channels"]["ch1"]["directOut"]["tap"] == "input" {
                assert_eq!(m[0][col].to_bits(), 0, "{s}");
                m[0][col] = *g;
            } else {
                assert_eq!(g.to_bits(), 0, "column {col}: {s}");
            }
        }
        assert_eq!(bits(m), bits(before::matrix(&shape, &s)), "{s}");
    }
}

/// The state of [`shape`] with ch1 and st1 open on every bus and
/// sent to aux1, and every bus, aux and mix-minus at 0 dB: every
/// column of an element carries a mix.
fn mixing(shape: &Shape) -> Value {
    let mut s = shape.initial_state();
    for ch in ["ch1", "st1"] {
        s["channels"][ch]["fader"] = json!(0);
        s["channels"][ch]["sends"]["aux1"]["on"] = json!(true);
        s["channels"][ch]["sends"]["aux1"]["level"] = json!(0);
    }
    for path in ["/buses/pgm", "/buses/cf", "/auxes/aux1", "/mixMinus/mm1"] {
        crate::state::set_at(&mut s, &format!("{path}/fader"), json!(0));
    }
    s
}

/// The line-up tone generators' rows, after the channels', and the
/// channels'.
fn tone_rows(m: &[Vec<f32>], shape: &Shape) -> (Vec<Vec<f32>>, Vec<Vec<f32>>) {
    let mut m = m.to_vec();
    let mut tone = m.split_off(shape.channels.iter().map(|c| c.width).sum());
    tone.truncate(tone_widths(shape).iter().sum());
    (tone, m)
}

/// While `/tone` is on, each column of an element in its targets
/// carries the leg of the same index of the generator as wide as the
/// element at unity, and no channel: the programme's output, the mono
/// aux's output, which takes the mono generator, a mix-minus output
/// and the meter columns of a bus no output carries. Every other
/// column keeps its mix. Off, or with no targets, the tone's rows are
/// 0 and the mix is as without it. The monitor's columns, its output's
/// and its meter's, which play the targeted programme, are
/// [`a_monitor_on_a_line_up_target_plays_its_tone`]'s.
#[test]
fn the_line_up_tone_replaces_the_mix_of_its_targets() {
    let shape = shape();
    assert_eq!(tone_widths(&shape), [1, 2], "aux1, then pgm, cf and mm1");
    let mut s = mixing(&shape);
    let mixed = matrix(&shape, &s);
    let (tone, channels) = tone_rows(&mixed, &shape);
    assert!(tone.iter().flatten().all(|g| *g == 0.0), "off: no tone");
    for (cols, what) in [(0..2, "pgm"), (2..3, "aux1"), (3..5, "mm1"), (8..10, "cf")] {
        assert!(
            cols.clone().all(|c| channels.iter().any(|r| r[c] > 0.1)),
            "{what} carries a mix"
        );
    }
    // The columns of the elements replaced: first column, width and
    // the first tone row of their generator.
    let replaced = |elements: &[(usize, usize, usize)], s: &Value| {
        let m = matrix(&shape, s);
        let (tone, channels) = tone_rows(&m, &shape);
        let monitor = |c: &usize| (5..7).contains(c) || (10..12).contains(c);
        for c in (0..mixed[0].len()).filter(|c| !monitor(c)) {
            let leg = (elements.iter())
                .find(|(first, width, _)| (*first..first + width).contains(&c))
                .map(|(first, _, generator)| generator + c - first);
            for (l, r) in tone.iter().enumerate() {
                let want = f32::from(u8::from(leg == Some(l)));
                assert_eq!(r[c].to_bits(), want.to_bits(), "tone leg {l} column {c}");
            }
            for (row, r) in channels.iter().enumerate() {
                let want = if leg.is_some() { 0.0 } else { mixed[row][c] };
                assert_eq!(r[c].to_bits(), want.to_bits(), "row {row} column {c}");
            }
        }
    };
    s["tone"]["targets"] = json!(["pgm", "aux1", "mm1", "cf"]);
    replaced(&[], &s);
    s["tone"]["on"] = json!(true);
    replaced(&[(0, 2, 1), (2, 1, 0), (3, 2, 1), (8, 2, 1)], &s);
    s["tone"]["targets"] = json!(["mm1"]);
    replaced(&[(3, 2, 1)], &s);
    s["tone"]["targets"] = json!([]);
    replaced(&[], &s);
}

/// The tone replaces a targeted element after its fader: the element's
/// fader, mute and polarity and the channels' faders leave its columns
/// as they are, and only its generator follows `/tone/level` and
/// `/tone/ident`, at 1 kHz.
#[test]
fn the_line_up_tone_is_after_the_elements_fader() {
    let shape = shape();
    let mut s = mixing(&shape);
    s["tone"] = json!({ "on": true, "level": -18, "ident": false, "targets": ["pgm", "mm1"] });
    let wiring = vec![
        Wiring {
            a: "a".into(),
            b: None,
            domain: "d".into(),
        };
        shape.channels.len()
    ];
    let toned = targets(&shape, &wiring, &s);
    for (path, value) in [
        ("/buses/pgm/fader", json!(-20)),
        ("/buses/pgm/mute", json!(true)),
        ("/buses/pgm/polarity", json!([true, false])),
        ("/mixMinus/mm1/fader", json!(-90)),
        ("/channels/ch1/fader", json!(-6)),
    ] {
        crate::state::set_at(&mut s, path, value);
    }
    let moved = targets(&shape, &wiring, &s);
    for c in [0, 1, 3, 4] {
        for (row, (a, b)) in toned.gains.iter().zip(&moved.gains).enumerate() {
            assert_eq!(a[c].to_bits(), b[c].to_bits(), "row {row} column {c}");
        }
    }
    assert_ne!(toned.gains, moved.gains, "the faders moved the rest");
    let slots = shape.channels.len()..shape.channels.len() + 2;
    let line_up = |level_db, ident| ToneParams {
        frequency_hz: 1000,
        level_db,
        ident,
    };
    assert_eq!(moved.tones[slots.clone()], [Some(line_up(-18.0, false)); 2]);
    crate::state::set_at(&mut s, "/tone/level", json!(-20));
    crate::state::set_at(&mut s, "/tone/ident", json!(true));
    let t = targets(&shape, &wiring, &s);
    assert_eq!(t.tones[slots.clone()], [Some(line_up(-20.0, true)); 2]);
    assert_eq!(t.gains, moved.gains, "level and ident move no gain");
    assert_eq!((t.flows.len(), t.strips.len(), t.tones.len()), (5, 5, 5));
    for slot in slots {
        assert_eq!((&t.flows[slot], t.strips[slot].len()), (&None, 0));
    }
    assert_eq!(
        (duck::masks(&shape, &s).len(), t.gains.len()),
        (8, 8),
        "the masks have the tone's rows and o-ch1's pre-strip row"
    );
}

/// A write of `/tone/level` or `/tone/ident` moves the tone's
/// generator alone; one of whether it is on or of its targets moves
/// everything.
#[test]
fn a_tone_write_moves_what_it_reads() {
    let shape = shape();
    let s = shape.initial_state();
    for path in ["/tone/level", "/tone/ident"] {
        let moved = Moved::of(&shape, &s, &Value::Null, [path]);
        assert!(moved.line_up && !moved.all, "{path}");
    }
    for path in ["/tone/on", "/tone/targets", "/tone"] {
        assert!(Moved::of(&shape, &s, &Value::Null, [path]).all, "{path}");
    }
}

/// The line-up tone has a generator per distinct width of a bus, aux
/// or mix-minus, narrowest first, and every column of a target takes
/// the legs of the one as wide as its element, leg to leg: the 5.1
/// programme's output its six legs and its stereo fold-down output
/// the first two, the stereo clean feed and mix-minus the stereo
/// generator, the mono aux the mono one. So the ident of each cuts
/// the target's own legs in turn.
#[test]
fn each_line_up_target_plays_the_generator_of_its_width() {
    let mut v = crate::shape::tests::surround();
    v["mixMinus"] =
        json!([{ "id": "mm1", "name": "N-1", "bus": "cf", "width": 2, "layout": "stereo" }]);
    let shape = Shape::parse(&v).unwrap();
    assert_eq!(tone_widths(&shape), [1, 2, 6], "aux1, cf and mm1, pgm");
    assert_eq!(tone_input(6), "/tone/6");
    let mut s = shape.initial_state();
    s["tone"]["on"] = json!(true);
    s["tone"]["targets"] = json!(["pgm", "cf", "aux1", "mm1"]);
    let m = matrix(&shape, &s);
    let (tone, _) = tone_rows(&m, &shape);
    assert_eq!(tone.len(), 1 + 2 + 6);
    // The first tone row of the generator `width` legs wide.
    let generator = |width: usize| match width {
        1 => 0,
        2 => 1,
        6 => 3,
        _ => unreachable!("{width}"),
    };
    let mut first = 0;
    let mut checked = Vec::new();
    for (id, width, _) in columns(&shape) {
        if shape.is_output_bus(id) {
            let row = generator(width_of(&shape, id));
            for leg in 0..width {
                let got: Vec<f32> = tone.iter().map(|r| r[first + leg]).collect();
                let want: Vec<f32> = (0..tone.len())
                    .map(|r| f32::from(u8::from(r == row + leg)))
                    .collect();
                assert_eq!(got, want, "{id} leg {leg}");
            }
            checked.push((id, width));
        }
        first += width;
    }
    assert_eq!(
        checked,
        [("pgm", 6), ("pgm", 2), ("cf", 2), ("aux1", 1), ("mm1", 2)]
    );
}

/// A monitor playing an element the line-up tone replaces plays what
/// the element's output carries: in place of its mix the generator
/// as wide as it, as the monitor plays a source, so its level, speaker
/// mutes and cut apply and the mono aux's generator is panned centre
/// onto both legs. Its meter columns take the generator before the
/// level, speaker mutes and cut. A source the tone does not replace
/// plays its mix,
/// and a solo the listen sum and no tone. A write of the source, a
/// solo or a monitor control derives the same entries as the whole
/// state.
#[test]
fn a_monitor_on_a_line_up_target_plays_its_tone() {
    let shape = shape();
    let mut s = mixing(&shape);
    s["monitor"]["level"] = json!(0);
    s["monitor"]["source"] = json!("pgm");
    s["tone"]["on"] = json!(true);
    s["tone"]["targets"] = json!(["pgm", "aux1"]);
    // The columns of o-mon, L and R: the tone's rows (mono, stereo L,
    // stereo R) and the channels'.
    let mon = |s: &Value| {
        let m = matrix(&shape, s);
        let (tone, channels) = tone_rows(&m, &shape);
        let lr = |r: &Vec<f32>| [r[5], r[6]];
        let tone: Vec<[f32; 2]> = tone.iter().map(lr).collect();
        (tone, channels.iter().map(lr).collect::<Vec<_>>())
    };
    // The monitor meter's columns, L and R, on the tone's rows.
    let meter = |s: &Value| -> Vec<[f32; 2]> {
        let (tone, _) = tone_rows(&matrix(&shape, s), &shape);
        tone.iter().map(|r| [r[10], r[11]]).collect()
    };
    let untoned = |s: &Value| {
        let mut s = s.clone();
        s["tone"]["on"] = json!(false);
        mon(&s).1
    };
    let (tone, channels) = mon(&s);
    assert!(channels.iter().flatten().all(|g| *g == 0.0), "{channels:?}");
    assert_eq!(tone, [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]);
    assert!(untoned(&s).iter().flatten().any(|g| *g > 0.1), "a mix");
    s["monitor"]["level"] = json!(-6);
    s["monitor"]["speakerMute"] = json!([false, true]);
    let g = gain(-6.0) as f32;
    assert_eq!(mon(&s).0, [[0.0, 0.0], [g, 0.0], [0.0, 0.0]]);
    assert_eq!(meter(&s), [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]]);
    s["monitor"]["speakerMute"] = json!([false, false]);
    s["monitor"]["cut"] = json!(true);
    assert!(mon(&s).0.iter().flatten().all(|g| *g == 0.0), "cut");
    assert_eq!(meter(&s), [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0]], "cut");
    s["monitor"]["cut"] = json!(false);
    s["monitor"]["level"] = json!(0);
    s["monitor"]["source"] = json!("aux1");
    let (tone, channels) = mon(&s);
    assert!(channels.iter().flatten().all(|g| *g == 0.0), "{channels:?}");
    let h = std::f64::consts::FRAC_1_SQRT_2;
    assert!(close(tone[0][0], h) && close(tone[0][1], h), "{tone:?}");
    assert!(tone[1..].iter().flatten().all(|g| *g == 0.0), "{tone:?}");
    s["monitor"]["source"] = json!("mm1");
    let (tone, channels) = mon(&s);
    assert!(tone.iter().flatten().all(|g| *g == 0.0), "{tone:?}");
    assert_eq!(channels, untoned(&s), "mm1's mix");
    s["monitor"]["source"] = json!("pgm");
    s["channels"]["ch1"]["solo"] = json!("pfl");
    let (tone, channels) = mon(&s);
    assert!(tone.iter().flatten().all(|g| *g == 0.0), "{tone:?}");
    assert_eq!(channels, untoned(&s), "the listen sum");
    assert!(channels.iter().flatten().any(|g| *g > 0.1), "ch1's PFL");

    let wiring = vec![
        Wiring {
            a: "a".into(),
            b: None,
            domain: "d".into(),
        };
        shape.channels.len()
    ];
    let mut d = Derived::new(&shape, &wiring, &s);
    for (path, value) in [
        ("/channels/ch1/solo", json!("off")),
        ("/monitor/source", json!("mm1")),
        ("/monitor/source", json!("pgm")),
        ("/monitor/level", json!(-12)),
        ("/monitor/source", json!("aux1")),
        ("/channels/st1/solo", json!("afl")),
    ] {
        let vcas = s["vcas"].clone();
        crate::state::set_at(&mut s, path, value);
        d.update(&shape, &wiring, &s, &vcas, [path]);
        assert_eq!(
            d.targets().gains,
            targets(&shape, &wiring, &s).gains,
            "{path}"
        );
    }
}
