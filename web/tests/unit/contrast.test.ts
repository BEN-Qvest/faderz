// The theme tokens meet WCAG 2.2 AA where an operator reads them: text
// at 4.5:1 on the surfaces it sits on, key edges at 3:1 (1.4.11).

import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { SCRIBBLE_COLORS } from '../../src/lib/contract/types';

const css = readFileSync(new URL('../../src/app.css', import.meta.url), 'utf8');

function tokens(selector: string): Record<string, string> {
  const at = css.indexOf(`${selector} {`);
  const block = css.slice(at, css.indexOf('}', at));
  return Object.fromEntries([...block.matchAll(/--([a-z0-9-]+):\s*(#[0-9a-f]{6})/g)].map((m) => [m[1]!, m[2]!]));
}

function luminance(hex: string): number {
  const c = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255).map((x) => (x <= 0.03928 ? x / 12.92 : ((x + 0.055) / 1.055) ** 2.4));
  return 0.2126 * c[0]! + 0.7152 * c[1]! + 0.0722 * c[2]!;
}

function contrast(a: string, b: string): number {
  const [x, y] = [luminance(a), luminance(b)].sort((p, q) => q - p);
  return (x! + 0.05) / (y! + 0.05);
}

const dark = tokens(':root');
const light = { ...dark, ...tokens(":root[data-theme='light']") };

/** The input rule of NumberEntry.svelte: its text colour and face. */
const entry = (() => {
  const src = readFileSync(new URL('../../src/components/NumberEntry.svelte', import.meta.url), 'utf8');
  const block = /\n  input \{([^}]*)\}/.exec(src)![1]!;
  const prop = (name: string) => new RegExp(`\\n\\s*${name}:\\s*([^;]+);`).exec(block)?.[1]?.trim();
  return { color: prop('color'), background: prop('background') };
})();

/** The colour of the first rule of DuckView.svelte with exactly this selector. */
function duckView(selector: string): string {
  const src = readFileSync(new URL('../../src/components/DuckView.svelte', import.meta.url), 'utf8');
  const at = src.indexOf(`\n  ${selector} {`);
  const block = src.slice(at, src.indexOf('}', at));
  return /\n\s*color:\s*([^;]+);/.exec(block)![1]!.trim();
}

/** A colour as written in a rule: a token of the theme or a literal. */
function resolve(value: string, t: Record<string, string>): string {
  const token = /^var\(--([a-z0-9-]+)\)$/.exec(value);
  return token ? t[token[1]!]! : value;
}

describe.each([
  ['dark', dark],
  ['light', light],
])('%s theme', (theme, t) => {
  it.each(['strip', 'strip-alt', 'panel', 'panel-2'])('faint text reads on --%s', (surface) => {
    expect(contrast(t['text-faint']!, t[surface]!)).toBeGreaterThanOrEqual(4.5);
  });
  it.each(['panel', 'panel-2', 'strip', 'bg'])('an unlit key edge shows on --%s', (surface) => {
    expect(contrast(t['key-edge']!, t[surface]!)).toBeGreaterThanOrEqual(3);
  });
  it('lit mute, PGM tally and AFL keys carry readable ink', () => {
    expect(contrast(t['mute-ink'] ?? '#ffffff', t['mute']!)).toBeGreaterThanOrEqual(4.5);
    expect(contrast('#ffffff', t['pgm']!)).toBeGreaterThanOrEqual(4.5);
    expect(contrast(t['afl-ink']!, t['afl']!)).toBeGreaterThanOrEqual(4.5);
  });
  it.each(['well-mark', 'well-dim', 'well-duck'])('a mark of --%s shows on a well (3:1)', (mark) => {
    expect(contrast(t[mark]!, t['well']!)).toBeGreaterThanOrEqual(3);
  });
  it('a lit PAUSED loudness key carries readable ink', () => {
    expect(contrast(t['paused-ink']!, t['paused']!)).toBeGreaterThanOrEqual(4.5);
  });
  it('an ownership tag carries readable ink', () => {
    expect(contrast(t['owned-ink']!, t['owned']!)).toBeGreaterThanOrEqual(4.5);
  });
  it('a lit cell of an on/off grid stands apart from an off one', () => {
    expect(contrast(t['on']!, t['switch-off']!)).toBeGreaterThanOrEqual(3);
  });
  it('a typed number entry reads on the field it sits in', () => {
    // The light theme gives every text field a white face (app.css), over the component's own.
    const field = theme === 'light' ? /:root\[data-theme='light'\] input\[type='text'\],[^{]*\{\s*background:\s*#([0-9a-f]{3})\b/.exec(css)![1]!.replace(/./g, '$&$&').replace(/^/, '#') : resolve(entry.background!, t);
    expect(contrast(resolve(entry.color!, t), field)).toBeGreaterThanOrEqual(4.5);
  });
  it.each(['thead th', '.setup th', '.idx'])('the Duck view labels of %s read on the page', (rule) => {
    // Every cell of the Duck view table sits on --bg.
    expect(contrast(resolve(duckView(rule), t), t['bg']!)).toBeGreaterThanOrEqual(4.5);
  });
  it('a lit duck key and lamp carry readable ink, and the key edge shows on the panels', () => {
    expect(contrast(t['duck-ink']!, t['duck']!)).toBeGreaterThanOrEqual(4.5);
    for (const surface of ['panel', 'panel-2']) expect(contrast(t['duck']!, t[surface]!)).toBeGreaterThanOrEqual(3);
  });
});

it('separates alternate strips in the light theme', () => {
  expect(contrast(light['strip']!, light['strip-alt']!)).toBeGreaterThanOrEqual(1.1);
});

/** color-mix(in srgb, a p%, b) as browsers compute it: per channel, gamma encoded. */
function mix(a: string, p: number, b: string): string {
  const ch = (hex: string, i: number) => parseInt(hex.slice(i, i + 2), 16);
  return `#${[1, 3, 5].map((i) => Math.round(ch(a, i) * (p / 100) + ch(b, i) * (1 - p / 100)).toString(16).padStart(2, '0')).join('')}`;
}

describe('channel colours', () => {
  const strip = readFileSync(new URL('../../src/components/ChannelStrip.svelte', import.meta.url), 'utf8');
  const fader = readFileSync(new URL('../../src/components/Fader.svelte', import.meta.url), 'utf8');
  const colours = [...strip.matchAll(/\.strip\[data-color='([a-z]+)'\] \{ --strip-color: (#[0-9a-f]{6}); \}/g)].map((m) => [m[1]!, m[2]!] as const);
  /** The text colour of the first rule of ChannelStrip.svelte with exactly this selector. */
  const ink = (selector: string) => new RegExp(`\\n  ${selector.replace('.', '\\.')} \\{[^}]*\\n    color: (#[0-9a-f]{6});`).exec(strip)![1]!;
  const inks = ['.head', '.no', '.source'].map((sel) => [sel, ink(sel)] as const);
  const headMix = Number(/var\(--strip-color, var\(--well\)\) (\d+)%, var\(--well\)/.exec(strip)![1]);
  /** --cap-tint of a theme block of app.css. */
  const capTint = (selector: string) => {
    const at = css.indexOf(`${selector} {`);
    return Number(/--cap-tint:\s*(\d+)%/.exec(css.slice(at, css.indexOf('}', at)))![1]);
  };
  const capLine = /\n  \.cap span \{[^}]*background: (#[0-9a-f]{6});/.exec(fader)![1]!;

  it('tints a fader cap by the --cap-tint of its theme', () => {
    expect(fader).toContain('color-mix(in srgb, ${tint} var(--cap-tint), var(--cap))');
  });

  it('has a tint for every colour of the strip colour picker', () => {
    expect(colours.map(([name]) => name)).toEqual(SCRIBBLE_COLORS.filter((c) => c !== 'none'));
  });

  describe.each([
    ['dark', dark, capTint(':root')],
    ['light', light, capTint(":root[data-theme='light']")],
  ])('%s theme', (_, t, tint) => {
    it.each(colours)('a %s scribble strip stays dark: its name, number and source read', (_name, colour) => {
      const head = mix(colour, headMix, t['well']!);
      for (const [, c] of inks) expect(contrast(c, head)).toBeGreaterThanOrEqual(4.5);
    });
    it('a selected strip shows its --on ring against the strip around it, tinted or not', () => {
      expect(contrast(t['on']!, t['strip-sel']!)).toBeGreaterThanOrEqual(3);
    });
    // Guards the colours, --cap and the line's ink: on these caps no share
    // of the colour would make the line less clear than on a plain one.
    it.each(colours)('a %s fader cap keeps its centre line as clear as a plain one', (_name, colour) => {
      const face = mix(colour, tint, t['cap']!);
      expect(contrast(capLine, face)).toBeGreaterThanOrEqual(Math.min(3, contrast(capLine, t['cap']!)));
    });
  });
});
