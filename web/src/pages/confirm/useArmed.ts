// The delay before the confirm button works (ADR-0025 §5.4): clickjacking
// and tap-through defense. The button is armed only when
//   1. the page has been visible AND focused for `ARM_DELAY_MS`, and then
//   2. a new pointer or key action happened inside the page.
// Hiding the page or losing the focus disarms it and starts over, so a
// click aimed at a moment the person did not choose finds it disabled.
import { useEffect, useState } from 'react';

export const ARM_DELAY_MS = 1000;

const INPUT_EVENTS = ['pointerdown', 'pointermove', 'keydown', 'touchstart'] as const;

export function useArmed(delay: number = ARM_DELAY_MS): { armed: boolean; waited: boolean } {
  const [waited, setWaited] = useState(false);
  const [armed, setArmed] = useState(false);

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
    const onInput = () => {
      if (elapsed && present()) setArmed(true);
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
  }, [delay]);

  return { armed, waited };
}
