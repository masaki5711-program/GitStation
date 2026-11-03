// The head the merge request screen has shown (re-review HIGH, 2026-11-03).
// Approve and merge name this head, never one a query read later: it is
// state of the always-mounted merge request screen, set when the screen first
// has the merge request, and moved only by an explicit act of the person —
// "review the new version", a reload after a refusal, or opening Changes
// (which shows that version). A background refetch never moves it, so a push
// between looking and pressing cannot be approved or merged unseen.
//
// Where it lives: the layout remounts the page on every address change (its
// error boundary is keyed by the path), so switching Overview / Changes would
// lose React state. The value is kept in the app's QueryClient under its own
// key per merge request, as a plain entry: the query is disabled and never
// fetched, and its key is outside ['mr', project, iid], so no invalidation or
// refetch of the merge request touches it. Only the acts above write it.
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { createContext, useCallback, useContext, useEffect, useMemo, type ReactNode } from 'react';
import { useNavigate } from 'react-router';

import { mrKeys } from '../../api/mergeRequests';
import type { MergeRequest, MergeStatus } from '../../api/types';
import { pullUrl } from '../repoUrls';

export interface ViewedHead {
  /** The head shown; null while the merge request has none (or is not read). */
  head: string | null;
  /** The screen has read the merge request once (before that nothing counts). */
  known: boolean;
  /** The person was shown this head (Changes rendered its version). */
  accept: (head: string | null) => void;
  /** Read everything again and take the head that is current now. */
  reload: () => Promise<void>;
  /** `reload`, then the Changes tab to look at the new version. */
  reviewNew: () => Promise<void>;
}

const Ctx = createContext<ViewedHead>({
  head: null,
  known: false,
  accept: () => undefined,
  reload: () => Promise.resolve(),
  reviewNew: () => Promise.resolve(),
});

interface Seen {
  head: string | null;
}

/** Not under ['mr', project, iid]: a reload of the merge request leaves it. */
export const viewedHeadKey = (project: number, iid: number) => ['mr-viewed-head', project, iid] as const;

export function useViewedHead(): ViewedHead {
  return useContext(Ctx);
}

export function ViewedHeadProvider({
  projectId,
  project,
  iid,
  mr,
  children,
}: {
  projectId: number;
  project: string;
  iid: number;
  /** The merge request as the screen first rendered it. */
  mr: MergeRequest | undefined;
  children: ReactNode;
}) {
  const queryClient = useQueryClient();
  const navigate = useNavigate();
  const key = useMemo(() => viewedHeadKey(projectId, iid), [projectId, iid]);
  // Read-only view of the entry (never fetched: `enabled: false`).
  const stored = useQuery<Seen | null>({
    queryKey: key,
    enabled: false,
    staleTime: Infinity,
    gcTime: Infinity,
    queryFn: () => queryClient.getQueryData<Seen>(key) ?? null,
  });
  const current = stored.data ?? null;
  const setState = useCallback(
    (seen: Seen) => {
      queryClient.setQueryData<Seen>(key, seen);
    },
    [queryClient, key],
  );

  // The first time the screen has this merge request: what it shows now.
  useEffect(() => {
    if (current === null && mr !== undefined && mr.iid === iid) setState({ head: mr.head_sha ?? null });
  }, [current, mr, iid, setState]);

  const accept = useCallback(
    (head: string | null) => {
      setState({ head });
    },
    [setState],
  );
  const reload = useCallback(async () => {
    await queryClient.invalidateQueries({ queryKey: mrKeys.whole(projectId, iid) });
    const status = queryClient.getQueryData<MergeStatus>(mrKeys.status(projectId, iid));
    const fresh = queryClient.getQueryData<MergeRequest>(mrKeys.one(projectId, iid));
    setState({ head: status?.head_sha ?? fresh?.head_sha ?? null });
  }, [queryClient, projectId, iid, setState]);
  const reviewNew = useCallback(async () => {
    await reload();
    void navigate(pullUrl(project, iid, 'changes'));
  }, [reload, navigate, project, iid]);

  const value = useMemo(
    () => ({ head: current?.head ?? null, known: current !== null, accept, reload, reviewNew }),
    [current, accept, reload, reviewNew],
  );
  return <Ctx.Provider value={value}>{children}</Ctx.Provider>;
}
