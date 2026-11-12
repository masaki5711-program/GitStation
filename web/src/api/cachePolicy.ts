// How long the client may trust what it read (plan: every screen under
// 100 ms). Two kinds of data:
//
// - addressed by a full commit id (a tree, a blob, a commit, a diff between
//   two commit ids): git objects never change, so the answer is fresh
//   forever and kept longer after its screen closes;
// - addressed by a name (a branch, a tag, a path in the namespace): it can
//   move at any time, so it keeps the app's short stale time (App.tsx) and
//   is read again on the next visit after that — the cached answer shows at
//   once meanwhile (TanStack Query's stale-while-revalidate), never longer
//   than the stale time without a new read.

/** A full SHA-1 (40) or SHA-256 (64) object id, lowercase as the API sends it. */
const FULL_OID = /^(?:[0-9a-f]{40}|[0-9a-f]{64})$/;

export function isFullOid(value: string): boolean {
  return FULL_OID.test(value);
}

/** Kept after the last screen using it closes (30 minutes). */
export const IMMUTABLE_GC_MS = 30 * 60_000;

/** The options of a query whose answer never changes. */
export const IMMUTABLE = { staleTime: Infinity, gcTime: IMMUTABLE_GC_MS } as const;

/** `IMMUTABLE` when every one of `refs` is a full commit id, else nothing
 * (the app's defaults apply). Only for refs from `refParam` (a branch or tag
 * is passed as its full name `refs/heads/…`, so a bare hex string is a
 * commit) or an address that names a commit: never for a typed name, which
 * may be a branch called like a commit id (see `compareQuery`). */
export function cacheFor(...refs: (string | null | undefined)[]): Partial<typeof IMMUTABLE> {
  return refs.every((r) => typeof r === 'string' && isFullOid(r)) ? IMMUTABLE : {};
}

/** How long a namespace path's meaning (project / group / user, its id) is
 * trusted: a path moves or disappears only by a transfer or a deletion,
 * and those drop the cache themselves (`forgetResolutions`). */
export const RESOLVE_STALE_MS = 5 * 60_000;
