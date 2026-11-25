<script lang="ts">
  // A fader's value under it, which takes a typed level in dB like every typed field.
  import { getMixer } from '../lib/state/mixer.svelte';
  import { FADER_MAX_DB, FADER_OFF_DB, formatDb, roundDb } from '../lib/audio/db';
  import { parseEntry } from '../lib/audio/entry';

  interface Props {
    path: string;
    label: string;
    testid?: string;
  }
  let { path, label, testid }: Props = $props();

  const mixer = getMixer();
  const value = $derived(mixer.value<number>(path) ?? FADER_OFF_DB);
  const shown = $derived(formatDb(value));
  /** A level in dB, or the floor; no percent, ratio or kilo as other fields take. */
  const DB_TEXT = /^(-inf|off|[-+]?\d*[.,]?\d+\s*(db)?)$/i;
  let input: HTMLInputElement | undefined = $state();
  // Set by typing: until then the field follows the fader, focused or not.
  let dirty = $state(false);

  $effect(() => {
    if (input && !dirty) input.value = shown;
  });

  function restore(): void {
    dirty = false;
    if (input) input.value = shown;
  }
  function commit(): void {
    if (!input || !dirty) return;
    const text = input.value.trim();
    const typed = DB_TEXT.test(text) ? parseEntry(text, FADER_OFF_DB) : null;
    if (typed === null || mixer.refuseIfHeld(path)) return restore();
    if (typed < FADER_OFF_DB || typed > FADER_MAX_DB) {
      restore();
      mixer.notify('error', `${label} takes ${FADER_OFF_DB} to +${FADER_MAX_DB} dB`, path);
      return;
    }
    const v = roundDb(typed);
    if (v !== value) mixer.set(path, v);
    dirty = false;
    input.value = formatDb(v);
  }
  function keydown(e: KeyboardEvent): void {
    if (e.key === 'Enter') {
      commit();
      input?.select();
    } else if (e.key === 'Escape') {
      restore();
      input?.blur();
    }
  }
</script>

<!-- No tab stop: the fader takes the keyboard, and a strip keeps its five tab stops. -->
<div class="num value">
  <input
    bind:this={input}
    type="text"
    tabindex="-1"
    disabled={mixer.locked}
    oninput={() => (dirty = true)}
    onfocus={() => input?.select()}
    onblur={() => {
      // change fires first when the text changed; a dirty field left unchanged follows the fader again.
      if (dirty) restore();
    }}
    onchange={commit}
    onkeydown={keydown}
    aria-label={`${label} value, type to set`}
    title="Type a value; Enter sets it, Escape restores"
    data-path={path}
    data-testid={testid}
  />
</div>

<style>
  .value {
    font-size: 11.5px;
    background: var(--well);
    border-radius: 2px;
    color: #e6edf3;
    line-height: 18px;
  }
  /* Fixed size and strict containment: a new value lays out nothing outside the field. */
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
    font: inherit;
    text-align: center;
    cursor: text;
  }
  input:focus {
    outline: 1px solid var(--focus);
    outline-offset: 0;
  }
  input:disabled {
    cursor: default;
  }
  /* A finger's target on touch screens (WCAG 2.5.8), as the pan's. */
  @media (pointer: coarse) {
    input {
      height: 24px;
    }
  }
</style>
