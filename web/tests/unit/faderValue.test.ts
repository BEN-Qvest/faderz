// @vitest-environment happy-dom
import { describe, expect, it } from 'vitest';
import { flushSync, mount, unmount } from 'svelte';
import FaderValue from '../../src/components/FaderValue.svelte';
import { INITIAL_STATUS, MixerStore, mixerContext } from '../../src/lib/state/mixer.svelte';
import type { Owner } from '../../src/lib/contract/types';
import type { ToWorker } from '../../src/lib/sync/messages';
import { exampleShape, initialState } from '../../dev/mock/shape';

const FADER = '/channels/ch1/fader';
const SEND = '/channels/ch1/sends/aux1/level';
const LIVE = { ...INITIAL_STATUS, link: 'Live' as const };
const OTHER: Owner = { principal: 'anonymous', handle: 'other', kind: 'ui', connection: 'c2' };

function field(path = FADER) {
  const s = new MixerStore();
  const posted: ToWorker[] = [];
  s.attach({ post: (m) => posted.push(m) });
  const shape = exampleShape();
  s.apply({ type: 'patch', input: 0, snapshot: { shape, state: initialState(shape), owners: {} }, changes: [[FADER, -10], [SEND, -20]], owners: [], status: LIVE });
  const target = document.createElement('div');
  document.body.append(target);
  const c = mount(FaderValue, { target, props: { path, label: 'Pres 1 fader', testid: 'fv' }, context: mixerContext(s) });
  flushSync();
  const input = target.querySelector('input')!;
  const type = (text: string) => {
    input.focus();
    input.value = text;
    input.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
  };
  const enter = () => {
    input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
    flushSync();
  };
  const sets = () => posted.filter((m) => m.type === 'set');
  const sign = () => target.querySelector('[data-testid="fv-inf"]');
  return { s, c, input, type, enter, sets, sign };
}

describe('fader value field', () => {
  it('follows the fader while focused and untouched, and Enter then writes nothing', () => {
    const { s, c, input, enter, sets } = field();
    input.focus();
    // A surface moves the fader while the field has focus.
    s.apply({ type: 'patch', input: 0, changes: [[FADER, -5]], owners: [] });
    flushSync();
    expect(input.value).toBe('-5.0');
    enter();
    expect(sets()).toEqual([]);
    unmount(c);
  });

  it('keeps typed text over a fader move until Enter writes it, rounded to 0.1 dB', () => {
    const { s, c, input, type, enter, sets } = field();
    type('-6.54');
    s.apply({ type: 'patch', input: 0, changes: [[FADER, -5]], owners: [] });
    flushSync();
    expect(input.value).toBe('-6.54');
    enter();
    expect(sets()).toEqual([{ type: 'set', path: FADER, value: -6.5 }]);
    unmount(c);
  });

  it('refuses a level outside the range with a notice, never clamping it', () => {
    const { s, c, input, type, enter, sets } = field();
    type('12');
    enter();
    expect(sets()).toEqual([]);
    expect(input.value).toBe('-10.0');
    expect(s.notices.at(-1)?.text).toBe('Pres 1 fader takes -90 to +10 dB');
    // Refused, it follows the fader again at once, focus or not.
    s.apply({ type: 'patch', input: 0, changes: [[FADER, -5]], owners: [] });
    flushSync();
    expect(input.value).toBe('-5.0');
    unmount(c);
  });

  it('takes -inf as the floor and refuses what is no level in dB', () => {
    const { c, input, type, enter, sets } = field();
    type('40%');
    enter();
    type('4:1');
    enter();
    expect(sets()).toEqual([]);
    expect(input.value).toBe('-10.0');
    type('-inf');
    enter();
    expect(sets()).toEqual([{ type: 'set', path: FADER, value: -90 }]);
    unmount(c);
  });

  it('refuses a level for a fader another session holds', () => {
    const { s, c, input, type, enter, sets } = field();
    s.apply({ type: 'patch', input: 0, changes: [], owners: [[FADER, OTHER]] });
    flushSync();
    type('-3');
    enter();
    expect(sets()).toEqual([]);
    expect(input.value).toBe('-10.0');
    expect(s.notices.at(-1)?.text).toContain('is held by');
    s.apply({ type: 'patch', input: 0, changes: [[FADER, -5]], owners: [] });
    flushSync();
    expect(input.value).toBe('-5.0');
    unmount(c);
  });

  it('writes the path it shows, a send level in sends on faders', () => {
    const { c, input, type, enter, sets } = field(SEND);
    expect(input.value).toBe('-20.0');
    type('-12');
    enter();
    expect(sets()).toEqual([{ type: 'set', path: SEND, value: -12 }]);
    unmount(c);
  });

  it('follows the fader again when left with its text put back, which fires no change', () => {
    const { s, c, input, type, enter, sets } = field();
    type('-');
    type('-10.0');
    input.blur();
    flushSync();
    s.apply({ type: 'patch', input: 0, changes: [[FADER, -5]], owners: [] });
    flushSync();
    expect(input.value).toBe('-5.0');
    input.focus();
    enter();
    expect(sets()).toEqual([]);
    unmount(c);
  });

  it('draws the floor as a sign over its text, which stays -inf and shows while the field has focus', () => {
    const { s, c, input, sign } = field();
    expect(sign()).toBeNull();
    s.apply({ type: 'patch', input: 0, changes: [[FADER, -90]], owners: [] });
    flushSync();
    expect(sign()).not.toBeNull();
    expect(input.value).toBe('-inf');
    input.focus();
    flushSync();
    expect(sign()).toBeNull();
    unmount(c);
  });
});
