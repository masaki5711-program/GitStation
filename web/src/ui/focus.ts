// Moving the focus after a modal <dialog> closes: focus moved while the
// dialog is still modal is lost (the page behind is inert), so this waits
// until no dialog is open (at most about a second), then focuses the target
// if it is still in the page.
import type { RefObject } from 'react';

export function focusWhenNoDialog(target: RefObject<HTMLElement | null> | (() => HTMLElement | null)) {
  let tries = 0;
  const attempt = () => {
    tries += 1;
    if (document.querySelector('dialog[open]') !== null && tries < 50) {
      window.setTimeout(attempt, 20);
      return;
    }
    const el = typeof target === 'function' ? target() : target.current;
    if (el?.isConnected === true) el.focus();
  };
  window.setTimeout(attempt, 0);
}
