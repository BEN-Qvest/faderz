import { describe, expect, it } from 'vitest';
import { isAltZero } from '../../src/lib/keys';

describe('Alt+0', () => {
  it('is the 0 key with Alt, whatever character the layout gives it', () => {
    // macOS: Option turns the 0 key into another character.
    expect(isAltZero({ altKey: true, key: '\u2260', code: 'Digit0' })).toBe(true);
    // AZERTY: the 0 key gives a-grave.
    expect(isAltZero({ altKey: true, key: '\u00e0', code: 'Digit0' })).toBe(true);
    expect(isAltZero({ altKey: true, key: '0', code: 'Numpad0' })).toBe(true);
  });

  it('is the character 0 with Alt on any key, or with no key place reported', () => {
    expect(isAltZero({ altKey: true, key: '0', code: '' })).toBe(true);
    // Programmer Dvorak: 0 sits on another key.
    expect(isAltZero({ altKey: true, key: '0', code: 'Digit7' })).toBe(true);
  });

  it('needs Alt: a lone 0 never jumps a control', () => {
    expect(isAltZero({ altKey: false, key: '0', code: 'Digit0' })).toBe(false);
    expect(isAltZero({ altKey: true, key: 'End', code: 'End' })).toBe(false);
  });
});
