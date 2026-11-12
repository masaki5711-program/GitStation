// Prefetch on intent (plan: every screen under 100 ms). When the pointer
// rests on a link (or it gets focus, or a finger touches it), the target
// screen's primary queries are asked for before the click, with the same
// options (key, request) the screen uses, so the screen renders from the
// cache. Rules:
//
// - only addresses inside this app's namespace routes (`/{path}[/-/…]`);
// - at most one prefetch in flight per target (a second hover joins it);
// - nothing on a slow connection or with Save-Data;
// - a failure is not hidden: TanStack Query keeps it as the query's state,
//   and the screen asks again when it opens (and shows the error with its
//   request id through ErrorNotice if it fails again). Prefetching itself
//   never shows anything.
import type { QueryClient } from '@tanstack/react-query';

import type { ApiClient } from '../api/client';
import { mergeRequestQuery, mergeRequestsQuery, parseIid, type MrListFilter, type MrState } from '../api/mergeRequests';
import { projectQuery, resolveQuery } from '../api/namespaces';
import { parseId, pipelinesQuery } from '../api/pipelines';
import {
  blameQuery,
  blobQuery,
  commitQuery,
  commitsQuery,
  defaultSpec,
  latestCommitQuery,
  refParam,
  refsQuery,
  resolveSpecQuery,
  treeQuery,
  type Spec,
} from '../api/repo';
import { isFullOid } from '../api/cachePolicy';
import { pathSegments } from './paths';
import { preloadScreen } from '../pages/lazyScreens';
import { isReadmeName } from './repoUrls';

/** How long the pointer must rest on a link before its screen is asked for. */
export const INTENT_DELAY_MS = 50;

interface NetworkInformationLike {
  saveData?: boolean;
  effectiveType?: string;
}

/** Save-Data, or a 2G-class connection: prefetching would cost more than it saves. */
export function connectionIsSlow(nav: Navigator | undefined = globalThis.navigator): boolean {
  const c = (nav as (Navigator & { connection?: NetworkInformationLike }) | undefined)?.connection;
  if (c === undefined) return false;
  return c.saveData === true || c.effectiveType === 'slow-2g' || c.effectiveType === '2g';
}

/** A prefetch's outcome is not the caller's to act on: on a failure the
 * query keeps its error state (see the header comment). */
export function settle(p: Promise<unknown>): Promise<void> {
  return p.then(
    () => undefined,
    () => undefined,
  );
}

const flights = new WeakMap<QueryClient, Map<string, Promise<void>>>();

/** Asks for what the screen at `to` reads first. Resolves when done (never
 * rejects); a second call for the same target while the first runs joins it. */
export function prefetchRoute(queryClient: QueryClient, api: ApiClient, to: string): Promise<void> {
  if (connectionIsSlow()) return Promise.resolve();
  let url: URL;
  try {
    url = new URL(to, window.location.origin);
  } catch {
    return Promise.resolve();
  }
  if (url.origin !== window.location.origin) return Promise.resolve();
  const target = url.pathname + url.search;
  let running = flights.get(queryClient);
  if (running === undefined) {
    running = new Map();
    flights.set(queryClient, running);
  }
  const current = running.get(target);
  if (current !== undefined) return current;
  const map = running;
  const flight = prefetchAddress(queryClient, api, url)
    // The failure stays in the query's state (see the header comment).
    .catch(() => undefined)
    .finally(() => {
      map.delete(target);
    });
  map.set(target, flight);
  return flight;
}

const MR_STATES: readonly MrState[] = ['open', 'merged', 'closed', 'all'];

function beforeOf(raw: string | null): number | undefined {
  return raw === null ? undefined : (parseId(raw) ?? undefined);
}

async function prefetchAddress(queryClient: QueryClient, api: ApiClient, url: URL): Promise<void> {
  const segments = pathSegments(url.pathname);
  if (segments === null || segments.length === 0) return;
  const dash = segments.indexOf('-');
  const namespace = dash < 0 ? segments : segments.slice(0, dash);
  // `/-/…`: the fixed screens read their own lists; nothing to resolve.
  if (namespace.length === 0) return;
  const path = namespace.join('/');
  const view = dash < 0 ? null : segments.slice(dash + 1);
  const r = await queryClient.query(resolveQuery(api, queryClient, path, view === null));
  // A moved or differently spelled path is redirected first: its screen
  // will ask under the canonical path.
  if (r.canonical_path !== path || r.kind !== 'project' || r.project_id == null) return;
  const id = r.project_id;
  void settle(queryClient.query(projectQuery(api, id)));
  if (view === null) {
    if (r.remainder === undefined || r.remainder === null || r.remainder === '') {
      await prefetchProjectHome(queryClient, api, id);
    }
    return;
  }
  await prefetchScreen(queryClient, api, id, view, url.searchParams);
}

async function specOf(queryClient: QueryClient, api: ApiClient, id: number, spec: string): Promise<Spec | undefined> {
  if (spec === '') return defaultSpec(await queryClient.query(refsQuery(api, id)));
  const resolved = await queryClient.query(resolveSpecQuery(api, id, spec));
  return { rev: resolved.ref, path: resolved.path };
}

/** The project top page: the refs, then the default branch's root, its
 * README and the newest commit. */
export async function prefetchProjectHome(queryClient: QueryClient, api: ApiClient, id: number): Promise<void> {
  const spec = await specOf(queryClient, api, id, '');
  if (spec === undefined || spec.rev.full_name == null) return;
  const ref = refParam(spec.rev);
  void settle(queryClient.query(latestCommitQuery(api, id, spec.rev.full_name)));
  const tree = await queryClient.query(treeQuery(api, id, ref, ''));
  const readme = tree.entries.find((e) => e.type === 'blob' && isReadmeName(e.name));
  if (readme !== undefined) await settle(queryClient.query(blobQuery(api, id, ref, readme.path)));
}

async function prefetchScreen(
  queryClient: QueryClient,
  api: ApiClient,
  id: number,
  view: string[],
  search: URLSearchParams,
): Promise<void> {
  const [word, ...tail] = view;
  if (word !== undefined) preloadScreen(word);
  switch (word) {
    case 'tree':
    case 'blob':
    case 'blame':
    case 'commits': {
      const spec = await specOf(queryClient, api, id, tail.join('/'));
      if (spec === undefined) return;
      const ref = refParam(spec.rev);
      if (word === 'tree') await settle(queryClient.query(treeQuery(api, id, ref, spec.path)));
      else if (word === 'commits') await settle(queryClient.infiniteQuery(commitsQuery(api, id, ref, spec.path)));
      else if (spec.path !== '' && word === 'blob') await settle(queryClient.query(blobQuery(api, id, ref, spec.path)));
      else if (spec.path !== '') await settle(queryClient.query(blameQuery(api, id, ref, spec.path)));
      return;
    }
    case 'commit': {
      const oid = (tail[0] ?? '').toLowerCase();
      if (tail.length === 1 && isFullOid(oid)) await settle(queryClient.query(commitQuery(api, id, oid)));
      return;
    }
    case 'branches':
    case 'tags':
    case 'compare':
      await settle(queryClient.query(refsQuery(api, id)));
      return;
    case 'pulls': {
      if (tail.length !== 0) return;
      const state = MR_STATES.find((s) => s === search.get('state')) ?? 'open';
      const before = beforeOf(search.get('before'));
      const filter: MrListFilter = { state, ...(before === undefined ? {} : { before }) };
      await settle(queryClient.query(mergeRequestsQuery(api, id, filter)));
      return;
    }
    case 'pull': {
      const iid = parseIid(tail[0] ?? '');
      if (iid !== null) await settle(queryClient.query(mergeRequestQuery(api, id, iid)));
      return;
    }
    case 'pipelines':
      if (tail.length === 0) {
        await settle(queryClient.query(pipelinesQuery(api, id, beforeOf(search.get('before')))));
      }
      return;
    default:
      // Settings, members, webhooks, documents: the project is enough.
      return;
  }
}

/** When the browser has nothing else to do (a timer where there is no
 * `requestIdleCallback`). Returns the cancel. */
export function whenIdle(run: () => void): () => void {
  if (typeof window.requestIdleCallback === 'function') {
    const handle = window.requestIdleCallback(run, { timeout: 2000 });
    return () => {
      window.cancelIdleCallback(handle);
    };
  }
  const handle = window.setTimeout(run, 200);
  return () => {
    window.clearTimeout(handle);
  };
}
