import { describe, expect, it } from 'vitest';
import {
  GR_WIDTH,
  LAMP_WIDTH,
  channelReduction,
  deepestDepth,
  lampLit,
  lampText,
  liveDuck,
  reductionText,
} from '../../src/lib/duck';
import { deriveMeterLayout, decodeMeterFrame, encodeMeterFrame } from '../../src/lib/contract/meterFrame';
import type { DuckMeter, DuckState } from '../../src/lib/contract/types';
import { flattenMeterFrame } from '../../src/lib/meters/flat';
import { STALE_MS } from '../../src/lib/meters/layer';
import { MeterStore } from '../../src/lib/meters/store';
import { ReplicaMeters, SourceModel, STEADY_KEY_DB, stepDetector, type Detector } from '../../dev/mock/signals';
import { exampleShape, initialState } from '../../dev/mock/shape';

const shape = exampleShape();
/** comm lowers ch13 to ch16 by 10 dB and st1 by 15 dB in the example state. */
const comm = (patch: Partial<DuckState> = {}): DuckState => ({ ...initialState(shape).ducks['comm']!, ...patch });
const reading = (envelope: number, engaged = false, key: number | null = -20): DuckMeter => ({ key, envelope, engaged });

describe('duck lamp text', () => {
  it('reads the state alone without readings', () => {
    expect(lampText(comm())).toBe('AUTO');
    expect(lampText(comm({ hold: true }))).toBe('DUCK HELD');
    expect(lampText(comm({ mode: 'on' }))).toBe('DUCK ON');
    expect(lampText(comm({ mode: 'off' }))).toBe('BYPASS');
  });

  it('reads how far the deepest target is lowered while the envelope is above 0, whatever the state', () => {
    expect(deepestDepth(comm())).toBe(15);
    expect(lampText(comm(), reading(1, true))).toBe('DUCK -15.0');
    expect(lampText(comm({ hold: true }), reading(0.5))).toBe('DUCK -7.5');
    expect(lampText(comm({ mode: 'on' }), reading(0.123))).toBe('DUCK -1.8');
    expect(lampText(comm(), reading(0.0001))).toBe('DUCK -0.0');
  });

  it('says KEY while the key triggers a group that is not ducking', () => {
    expect(lampText(comm(), reading(0, true))).toBe('KEY');
    expect(lampText(comm({ mode: 'off' }), reading(0, true))).toBe('BYPASS KEY');
    expect(lampText(comm(), reading(0, false))).toBe('AUTO');
    expect(lampText(comm({ hold: true }), reading(0, true))).toBe('DUCK HELD');
  });

  it('fits LAMP_WIDTH at the deepest depth, so the lamp never truncates', () => {
    const deep = comm();
    deep.channels['ch13'] = { trigger: false, target: true, depth: 40 };
    expect(lampText(deep, reading(1))).toBe('DUCK -40.0');
    for (const text of ['DUCK -40.0', 'DUCK HELD', 'DUCK ON', 'BYPASS KEY', 'BYPASS', 'KEY', 'AUTO']) {
      expect(text.length).toBeLessThanOrEqual(LAMP_WIDTH);
    }
  });

  it('is lit while forced in or while the envelope is above 0', () => {
    expect(lampLit(comm(), null)).toBe(false);
    expect(lampLit(comm(), reading(0, true))).toBe(false);
    expect(lampLit(comm(), reading(0.01))).toBe(true);
    expect(lampLit(comm({ hold: true }), null)).toBe(true);
    expect(lampLit(comm({ mode: 'on' }), reading(0))).toBe(true);
  });

  it('has no depth without a target', () => {
    const none = comm();
    for (const m of Object.values(none.channels)) m.target = false;
    expect(deepestDepth(none)).toBe(0);
  });
});

describe('strip gain reduction', () => {
  const state = initialState(shape);
  const groups = shape.ducks!;

  it('is the deepest of depth times envelope over the groups that lower the channel', () => {
    // ch13: comm 10 dB, remote 8 dB.
    const live = (env: Record<string, number>) => (id: string) => (id in env ? reading(env[id]!) : null);
    expect(channelReduction(state, groups, 'ch13', live({ comm: 0.5, remote: 1 }))).toBe(8);
    expect(channelReduction(state, groups, 'ch13', live({ comm: 1, remote: 1 }))).toBe(10);
    expect(channelReduction(state, groups, 'ch13', live({}))).toBe(0);
    expect(channelReduction(state, [], 'ch13', live({ comm: 1 }))).toBe(0);
  });

  it('reads in GR_WIDTH characters, one decimal below 10 dB, whole dB from there', () => {
    expect(reductionText(0.04)).toBe('-0.0');
    expect(reductionText(3.46)).toBe('-3.5');
    expect(reductionText(9.94)).toBe('-9.9');
    expect(reductionText(9.96)).toBe(' -10');
    expect(reductionText(12.4)).toBe(' -12');
    expect(reductionText(40)).toBe(' -40');
    for (const db of [0, 0.05, 5, 9.949, 9.95, 10, 25.5, 40]) expect(reductionText(db)).toHaveLength(GR_WIDTH);
  });
});

describe('duck readings in the meter store', () => {
  const layout = deriveMeterLayout(shape);
  const store = new MeterStore();
  store.setShape(shape);
  const frame = (ducks: DuckMeter[]) => {
    const r = new ReplicaMeters(shape, 'steady').frame(initialState(shape), new SourceModel(shape));
    return decodeMeterFrame(layout, encodeMeterFrame(layout, { sequence: 1, replica: 'r1' }, { ...r, ducks }));
  };

  it('reads each group from the latest frame while it is fresh, and nothing once stale', () => {
    const f = frame([reading(0.5, true, -20), reading(0, false, null)]);
    store.ingest('r1', flattenMeterFrame(layout, f), 1000);
    expect(liveDuck(store, 'comm', 1000 + STALE_MS - 1)).toEqual({ key: -20, envelope: 0.5, engaged: true });
    expect(liveDuck(store, 'remote', 1000)).toEqual({ key: null, envelope: 0, engaged: false });
    expect(liveDuck(store, 'comm', 1000 + STALE_MS)).toBeNull();
    expect(liveDuck(store, 'nope', 1000)).toBeNull();
    store.clear();
    expect(liveDuck(store, 'comm', 1000)).toBeNull();
  });
});

describe('mock duck detector', () => {
  const step = (det: Detector, d: DuckState, key: number, frames = 1) => {
    for (let i = 0; i < frames; i++) stepDetector(det, d, key);
    return det;
  };
  const fresh = (): Detector => ({ triggered: false, holdLeft: 0, envelope: 0 });

  it('triggers above the threshold and ramps the envelope up over attackMs', () => {
    // threshold -30, attack 20 ms: one 40 ms period reaches full depth.
    const det = step(fresh(), comm(), -20);
    expect(det.triggered).toBe(true);
    expect(det.envelope).toBe(1);
  });

  it('stays triggered within the hysteresis, holds for holdMs after it, then releases over releaseMs', () => {
    const d = comm({ attackMs: 20, holdMs: 500, releaseMs: 1000, hysteresis: 4 });
    const det = step(fresh(), d, -20);
    step(det, d, -32, 5);
    expect(det.triggered).toBe(true);
    step(det, d, -40);
    expect(det.triggered).toBe(false);
    // Holding: still engaged without the key, so still at full depth.
    expect(det.holdLeft).toBe(460);
    step(det, d, -40, 11);
    expect(det.envelope).toBe(1);
    step(det, d, -40);
    expect(det.holdLeft).toBe(0);
    step(det, d, -40, 30);
    expect(det.envelope).toBe(0);
  });

  it('ducks while held or on whatever the key, and never in bypass from the key alone', () => {
    expect(step(fresh(), comm({ hold: true }), STEADY_KEY_DB).envelope).toBe(1);
    expect(step(fresh(), comm({ mode: 'on' }), STEADY_KEY_DB).envelope).toBe(1);
    const off = step(fresh(), comm({ mode: 'off' }), -10);
    expect(off.triggered).toBe(true);
    expect(off.envelope).toBe(0);
  });

  it('reports a steady key below threshold for a group in use and null for one without a target', () => {
    const state = initialState(shape);
    const meters = new ReplicaMeters(shape, 'steady');
    const sources = new SourceModel(shape);
    expect(meters.frame(state, sources).ducks[0]).toEqual({ key: STEADY_KEY_DB, envelope: 0, engaged: false });
    for (const m of Object.values(state.ducks['remote']!.channels)) m.target = false;
    expect(meters.frame(state, sources).ducks[1]).toEqual({ key: null, envelope: 0, engaged: false });
    state.ducks['comm']!.hold = true;
    expect(meters.frame(state, sources).ducks[0]).toEqual({ key: STEADY_KEY_DB, envelope: 1, engaged: false });
  });

  it('keys from its triggers: speech above threshold ducks it, silence lets it go', () => {
    const state = initialState(shape);
    const meters = new ReplicaMeters(shape, 'triggers');
    const sources = new SourceModel(shape);
    for (const c of shape.channels) sources.levels.set(c.id, Array<number>(c.width).fill(-70));
    sources.levels.set('ch1', [-20]);
    const loud = meters.frame(state, sources).ducks[0]!;
    expect(loud.key).toBeGreaterThan(state.ducks['comm']!.threshold);
    expect(loud).toMatchObject({ envelope: 1, engaged: true });
    sources.levels.set('ch1', [-70]);
    let quiet = loud;
    for (let i = 0; i < 50; i++) quiet = meters.frame(state, sources).ducks[0]!;
    expect(quiet).toMatchObject({ envelope: 0, engaged: false });
    // A closed trigger keys nothing at preFaderOpen.
    sources.levels.set('ch1', [-20]);
    state.channels['ch1']!.mute = true;
    expect(meters.frame(state, sources).ducks[0]!.engaged).toBe(false);
  });
});
