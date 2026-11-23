// @vitest-environment happy-dom
import { describe, expect, it } from 'vitest';
import { pendingWrite } from '../../src/lib/pending';
import type { MixerStore } from '../../src/lib/state/mixer.svelte';

/** A mixer whose pending keys are exactly the given paths. */
function mixer(...pending: string[]): MixerStore {
  return { pendingKey: (path: string) => pending.includes(path) } as unknown as MixerStore;
}

function attach(m: MixerStore, path: string | readonly string[], node: HTMLElement = document.createElement('button')): HTMLElement {
  pendingWrite(m, path)(node);
  return node;
}

describe('pendingWrite', () => {
  it('marks a key whose path is pending', () => {
    expect(attach(mixer('/channels/ch1/fader'), '/channels/ch1/fader').hasAttribute('data-pending')).toBe(true);
  });

  it('leaves a key unmarked while its path is not pending', () => {
    expect(attach(mixer('/channels/ch2/fader'), '/channels/ch1/fader').hasAttribute('data-pending')).toBe(false);
  });

  it('marks a key that writes several paths when any one is pending', () => {
    const paths = ['/channels/ch1/mute', '/channels/ch2/mute', '/channels/ch3/mute'];
    expect(attach(mixer('/channels/ch2/mute'), paths).hasAttribute('data-pending')).toBe(true);
    expect(attach(mixer(), paths).hasAttribute('data-pending')).toBe(false);
  });

  it('clears the mark once the path commits', () => {
    const node = attach(mixer('/channels/ch1/fader'), '/channels/ch1/fader');
    attach(mixer(), '/channels/ch1/fader', node);
    expect(node.hasAttribute('data-pending')).toBe(false);
  });
});
