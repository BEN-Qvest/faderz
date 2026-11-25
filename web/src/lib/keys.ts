// Keyboard shortcuts matched across platforms and layouts.

/**
 * Alt+0, which sets a fader to 0 dB and centres a pan. Matched on the
 * character "0" or on the 0 key's place in the main block or the keypad:
 * on macOS, Option turns the 0 key's character into another one, and a
 * layout may put "0" on another key or give the 0 key no "0".
 */
export function isAltZero(e: Pick<KeyboardEvent, 'altKey' | 'key' | 'code'>): boolean {
  return e.altKey && (e.key === '0' || e.code === 'Digit0' || e.code === 'Numpad0');
}
