use super::*;
use crate::shape::tests::small;
use serde_json::Value;

/// The client derives the same fingerprint from the shape
/// (web/src/lib/contract/meterFrame.ts) and refuses a frame of another.
#[test]
fn the_fingerprint_is_the_clients() {
    let layout = Layout::of(&Shape::parse(&small()).unwrap());
    let text = "c/ch1/1;c/ch2/1;c/st1/2;b/pgm/2;b/cf/2;a/aux1/1;m/mm1/2;o/mon/2;d/comm/1;";
    assert_eq!(layout.fingerprint, fnv1a(text));
    assert_eq!(fnv1a("a"), 0xe40c_292c);
}

#[test]
fn a_loudness_meter_is_its_output_or_its_engine_meter_after_them() {
    let layout = Layout::of(&Shape::parse(&small()).unwrap());
    // Outputs: out-pgm. Meters: cf, aux1, mm1, mon.
    assert_eq!(layout.loudness_meter("pgm"), Some(0));
    assert_eq!(layout.loudness_meter("cf"), Some(1));
    assert_eq!(layout.loudness_meter("aux1"), Some(2));
    assert_eq!(layout.loudness_meter("mm1"), Some(3));
    assert_eq!(layout.loudness_meter("ch1"), None);
    assert_eq!(layout.loudness_meter("nope"), None);
}

/// An element's offset is the values of every element before it, so
/// it moves when an element is added ahead of it.
#[test]
fn an_offset_counts_the_values_of_the_elements_before() {
    let layout = Layout::of(&Shape::parse(&small()).unwrap());
    // ch1 3, ch2 3, st1 5, pgm 14, cf 14, aux1 11, mm1 14.
    assert_eq!(layout.offset("ch1"), Some(0));
    assert_eq!(layout.offset("ch2"), Some(3));
    assert_eq!(layout.offset("pgm"), Some(11));
    assert_eq!(layout.offset("mon"), Some(11 + 14 + 14 + 11 + 14));
    assert_eq!(layout.offset("nope"), None);
    let mut with_a_bus = small();
    with_a_bus["buses"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({ "id": "b2", "name": "B2", "kind": "cleanFeed", "width": 2, "layout": "stereo" }));
    let more = Layout::of(&Shape::parse(&with_a_bus).unwrap());
    assert_eq!(more.offset("mon"), Some(64 + 14));
}

/// An element is metered by the output that carries it at its full
/// width; one a narrower output alone carries has an engine meter.
#[test]
fn a_fold_down_output_is_not_its_sources_meter() {
    let mut v = crate::shape::tests::surround();
    let source = |v: &Value| {
        let layout = Layout::of(&Shape::parse(v).unwrap());
        layout
            .elements
            .iter()
            .find(|e| e.1 == 6 && e.0 == Kind::Bus)
            .unwrap()
            .2
    };
    assert!(matches!(source(&v), Source::Output(0)));
    v["outputs"].as_array_mut().unwrap().remove(0);
    assert!(matches!(source(&v), Source::Meter(0)));
}

/// A monitor output reads its engine meter, the monitor before its
/// level, also while a full-width output carries it and meters it
/// after: the frame takes the engine meter's peak and not the
/// output's, and the monitor keeps its place and width in the frame.
#[test]
fn a_monitor_reads_its_engine_meter_while_an_output_carries_it() {
    let mut v = small();
    v["outputs"].as_array_mut().unwrap().push(serde_json::json!(
        { "id": "out-mon", "name": "Mon", "source": "mon", "width": 2, "layout": "stereo" }
    ));
    let shape = Shape::parse(&v).unwrap();
    let layout = Layout::of(&shape);
    let uncarried = Layout::of(&Shape::parse(&small()).unwrap());
    let ids: Vec<&str> = shape.meter_columns().iter().map(|m| m.0).collect();
    assert_eq!(ids, ["cf", "aux1", "mm1", "mon"]);
    let mon = (layout.elements.iter())
        .find(|e| e.0 == Kind::Monitor)
        .unwrap()
        .2;
    assert!(matches!(mon, Source::Meter(3)));
    assert_eq!(layout.offset("mon"), uncarried.offset("mon"));
    assert_eq!(layout.fingerprint, uncarried.fingerprint);
    // out-mon at -12 dB after a monitor level of -6, its engine
    // meter at -6.
    let out_mon = OutputMeters {
        peak: vec![0.25, 0.25],
        ..Default::default()
    };
    let mon_meter = OutputMeters {
        peak: vec![0.5, 0.5],
        ..Default::default()
    };
    let none = OutputMeters::default();
    let silent = InputMeters::default();
    let r = Readings {
        outputs: vec![&none, &out_mon],
        meters: vec![&none, &none, &none, &mon_meter],
        inputs: vec![&silent; 3],
        pre_strip: vec![None; 3],
        ducks: &[],
        taps: &[TAP; 3],
    };
    let f = layout.frame("r1", 0, &r);
    let v = |i: usize| i16::from_le_bytes([f[14 + 2 * i], f[15 + 2 * i]]);
    let at = layout.offset("mon").unwrap();
    assert_eq!([v(at), v(at + 1)], [-602, -602]);
}

fn input(peak: Vec<f32>, momentary: Option<f64>) -> InputMeters {
    InputMeters {
        window: Some(10),
        peak,
        rms: Vec::new(),
        momentary,
        true_peak: Vec::new(),
        max_true_peak: Vec::new(),
    }
}

const TAP: MeterTap = MeterTap {
    meter: 1.0,
    pre_strip: false,
};

fn tap(meter: f64) -> MeterTap {
    MeterTap { meter, ..TAP }
}

/// [`input`] with a true peak.
fn input_tp(peak: Vec<f32>, true_peak: Vec<f32>, momentary: Option<f64>) -> InputMeters {
    InputMeters {
        true_peak,
        ..input(peak, momentary)
    }
}

fn lin(db: f64) -> f32 {
    10f64.powf(db / 20.0) as f32
}

/// Header, padding and value count as the client decodes them. Every
/// element is metered whether an output carries it or not: channels
/// by their input meters, pgm by its output, the other buses, the
/// aux, the mix-minus and the monitor by engine meters of their own.
/// Silence reads the floor, a value not measured null.
#[test]
fn a_frame_has_the_clients_length_and_values() {
    let shape = Shape::parse(&small()).unwrap();
    let layout = Layout::of(&shape);
    let pgm = OutputMeters {
        peak: vec![0.5, 0.0],
        true_peak: vec![0.5, 0.0],
        rms: vec![0.5, 0.0],
        momentary: Some(-23.0),
        correlation: Some(-0.85),
        ..Default::default()
    };
    let cf = OutputMeters {
        peak: vec![0.25, 0.25],
        ..Default::default()
    };
    let mon = OutputMeters {
        peak: vec![0.5, 0.5],
        ..Default::default()
    };
    let none = OutputMeters::default();
    let ch1 = input_tp(vec![0.25], vec![0.5], Some(-20.0));
    let ch2 = InputMeters::default();
    let st1 = input(vec![0.0, 0.0], None);
    let r = Readings {
        outputs: vec![&pgm],
        meters: vec![&cf, &none, &none, &mon],
        inputs: vec![&ch1, &ch2, &st1],
        pre_strip: vec![None; 3],
        ducks: &[DuckMeter {
            slice: Some(3),
            key: 0.5,
            envelope: 0.25,
            engaged: true,
        }],
        taps: &[TAP; 3],
    };
    let f = layout.frame("r1", 7, &r);
    // Channels 2*legs+1, buses 3*legs+8, monitor legs, ducks 3.
    let values = 3 + 3 + 5 + (6 + 8) * 2 + (3 + 8) + (6 + 8) + 2 + 3;
    assert_eq!(f.len(), 12 + 2 + 2 * values);
    assert_eq!(&f[..4], &[MAGIC, VERSION, 2, 0]);
    assert_eq!(u32::from_le_bytes(f[4..8].try_into().unwrap()), 7);
    let v = |i: usize| i16::from_le_bytes([f[14 + 2 * i], f[15 + 2 * i]]);
    // ch1 at its input meter, its true peak too: it has no direct out.
    assert_eq!([v(0), v(1), v(2)], [-1204, -602, -2000]);
    // ch2 not measured yet.
    assert_eq!([v(3), v(4), v(5)], [NULL, NULL, NULL]);
    // st1 silent.
    assert_eq!(
        [v(6), v(7), v(8), v(9), v(10)],
        [FLOOR, FLOOR, NULL, NULL, NULL]
    );
    // pgm, from its output: peak L -6.02 dB, R silent; its
    // correlation, the 13th value after its legs.
    assert_eq!([v(11), v(12), v(17), v(23)], [-602, FLOOR, -2300, -85]);
    // cf, from its engine meter, which measured no correlation.
    assert_eq!([v(25), v(26), v(37)], [-1204, -1204, NULL]);
    // The monitor, then the duck group: key -6.02 dBFS, envelope a
    // quarter, engaged.
    assert_eq!([v(values - 5), v(values - 4)], [-602, -602]);
    assert_eq!(
        [v(values - 3), v(values - 2), v(values - 1)],
        [-602, 2500, 1]
    );
}

/// A duck group not in use reads a null key, no envelope and no flag;
/// a silent key the floor; the envelope rounds to ten-thousandths and
/// stays within 0 to 10000.
#[test]
fn a_duck_group_reads_its_engine_meter() {
    let m = |slice, key, envelope, engaged| DuckMeter {
        slice,
        key,
        envelope,
        engaged,
    };
    assert_eq!(duck(None), [NULL, 0, 0]);
    assert_eq!(duck(Some(&m(None, 0.5, 0.5, true))), [NULL, 0, 0]);
    assert_eq!(duck(Some(&m(Some(0), 0.0, 0.0, false))), [FLOOR, 0, 0]);
    assert_eq!(duck(Some(&m(Some(9), 1.0, 1.0, true))), [0, 10_000, 1]);
    assert_eq!(duck(Some(&m(Some(9), 1.0, 0.123_45, false))), [0, 1235, 0]);
    assert_eq!(duck(Some(&m(Some(9), 1.0, 1.5, false)))[1], 10_000);
}

/// A channel's meter follows its meterTap: the gain of a post-fader
/// tap scales the levels and the momentary loudness by the power; a
/// closed fader reads silence in both, and an input not measured yet
/// reads no loudness whatever its tap.
#[test]
fn a_channel_meter_takes_its_tap_gain() {
    let shape = Shape::parse(&small()).unwrap();
    let layout = Layout::of(&shape);
    let none = OutputMeters::default();
    let ch1 = input(vec![0.5], Some(-20.0));
    let st1 = input(vec![0.5, 0.25], Some(-20.0));
    let r = Readings {
        outputs: vec![&none],
        meters: vec![&none; 4],
        inputs: vec![&ch1, &ch1, &st1],
        pre_strip: vec![None; 3],
        ducks: &[],
        taps: &[tap(0.5), tap(0.0), TAP],
    };
    let f = layout.frame("r1", 0, &r);
    let v = |i: usize| i16::from_le_bytes([f[14 + 2 * i], f[15 + 2 * i]]);
    assert_eq!([v(0), v(2)], [-1204, -2602]);
    assert_eq!([v(3), v(5)], [FLOOR, FLOOR]);
    // An input not measured yet has no loudness, closed or not.
    let unmeasured = input(vec![0.5], None);
    let r = Readings {
        inputs: vec![&unmeasured, &unmeasured, &st1],
        ..r
    };
    let f = layout.frame("r1", 0, &r);
    let v = |i: usize| i16::from_le_bytes([f[14 + 2 * i], f[15 + 2 * i]]);
    assert_eq!([v(2), v(5)], [NULL, NULL]);
    assert_eq!([v(6), v(7), v(10)], [-602, -1204, -2000]);
}

/// A channel's true peak is its input meter's, taken to the meter tap
/// as its peak is: scaled by a post-fader tap's gain, the floor on a
/// closed one. A direct out's meter does not enter it, and a channel
/// without one has it too; an input with no true peak reads null.
#[test]
fn a_channel_true_peak_is_its_inputs_at_its_meter_tap() {
    let mut v = crate::shape::tests::small();
    v["outputs"] = serde_json::json!([
        { "id": "do", "name": "d", "source": "ch1", "width": 1, "layout": "mono" },
        { "id": "out", "name": "o", "source": "pgm", "width": 2, "layout": "stereo" },
    ]);
    let layout = Layout::of(&Shape::parse(&v).unwrap());
    let ch = input_tp(vec![0.25], vec![0.5], Some(-20.0));
    let st1 = input_tp(vec![0.25, 0.25], vec![0.5, 0.25], Some(-20.0));
    let direct = OutputMeters {
        true_peak: vec![1.0],
        ..Default::default()
    };
    let none = OutputMeters::default();
    let frame = |t: MeterTap, ch2: &InputMeters| {
        let r = Readings {
            outputs: vec![&direct, &none],
            meters: vec![&none; 4],
            inputs: vec![&ch, ch2, &st1],
            pre_strip: vec![None; 3],
            ducks: &[],
            taps: &[t, t, t],
        };
        let f = layout.frame("r1", 0, &r);
        let v = |i: usize| i16::from_le_bytes([f[14 + 2 * i], f[15 + 2 * i]]);
        // ch1 with its direct out, ch2 and st1 without one.
        [v(1), v(4), v(8), v(9)]
    };
    assert_eq!(frame(TAP, &ch), [-602, -602, -602, -1204]);
    // A post-fader meter at -6 dB.
    assert_eq!(frame(tap(0.5), &ch), [-1204, -1204, -1204, -1806]);
    // A closed fader.
    assert_eq!(frame(tap(0.0), &ch), [FLOOR, FLOOR, FLOOR, FLOOR]);
    // An input whose engine meters no true peak.
    let without = input(vec![0.25], Some(-20.0));
    assert_eq!(frame(TAP, &without)[1], NULL);
}

/// A channel at its input tap reads its input's meter before the
/// strip, peak, true peak and momentary loudness, at unity whatever
/// its fader; at its pre-fader tap the meter after the strip. An
/// input tap on an input with no meter before its strip reads null,
/// not the meter after it.
#[test]
fn an_input_meter_tap_reads_the_input_before_its_strip() {
    let layout = Layout::of(&Shape::parse(&small()).unwrap());
    let none = OutputMeters::default();
    let after = input_tp(vec![lin(-12.0)], vec![lin(-11.0)], Some(-30.0));
    let before = input_tp(vec![lin(-6.0)], vec![lin(-5.0)], Some(-24.0));
    let st1 = input(vec![0.5, 0.5], Some(-20.0));
    let input_tap = MeterTap {
        meter: 1.0,
        pre_strip: true,
    };
    let frame = |taps: &[MeterTap], pre: Option<&InputMeters>| {
        let r = Readings {
            outputs: vec![&none],
            meters: vec![&none; 4],
            inputs: vec![&after, &after, &st1],
            pre_strip: vec![pre, pre, None],
            ducks: &[],
            taps,
        };
        let f = layout.frame("r1", 0, &r);
        let v = |i: usize| i16::from_le_bytes([f[14 + 2 * i], f[15 + 2 * i]]);
        [v(0), v(1), v(2), v(3), v(4), v(5)]
    };
    assert_eq!(
        frame(&[input_tap, TAP, TAP], Some(&before)),
        [-600, -500, -2400, -1200, -1100, -3000]
    );
    assert_eq!(
        frame(&[input_tap, TAP, TAP], None),
        [NULL, NULL, NULL, -1200, -1100, -3000]
    );
}

/// A bus's correlation in hundredths, rounded half up and within -1
/// to 1 (-100..100); null where the engine measured none.
#[test]
fn a_bus_correlation_is_its_meters_in_hundredths() {
    let layout = Layout::of(&Shape::parse(&small()).unwrap());
    let none = OutputMeters::default();
    let ins = InputMeters::default();
    let corr = |c: Option<f64>| {
        let pgm = OutputMeters {
            correlation: c,
            ..Default::default()
        };
        let r = Readings {
            outputs: vec![&pgm],
            meters: vec![&none; 4],
            inputs: vec![&ins; 3],
            pre_strip: vec![None; 3],
            ducks: &[],
            taps: &[TAP; 3],
        };
        let f = layout.frame("r1", 0, &r);
        let at = layout.offset("pgm").unwrap() + 3 * 2 + 6;
        i16::from_le_bytes([f[14 + 2 * at], f[15 + 2 * at]])
    };
    assert_eq!(corr(Some(0.85)), 85);
    assert_eq!(corr(Some(1.0)), 100);
    assert_eq!(corr(Some(-1.0)), -100);
    assert_eq!(corr(Some(0.0)), 0);
    assert_eq!(corr(Some(0.004_9)), 0);
    assert_eq!(corr(Some(-0.004)), 0);
    assert_eq!(corr(Some(-0.015_1)), -2);
    assert_eq!(corr(None), NULL);
}

/// The example frame of server-message.schema.json `MeterFrame`,
/// encoded from engine meters holding its values. ch1's true peak is
/// its input's, whatever its direct out reads.
#[test]
fn the_contracts_example_frame() {
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../../api/ws/server-message.schema.json")).unwrap();
    let hex = schema["$defs"]["MeterFrame"]["examples"][0]
        .as_str()
        .unwrap();
    let want: Vec<u8> = hex
        .split(' ')
        .map(|b| u8::from_str_radix(b, 16).unwrap())
        .collect();

    let shape: Shape = serde_json::from_value(serde_json::json!({
        "sampleRate": 48000,
        "frameRate": { "numerator": 25, "denominator": 1 },
        "channels": [{ "id": "ch1", "name": "c", "width": 1, "layout": "mono", "altInput": false }],
        "buses": [{ "id": "pgm", "name": "p", "kind": "program", "width": 2, "layout": "stereo" }],
        "auxes": [], "mixMinus": [], "vcas": [], "monitorOutputs": [],
        "ducks": [{ "id": "comm", "name": "c" }],
        "outputs": [
            { "id": "do", "name": "d", "source": "ch1", "width": 1, "layout": "mono" },
            { "id": "out", "name": "o", "source": "pgm", "width": 2, "layout": "stereo" },
        ],
    }))
    .unwrap();
    let ch1 = input_tp(vec![lin(-6.5)], vec![lin(-5.9)], Some(-20.0));
    let direct = OutputMeters {
        true_peak: vec![lin(-1.0)],
        ..Default::default()
    };
    let pgm = OutputMeters {
        window: Some(10),
        since: Some(0),
        peak: vec![lin(0.0), lin(3.21)],
        true_peak: vec![lin(0.5), 0.0],
        rms: vec![lin(-18.0), lin(-18.5)],
        momentary: Some(-23.0),
        integrated: Some(-23.46),
        range: Some(7.25),
        max_momentary: Some(-21.5),
        correlation: Some(0.85),
        ..Default::default()
    };
    let r = Readings {
        outputs: vec![&direct, &pgm],
        meters: Vec::new(),
        inputs: vec![&ch1],
        pre_strip: vec![None; 1],
        ducks: &[DuckMeter {
            slice: Some(0),
            key: lin(-20.0),
            envelope: 0.5,
            engaged: true,
        }],
        taps: &[TAP],
    };
    let got = Layout::of(&shape).frame("rep", 258, &r);
    assert_eq!(got, want);
    // The same programme with gaps and a slice lost: samplez measures
    // across them, and the frame carries the integrated loudness and
    // its valid flag as before.
    let gapped = OutputMeters {
        gaps: 12,
        lost: 3,
        ..pgm.clone()
    };
    let r = Readings {
        outputs: vec![&direct, &gapped],
        ..r
    };
    assert_eq!(Layout::of(&shape).frame("rep", 258, &r), want);
}

/// Loudness: integratedValid while an integrated loudness is measured,
/// across gaps and lost slices, which never blank it; lraStable from
/// 60 s of measurement on.
#[test]
fn the_loudness_flags_follow_the_measurement_not_its_gaps() {
    let m = |gaps, lost, window| OutputMeters {
        gaps,
        lost,
        window: Some(window),
        since: Some(4_800),
        integrated: Some(-23.0),
        range: Some(5.0),
        ..Default::default()
    };
    assert_eq!(flags(&m(0, 0, 10)), 1);
    assert_eq!(flags(&m(0, 0, 599)), 1);
    assert_eq!(flags(&m(0, 0, 600)), 3);
    assert_eq!(flags(&m(1, 0, 600)), 3, "a gap");
    assert_eq!(flags(&m(0, 1, 10)), 1, "a slice lost");
    assert_eq!(flags(&OutputMeters::default()), 0, "nothing measured");
}
