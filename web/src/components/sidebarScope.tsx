// What the sidebar is about (plan 15 §3.1): a project, a group, or — by
// default — "My work". The namespace resolver (Namespace.tsx) is the one
// place that knows what a path is, so it reports it here; the sidebar shows
// the scope only while the address is still inside that namespace, so a
// scope left over from an earlier page never shows on another one.
import { createContext, useContext, useEffect, useMemo, useState, type ReactNode } from 'react';

import { pathSegments } from '../features/paths';

export type SidebarScope =
  | { kind: 'project'; id: number; path: string }
  | { kind: 'group'; id: number; path: string };

interface Value {
  scope: SidebarScope | null;
  setScope: (scope: SidebarScope | null) => void;
}

const ScopeContext = createContext<Value>({
  scope: null,
  setScope: () => undefined,
});

export function SidebarScopeProvider({ children }: { children: ReactNode }) {
  const [scope, setScope] = useState<SidebarScope | null>(null);
  const value = useMemo(() => ({ scope, setScope }), [scope]);
  return <ScopeContext.Provider value={value}>{children}</ScopeContext.Provider>;
}

/** Reports the namespace this page is in: `null` none (a person, not
 * found), `undefined` not known yet (still resolving, or about to be
 * redirected): the last report stands. Leaving the page clears it. */
export function useReportSidebarScope(scope: SidebarScope | null | undefined): void {
  const { setScope } = useContext(ScopeContext);
  const known = scope !== undefined;
  const kind = scope?.kind ?? null;
  const id = scope?.id ?? null;
  const path = scope?.path ?? null;
  useEffect(() => {
    if (!known) return;
    setScope(kind === null || id === null || path === null ? null : { kind, id, path });
  }, [setScope, known, kind, id, path]);
  useEffect(
    () => () => {
      setScope(null);
    },
    [setScope],
  );
}

/** The namespace part of an address and the screen words after `/-/`. */
export function splitAddress(pathname: string): { namespace: string; view: string[] } | null {
  const segments = pathSegments(pathname);
  if (segments === null || segments.length === 0) return null;
  const dash = segments.indexOf('-');
  const namespace = dash < 0 ? segments : segments.slice(0, dash);
  if (namespace.length === 0) return null;
  return { namespace: namespace.join('/'), view: dash < 0 ? [] : segments.slice(dash + 1) };
}

/** The reported scope if the address is (still) inside it, and the screen words. */
export function useSidebarScope(pathname: string): { scope: SidebarScope; view: string[] } | null {
  const { scope } = useContext(ScopeContext);
  if (scope === null) return null;
  const address = splitAddress(pathname);
  if (address?.namespace !== scope.path) return null;
  return { scope, view: address.view };
}
