// Resolve / reopen, owned by the list that stays mounted (the Overview, the
// Changes tab), not by the thread: on the Overview a thread moves between
// the open list and the resolved summary, which re-mounts it, and TanStack
// drops the callbacks of a `mutate` call whose component unmounted. Awaiting
// `mutateAsync` here outlives that move, so the announcement and the focus
// always follow the server's answer, whichever refetch lands first.
import { useTranslation } from 'react-i18next';

import { useResolve, useUnresolve } from '../../api/mergeRequests';
import type { Discussion } from '../../api/types';
import { useAnnounce } from './announce';

export interface ThreadToggle {
  /** Resolve an open thread, reopen a resolved one. */
  toggle: (discussion: Discussion) => void;
  /** The thread whose change is being sent (null: none). */
  pendingId: number | null;
  /** The last failed change and its thread (shown in that thread). */
  failed: { id: number; error: unknown } | null;
}

export function useThreadToggle(
  projectId: number,
  iid: number,
  onToggled?: (id: number, resolved: boolean) => void,
): ThreadToggle {
  const { t } = useTranslation();
  const announce = useAnnounce();
  const resolve = useResolve(projectId, iid);
  const unresolve = useUnresolve(projectId, iid);
  const toggle = (d: Discussion) => {
    if (resolve.isPending || unresolve.isPending) return;
    const resolving = !d.resolved;
    (resolving ? unresolve : resolve).reset();
    const run = resolving ? resolve.mutateAsync(d.id) : unresolve.mutateAsync(d.id);
    run.then(
      () => {
        announce(resolving ? t('mr.thread.resolvedNotice') : t('mr.thread.reopenedNotice'));
        onToggled?.(d.id, resolving);
      },
      () => {
        // Not swallowed: the mutation keeps the error, `failed` shows it
        // (user_message + request_id) in the thread.
      },
    );
  };
  const current = resolve.isPending || resolve.isError ? resolve : unresolve;
  const id = current.variables ?? null;
  return {
    toggle,
    pendingId: current.isPending ? id : null,
    failed: current.isError && id !== null ? { id, error: current.error } : null,
  };
}
