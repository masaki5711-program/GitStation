// Leaving a page that holds something the person would lose (a token secret
// shown once, an unsaved edit) asks first. Moved here from the token page so
// the file editor shares it.
import { useEffect, useRef, useState } from 'react';
import { useNavigate } from 'react-router';

/** While `active`, leaving asks first (a secret or an edit would
 * be lost for good). The app uses a plain BrowserRouter, so a router blocker
 * is not available:
 * - a click on an in-app link is intercepted (same-origin, not new-tab, not a
 *   download, not a jump inside this page);
 * - browser Back / Forward: a sentinel history entry sits on top of the page;
 *   Back lands on the page's own entry (same address, nothing re-rendered),
 *   the question is asked, and Cancel puts the sentinel back;
 * - reload and close are covered by `beforeunload` in the panel.
 * A navigation the app starts by code (`navigate()`) cannot be caught without
 * a data router; none of the screens does that while the secret is shown. */
export function useLeaveGuard(active: boolean, onLeave: () => void) {
  const navigate = useNavigate();
  const [target, setTarget] = useState<{ kind: 'link'; to: string } | { kind: 'back' } | null>(null);
  const armed = useRef(false);
  useEffect(() => {
    if (!active) return;
    const arm = () => {
      window.history.pushState({ ...(window.history.state as object | null), gsSecretGuard: true }, '');
      armed.current = true;
    };
    arm();
    const onClick = (e: MouseEvent) => {
      if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
      const anchor = e.target instanceof Element ? e.target.closest('a[href]') : null;
      if (!(anchor instanceof HTMLAnchorElement)) return;
      if (anchor.hasAttribute('download')) return;
      const kind = anchor.getAttribute('target');
      if (kind !== null && kind !== '' && kind !== '_self') return;
      if (anchor.closest('dialog') !== null) return;
      const url = new URL(anchor.href, window.location.href);
      if (url.origin !== window.location.origin) return;
      // A jump inside this page loses nothing.
      if (url.pathname === window.location.pathname && url.search === window.location.search) return;
      e.preventDefault();
      e.stopPropagation();
      setTarget({ kind: 'link', to: url.pathname + url.search + url.hash });
    };
    const onPop = () => {
      // Back consumed the sentinel (or a Forward moved off it).
      if (!armed.current) return;
      armed.current = false;
      setTarget({ kind: 'back' });
    };
    document.addEventListener('click', onClick, true);
    window.addEventListener('popstate', onPop);
    return () => {
      document.removeEventListener('click', onClick, true);
      window.removeEventListener('popstate', onPop);
      // Leave no sentinel behind when the secret goes away on this page.
      if (armed.current && (window.history.state as { gsSecretGuard?: boolean } | null)?.gsSecretGuard === true) {
        armed.current = false;
        window.history.back();
      }
    };
  }, [active]);
  return {
    target,
    /** The page leaves by code (a save): forget the sentinel so the cleanup
     * never steps back. Call it before `navigate(…, { replace: true })`,
     * which then replaces the sentinel entry. */
    release: () => {
      armed.current = false;
    },
    stay: () => {
      if (target?.kind === 'back') {
        window.history.pushState({ ...(window.history.state as object | null), gsSecretGuard: true }, '');
        armed.current = true;
      }
      setTarget(null);
    },
    proceed: () => {
      const to = target;
      setTarget(null);
      onLeave();
      if (to?.kind === 'link') {
        // The sentinel is the top entry: replace it, so the stack is
        // [page, new address] and Back returns to the page.
        armed.current = false;
        void navigate(to.to, { replace: true });
      } else {
        // Back: the sentinel is already gone; go on back one more entry.
        window.history.back();
      }
    },
  };
}
