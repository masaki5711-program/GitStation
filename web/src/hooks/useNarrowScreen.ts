// Below 768px the sidebar is not a column: it opens from the header's menu
// button (plan 15 §3.1). The same breakpoint as Sidebar.module.css /
// App.module.css. Without matchMedia (an old engine, jsdom) the screen is wide.
import { useSyncExternalStore } from 'react';

export const NARROW_QUERY = '(max-width: 767.98px)';

function media(): MediaQueryList | null {
  return typeof window.matchMedia === 'function' ? window.matchMedia(NARROW_QUERY) : null;
}

function subscribe(onChange: () => void): () => void {
  const m = media();
  if (m === null) return () => undefined;
  m.addEventListener('change', onChange);
  return () => {
    m.removeEventListener('change', onChange);
  };
}

export function useNarrowScreen(): boolean {
  return useSyncExternalStore(
    subscribe,
    () => media()?.matches ?? false,
    () => false,
  );
}
