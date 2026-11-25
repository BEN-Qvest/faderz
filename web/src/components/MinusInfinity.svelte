<script lang="ts">
  // A drawn minus-infinity sign: the console's fonts carry no infinity glyph.
  let { testid }: { testid?: string } = $props();
</script>

<svg viewBox="0 0 22 10" aria-hidden="true" data-testid={testid}>
  <path d="M1 5h5" />
  <path d="M15 5c-1.5-2-6-3.2-6 0s4.5 2 6 0 6-3.2 6 0-4.5 2-6 0z" />
</svg>

<style>
  /* A box 1.5 times a digit's height, in the text colour, strokes as thick as the digits'. */
  svg {
    display: block;
    width: 2.3em;
    height: 1.05em;
    fill: none;
    stroke: currentColor;
    stroke-width: 0.85;
    stroke-linecap: round;
  }
</style>
