<script lang="ts">
  // A fader's value under it, which also takes a typed value: Enter or
  // leaving the field writes it, rounded to 0.1 dB; Escape restores the
  // current value. "-inf" and "off" are the floor. A value outside the
  // fader's range is never clamped: the field is restored and the range
  // shown, as in every typed field. While the field has focus the fader's
  // moves do not overwrite what is being typed.
  import { getMixer } from '../lib/state/mixer.svelte';
  import { FADER_MAX_DB, FADER_OFF_DB, formatDb } from '../lib/audio/db';
  import { parseEntry } from '../lib/audio/entry';

  interface Props {
    path: string;
    label: string;
    testid?: string;
  }
  let { path, label, testid }: Props = $props();

  const mixer = getMixer();
  const value = $derived(mixer.value<number>(path) ?? FADER_OFF_DB);
  // Centred in a field of fixed size, so no padding to a fixed width.
  const shown = $derived(formatDb(value));
  let input: HTMLInputElement | undefined = $state();
  let editing = $state(false);

  $effect(() => {
    if (input && !editing) input.value = shown;
  });

  function commit(): void {
    if (!input) return;
    const typed = parseEntry(input.value, FADER_OFF_DB);
    if (typed === null || mixer.refuseIfHeld(path)) {
      input.value = shown;
      return;
    }
    if (typed < FADER_OFF_DB || typed > FADER_MAX_DB) {
      input.value = shown;
      mixer.notify('error', `${label} takes ${FADER_OFF_DB} to +${FADER_MAX_DB} dB`, path);
      return;
    }
    const v = Number(typed.toFixed(1));
    if (v !== value) mixer.set(path, v);
    input.value = formatDb(v);
  }
  function keydown(e: KeyboardEvent): void {
    if (e.key === 'Enter') {
      commit();
      input?.select();
    } else if (e.key === 'Escape' && input) {
      input.value = shown;
      input.blur();
    }
  }
</script>

<!-- No tab stop: the fader beside it already takes the keyboard, and a
     strip keeps its five tab stops. -->
<input
  bind:this={input}
  type="text"
  inputmode="decimal"
  tabindex="-1"
  disabled={mixer.locked}
  onfocus={() => {
    editing = true;
    input?.select();
  }}
  onblur={() => (editing = false)}
  onchange={commit}
  onkeydown={keydown}
  aria-label={`${label} value, type to set`}
  title="Type a value; Enter sets it, Escape restores"
  data-path={path}
  data-testid={testid}
/>

<style>
  /* Takes the font and colour of the readout it sits in. The text changes
     with every fader move; the field has a fixed size and strict
     containment, so an update lays out nothing outside it. */
  input {
    display: block;
    width: 100%;
    height: 18px;
    box-sizing: border-box;
    contain: strict;
    margin: 0;
    padding: 0;
    border: 0;
    border-radius: inherit;
    background: transparent;
    color: inherit;
    font-size: inherit;
    line-height: inherit;
    text-align: center;
    cursor: text;
  }
  input:focus-visible,
  input:focus {
    outline: 1px solid var(--focus);
    outline-offset: 0;
  }
  input:disabled {
    cursor: default;
  }
</style>
