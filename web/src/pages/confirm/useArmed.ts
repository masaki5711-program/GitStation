// The delay before the confirm button works (ADR-0025 §5.4): clickjacking
// and tap-through defense. A press is accepted only when
//   1. the page has been visible AND focused for `ARM_DELAY_MS`, and then
//   2. a real (trusted) pointer-down or touch ON the button, or a key press,
//      armed it - and
//   3. that arming action is at least `ARM_GAP_MS` older than the click, so
//      the press itself can never be the action that arms it. (A first click
//      on a freshly armed button is therefore refused with a hint; the
//      second one, a moment later, counts. A keyboard user Tabs to the
//      button, then presses it.)
// Pointer movement does not arm. Hiding the page or losing the focus starts
// over, and an accepted press needs a new arming action afterwards.
import { useCallback, useEffect, useRef, useState, type RefObject } from 'react';

export const ARM_DELAY_MS = 1000;
export const ARM_GAP_MS = 300;

/** Whether an event is a person's. A seam only for tests: jsdom's events are
 * never trusted and `isTrusted` cannot be redefined on them. */
export const eventTrust = { check: (e: Event): boolean => e.isTrusted };

const INPUT_EVENTS = ['pointerdown', 'keydown', 'touchstart'] as const;

export interface Armed {
  /** The 1 s wait is over. */
  waited: boolean;
  /** An arming action happened since the wait ended. */
  armed: boolean;
  /** Ask to press now: `true` only for an arming action older than the gap,
   * and then the arming is used up (a new action is needed to press again). */
  take: () => boolean;
}

export function useArmed(target: RefObject<HTMLElement | null>, delay: number = ARM_DELAY_MS): Armed {
  const [waited, setWaited] = useState(false);
  const [armed, setArmed] = useState(false);
  const armedAt = useRef<number | null>(null);

  useEffect(() => {
    let timer: number | undefined;
    let elapsed = false;
    const present = () => document.visibilityState === 'visible' && document.hasFocus();
    const stop = () => {
      if (timer !== undefined) window.clearTimeout(timer);
      timer = undefined;
    };
    const disarm = () => {
      stop();
      elapsed = false;
      armedAt.current = null;
      setWaited(false);
      setArmed(false);
    };
    const start = () => {
      if (timer !== undefined || elapsed) return;
      timer = window.setTimeout(() => {
        timer = undefined;
        elapsed = true;
        setWaited(true);
      }, delay);
    };
    const sync = () => {
      if (present()) start();
      else disarm();
    };
    const onInput = (e: Event) => {
      // A script's event is not a person's action.
      if (!eventTrust.check(e) || !elapsed || !present() || armedAt.current !== null) return;
      if (e.type !== 'keydown') {
        const button = target.current;
        if (button === null || !(e.target instanceof Node) || !button.contains(e.target)) return;
      }
      armedAt.current = Date.now();
      setArmed(true);
    };

    sync();
    document.addEventListener('visibilitychange', sync);
    window.addEventListener('focus', sync);
    window.addEventListener('blur', sync);
    for (const name of INPUT_EVENTS) document.addEventListener(name, onInput, true);
    return () => {
      stop();
      document.removeEventListener('visibilitychange', sync);
      window.removeEventListener('focus', sync);
      window.removeEventListener('blur', sync);
      for (const name of INPUT_EVENTS) document.removeEventListener(name, onInput, true);
    };
  }, [delay, target]);

  const take = useCallback(() => {
    const at = armedAt.current;
    if (at === null) return false;
    // Too soon: the click may be the tail of the arming gesture itself. Keep
    // the arming; the next click, once the gap has passed, counts.
    if (Date.now() - at < ARM_GAP_MS) return false;
    armedAt.current = null;
    setArmed(false);
    return true;
  }, []);

  return { waited, armed, take };
}
