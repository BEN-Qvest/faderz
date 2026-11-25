//! Meter frames (server-message.schema.json `MeterFrame`), of which the
//! bridge reads the channels' peaks and the duck section.

use serde_json::Value;

const MAGIC: u8 = 0x4D;
const VERSION: u8 = 4;
const NULL: i16 = i16::MIN;

/// The frame layout of a shape.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    /// FNV-1a 32-bit of the layout text.
    pub fingerprint: u32,
    /// Values in a frame.
    values: usize,
    /// Each channel's first value and width, in shape order.
    channels: Vec<(usize, usize)>,
    /// Index of the first duck group's first value.
    ducks_at: usize,
    /// The duck group ids, in shape order.
    pub ducks: Vec<String>,
}

/// A duck group's values (model `DuckMeter`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DuckMeter {
    /// dBFS; `None` while the group is not in use.
    pub key: Option<f64>,
    /// 0 to 1.
    pub envelope: f64,
    pub engaged: bool,
}

/// What the bridge reads of a meter frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// Each channel's highest peak leg, dBFS from -120, in shape order.
    pub peaks: Vec<f64>,
    /// One reading per duck group, in shape order.
    pub ducks: Vec<DuckMeter>,
}

/// Why a frame does not match a layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mismatch {
    /// Magic, version or replica id length.
    Header,
    Fingerprint,
    Length,
}

impl Layout {
    /// The layout of model `Shape` `shape`; `None` when an element lacks
    /// its id or width.
    pub fn of(shape: &Value) -> Option<Layout> {
        let mut text = String::new();
        let mut values = 0;
        let mut channels = Vec::new();
        // Section, field, values per leg and values per element.
        for (section, field, per_leg, per_element) in [
            ("c", "channels", 2, 1),
            ("b", "buses", 3, 8),
            ("a", "auxes", 3, 8),
            ("m", "mixMinus", 3, 8),
            ("o", "monitorOutputs", 1, 0),
        ] {
            for e in shape[field].as_array().into_iter().flatten() {
                let id = e["id"].as_str()?;
                let width = usize::try_from(e["width"].as_u64()?).ok()?;
                text.push_str(&format!("{section}/{id}/{width};"));
                if section == "c" {
                    channels.push((values, width));
                }
                values += per_leg * width + per_element;
            }
        }
        let ducks_at = values;
        let mut ducks = Vec::new();
        for d in shape["ducks"].as_array().into_iter().flatten() {
            let id = d["id"].as_str()?;
            text.push_str(&format!("d/{id}/1;"));
            ducks.push(id.to_owned());
        }
        values += 3 * ducks.len();
        Some(Layout {
            fingerprint: fnv1a(&text),
            values,
            channels,
            ducks_at,
            ducks,
        })
    }

    /// What the bridge reads of `frame`: each channel's highest peak leg,
    /// dBFS from -120, and the duck section, in shape order.
    pub fn read(&self, frame: &[u8]) -> Result<Reading, Mismatch> {
        let value = self.values(frame)?;
        let peaks = (self.channels.iter())
            .map(|&(at, width)| {
                let peak = (0..width).map(|leg| value(at + leg)).max().unwrap_or(NULL);
                if peak == NULL {
                    -120.0
                } else {
                    (f64::from(peak) / 100.0).clamp(-120.0, 30.0)
                }
            })
            .collect();
        let ducks = (0..self.ducks.len())
            .map(|g| {
                let key = value(self.ducks_at + 3 * g);
                DuckMeter {
                    key: (key != NULL).then(|| (f64::from(key) / 100.0).clamp(-120.0, 30.0)),
                    envelope: (f64::from(value(self.ducks_at + 3 * g + 1)) / 10_000.0)
                        .clamp(0.0, 1.0),
                    engaged: value(self.ducks_at + 3 * g + 2) & 1 == 1,
                }
            })
            .collect();
        Ok(Reading { peaks, ducks })
    }

    /// The value at an index of `frame`, once its header, layout and
    /// length match.
    fn values<'a>(&self, frame: &'a [u8]) -> Result<impl Fn(usize) -> i16 + 'a, Mismatch> {
        if frame.len() < 12 || frame[0] != MAGIC || frame[1] != VERSION {
            return Err(Mismatch::Header);
        }
        let l = usize::from(frame[2]);
        if !(1..=32).contains(&l) {
            return Err(Mismatch::Header);
        }
        if frame[8..12] != self.fingerprint.to_le_bytes() {
            return Err(Mismatch::Fingerprint);
        }
        let start = 12 + l + l % 2;
        if frame.len() != start + 2 * self.values {
            return Err(Mismatch::Length);
        }
        Ok(move |i: usize| {
            let at = start + 2 * i;
            i16::from_le_bytes([frame[at], frame[at + 1]])
        })
    }
}

fn fnv1a(text: &str) -> u32 {
    text.bytes().fold(2_166_136_261u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(16_777_619)
    })
}

#[cfg(test)]
impl Layout {
    pub(crate) fn ducks(&self, frame: &[u8]) -> Result<Vec<DuckMeter>, Mismatch> {
        self.read(frame).map(|r| r.ducks)
    }

    pub(crate) fn channel_peaks(&self, frame: &[u8]) -> Result<Vec<f64>, Mismatch> {
        self.read(frame).map(|r| r.peaks)
    }

    /// A frame of this layout from replica `r1` whose duck groups read
    /// `ducks` (key, envelope and flags as sent), every other value 0.
    pub(crate) fn frame(&self, ducks: &[[i16; 3]]) -> Vec<u8> {
        self.frame_with(&[], ducks)
    }

    /// A frame whose channels' peak legs read `peaks` in centi-dBFS, the
    /// first channels first, and whose duck groups read `ducks`.
    pub(crate) fn frame_with(&self, peaks: &[i16], ducks: &[[i16; 3]]) -> Vec<u8> {
        let mut b = vec![MAGIC, VERSION, 2, 0];
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&self.fingerprint.to_le_bytes());
        b.extend_from_slice(b"r1");
        let mut values = vec![0i16; self.values];
        for (&(at, width), &p) in self.channels.iter().zip(peaks) {
            values[at..at + width].fill(p);
        }
        for (g, d) in ducks.iter().enumerate() {
            values[self.ducks_at + 3 * g..][..3].copy_from_slice(d);
        }
        for v in values {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    /// The shape of the contract's example frame.
    pub(crate) fn shape() -> Value {
        json!({
            "channels": [{ "id": "ch1", "name": "Mic 1", "width": 1 }],
            "buses": [{ "id": "pgm", "name": "PGM", "kind": "program", "width": 2 }],
            "auxes": [], "mixMinus": [], "monitorOutputs": [],
            "ducks": [{ "id": "comm", "name": "Commentary" }],
        })
    }

    /// The contract's example frame (server-message.schema.json
    /// `MeterFrame`).
    pub(crate) fn example() -> Vec<u8> {
        "4d 04 03 00 02 01 00 00 39 ed 5e 3d 72 65 70 00 76 fd b2 fd 30 f8 00 00 \
         41 01 32 00 20 d1 f8 f8 c6 f8 04 f7 00 80 d6 f6 d5 02 9a f7 00 80 55 00 \
         01 00 30 f8 88 13 01 00"
            .split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).expect("a hex byte"))
            .collect()
    }

    #[test]
    fn the_contracts_example_frame_reads_its_duck_group() {
        let layout = Layout::of(&shape()).expect("a layout");
        assert_eq!(layout.fingerprint, fnv1a("c/ch1/1;b/pgm/2;d/comm/1;"));
        assert_eq!(
            layout.ducks(&example()),
            Ok(vec![DuckMeter {
                key: Some(-20.0),
                envelope: 0.5,
                engaged: true
            }])
        );
    }

    #[test]
    fn the_contracts_example_frame_reads_its_channels_peak() {
        let layout = Layout::of(&shape()).expect("a layout");
        assert_eq!(layout.channel_peaks(&example()), Ok(vec![-6.5]));
        // The highest leg of a wider channel, and silence at the floor.
        let mut shape = shape();
        shape["channels"] = json!([{ "id": "st", "width": 2 }, { "id": "m", "width": 1 }]);
        let two = Layout::of(&shape).expect("a layout");
        let mut frame = two.frame_with(&[-1200, NULL], &[[0, 0, 0]]);
        // The header, the replica id, then leg 1 of the first channel.
        let first = 12 + 2 + 2;
        frame[first..first + 2].copy_from_slice(&(-300i16).to_le_bytes());
        assert_eq!(two.channel_peaks(&frame), Ok(vec![-3.0, -120.0]));
    }

    #[test]
    fn a_frame_of_another_version_layout_or_length_is_dropped() {
        let layout = Layout::of(&shape()).expect("a layout");
        let mut v3 = example();
        v3[1] = 3;
        assert_eq!(layout.ducks(&v3), Err(Mismatch::Header));
        let mut other = example();
        other[8] ^= 1;
        assert_eq!(layout.ducks(&other), Err(Mismatch::Fingerprint));
        let short = &example()[..54];
        assert_eq!(layout.ducks(short), Err(Mismatch::Length));
        let mut long = example();
        long.extend_from_slice(&[0, 0]);
        assert_eq!(layout.ducks(&long), Err(Mismatch::Length));
        // Another shape: the same frame is another layout's.
        let mut shape = shape();
        shape["ducks"] =
            json!([{ "id": "comm", "name": "Commentary" }, { "id": "b", "name": "B" }]);
        let two = Layout::of(&shape).expect("a layout");
        assert_eq!(two.ducks(&example()), Err(Mismatch::Fingerprint));
    }

    #[test]
    fn a_group_not_in_use_has_no_key_and_values_are_clamped() {
        let layout = Layout::of(&shape()).expect("a layout");
        let frame = layout.frame(&[[NULL, 12_000, 2]]);
        assert_eq!(
            layout.ducks(&frame),
            Ok(vec![DuckMeter {
                key: None,
                envelope: 1.0,
                engaged: false
            }])
        );
    }

    #[test]
    fn a_shape_element_without_a_width_has_no_layout() {
        let mut shape = shape();
        shape["buses"][0]["width"] = Value::Null;
        assert_eq!(Layout::of(&shape), None);
    }
}
