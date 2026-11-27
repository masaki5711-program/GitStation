// The agent marketplace and the project's agent builder (plan 16 A8, §6):
// the catalog (`GET /api/v1/agent-parts`), one part and one version, the
// publishing actions (publish, promote, revoke), and a project's builder
// (`…/agent-builder`: status, recommend, preview, compile). Keys start with
// ['agentParts'] or ['agentBuilder', projectId].
//
// Who sees which button is decided from roles the API already exposes,
// never guessed, and the server decides again on every request:
// - publish, promote, revoke: the caller's role on the publishing group
//   (the parts project's parent group, `GET /api/v1/groups/{id}`), a
//   Maintainer or an administrator; promoting a part that uses the
//   network: an administrator (forge-auth `AgentPart*`);
// - recommend: a Reporter of the project; Dry-Run, status and "Create the
//   merge request": a Developer (forge-auth `AgentBuilder*`). A web session
//   is always a person, so the people-only rule of compile holds here.
import { queryOptions, useMutation, useQuery, useQueryClient } from '@tanstack/react-query';

import { roleRank } from './fileCommits';
import { call, type ApiClient } from './client';
import { useApi } from './context';
import { projectQuery } from './namespaces';
import type { components } from './schema.gen';

type S = components['schemas'];
export type PartSummary = S['AgentPartSummaryBody'];
export type PartVersion = S['AgentPartVersionBody'];
export type PartDetail = S['AgentPartBody'];
export type PartVersionDetail = S['AgentPartVersionDetailBody'];
export type BuildPreview = S['BuildPreviewBody'];
export type BuildCompiled = S['BuildCompiledBody'];
export type BuildRecord = S['BuildRecordBody'];
export type Recommendation = S['RecommendBody'];
export type SelectionItem = S['SelectionItem'];
export type BlobBody = S['BlobBody'];

/** The kinds of part, in the order of the catalog's tabs. */
export const PART_KINDS = ['skill', 'agent', 'rule', 'hook', 'command', 'mcp', 'adapter'] as const;
export type PartKind = (typeof PART_KINDS)[number];

export function parseKind(value: string | null): PartKind | null {
  return PART_KINDS.find((k) => k === value) ?? null;
}

/** The capabilities a part can declare (ADR-0032), in a fixed order. */
export const CAPABILITIES = ['exec', 'network', 'permissions', 'fs-write'] as const;
export type Capability = (typeof CAPABILITIES)[number];

/** The declared capabilities in the fixed order; unknown words are kept
 * after them (shown as they are, never dropped). */
export function orderedCapabilities(caps: readonly string[]): string[] {
  const known = CAPABILITIES.filter((c) => caps.includes(c));
  const other = caps.filter((c) => !(CAPABILITIES as readonly string[]).includes(c)).sort();
  return [...known, ...other];
}

/** What a new version adds and drops against the previous one. */
export function capabilityDiff(
  before: readonly string[],
  after: readonly string[],
): { added: string[]; removed: string[]; kept: string[] } {
  return {
    added: orderedCapabilities(after.filter((c) => !before.includes(c))),
    removed: orderedCapabilities(before.filter((c) => !after.includes(c))),
    kept: orderedCapabilities(after.filter((c) => before.includes(c))),
  };
}

export const partKeys = {
  all: ['agentParts'] as const,
  list: (kind: PartKind | null, channel: string | null, q: string) =>
    ['agentParts', 'list', kind, channel, q] as const,
  one: (slug: string) => ['agentParts', 'one', slug] as const,
  version: (slug: string, version: string) => ['agentParts', 'version', slug, version] as const,
};

export const partListQuery = (api: ApiClient, kind: PartKind | null, channel: string | null, q: string) =>
  queryOptions({
    queryKey: partKeys.list(kind, channel, q),
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/agent-parts', {
          signal,
          params: {
            query: {
              ...(kind === null ? {} : { kind }),
              ...(channel === null ? {} : { channel }),
              ...(q === '' ? {} : { q }),
            },
          },
        }),
      ),
  });

export function useParts(kind: PartKind | null, channel: string | null, q: string) {
  const api = useApi();
  return useQuery(partListQuery(api, kind, channel, q));
}

export function usePart(slug: string) {
  const api = useApi();
  return useQuery({
    queryKey: partKeys.one(slug),
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/agent-parts/{slug}', { signal, params: { path: { slug } } })),
  });
}

export function usePartVersion(slug: string, version: string | null) {
  const api = useApi();
  return useQuery({
    queryKey: partKeys.version(slug, version ?? ''),
    enabled: version !== null,
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/agent-parts/{slug}/versions/{version}', {
          signal,
          params: { path: { slug, version: version ?? '' } },
        }),
      ),
  });
}

/** May the caller publish, promote or revoke parts? From their role on the
 * publishing group (a Maintainer) or the administrator flag. */
export function mayManageParts(groupRole: string | null | undefined, isAdmin: boolean): boolean {
  return isAdmin || roleRank(groupRole) >= roleRank('maintainer');
}

/** May the caller move this version to stable? */
export function mayPromote(v: PartVersion, groupRole: string | null | undefined, isAdmin: boolean): boolean {
  if (v.state !== 'published' || v.channel !== 'beta') return false;
  if (!mayManageParts(groupRole, isAdmin)) return false;
  // A part that uses the network: an administrator only.
  return !v.capabilities.includes('network') || isAdmin;
}

export function mayRevoke(v: PartVersion, groupRole: string | null | undefined, isAdmin: boolean): boolean {
  return v.state === 'published' && mayManageParts(groupRole, isAdmin);
}

/** The group's path of a project path (`group/sub/parts` → `group/sub`). */
export function groupPathOf(projectPath: string): string | null {
  const at = projectPath.lastIndexOf('/');
  return at <= 0 ? null : projectPath.slice(0, at);
}

/**
 * The caller's role on the publishing group of the parts project
 * `sourceProjectId` (`undefined` while it is read, `null` when there is no
 * role or it cannot be read: no button is shown then).
 */
export function usePublishingGroupRole(sourceProjectId: number | null): {
  role: string | null | undefined;
} {
  const api = useApi();
  const project = useQuery({
    ...projectQuery(api, sourceProjectId ?? 0),
    enabled: sourceProjectId !== null,
  });
  const groupPath = project.isSuccess ? groupPathOf(project.data.path) : null;
  const resolved = useQuery({
    queryKey: ['agentParts', 'groupOf', groupPath],
    enabled: groupPath !== null,
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/namespaces/resolve', { signal, params: { query: { path: groupPath ?? '' } } }),
      ),
  });
  const groupId = resolved.isSuccess && resolved.data.kind === 'group' ? (resolved.data.group_id ?? null) : null;
  const group = useQuery({
    queryKey: ['group', groupId],
    enabled: groupId !== null,
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/groups/{id}', { signal, params: { path: { id: groupId ?? 0 } } })),
  });
  if (sourceProjectId === null) return { role: null };
  if (project.isError || resolved.isError || group.isError) return { role: null };
  if (project.isSuccess && groupPath === null) return { role: null };
  if (resolved.isSuccess && groupId === null) return { role: null };
  if (group.isSuccess) return { role: group.data.role ?? null };
  return { role: undefined };
}

function useInvalidateParts() {
  const queryClient = useQueryClient();
  return () => queryClient.invalidateQueries({ queryKey: partKeys.all });
}

export function usePublish() {
  const api = useApi();
  const invalidate = useInvalidateParts();
  return useMutation({
    mutationFn: (v: { slug: string; tag: string }) =>
      call(() =>
        api.POST('/api/v1/agent-parts/{slug}/versions', {
          params: { path: { slug: v.slug } },
          body: { tag: v.tag },
        }),
      ),
    onSuccess: async () => {
      await invalidate();
    },
  });
}

export function usePromote() {
  const api = useApi();
  const invalidate = useInvalidateParts();
  return useMutation({
    mutationFn: (v: { slug: string; version: string }) =>
      call(() =>
        api.POST('/api/v1/agent-parts/{slug}/versions/{version}/promote', {
          params: { path: { slug: v.slug, version: v.version } },
        }),
      ),
    onSuccess: async () => {
      await invalidate();
    },
  });
}

export function useRevoke() {
  const api = useApi();
  const invalidate = useInvalidateParts();
  return useMutation({
    mutationFn: (v: { slug: string; version: string; reason: string }) =>
      call(() =>
        api.POST('/api/v1/agent-parts/{slug}/versions/{version}/revoke', {
          params: { path: { slug: v.slug, version: v.version } },
          body: { reason: v.reason },
        }),
      ),
    onSuccess: async () => {
      await invalidate();
    },
  });
}

// ---- The project's agent builder -------------------------------------

export const builderKeys = {
  all: (project: number) => ['agentBuilder', project] as const,
  status: (project: number) => ['agentBuilder', project, 'status'] as const,
  recommend: (project: number) => ['agentBuilder', project, 'recommend'] as const,
};

/** May the caller see the recommendation (a Reporter)? */
export function mayRecommend(role: string | null | undefined, isAdmin: boolean): boolean {
  return isAdmin || roleRank(role) >= roleRank('reporter');
}

/** May the caller run the Dry-Run, read the builds and open the merge
 * request (a Developer; a web session is a person)? */
export function mayBuild(role: string | null | undefined, isAdmin: boolean): boolean {
  return isAdmin || roleRank(role) >= roleRank('developer');
}

export function useBuilderStatus(project: number, enabled: boolean) {
  const api = useApi();
  return useQuery({
    queryKey: builderKeys.status(project),
    enabled,
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/agent-builder', { signal, params: { path: { id: project } } }),
      ),
  });
}

/** The rules-only recommendation: a read (it writes nothing), asked as a
 * POST with `{}`. */
export function useRecommendation(project: number, enabled: boolean) {
  const api = useApi();
  return useQuery({
    queryKey: builderKeys.recommend(project),
    enabled,
    queryFn: ({ signal }) =>
      call(() =>
        api.POST('/api/v1/projects/{id}/agent-builder/recommend', {
          signal,
          params: { path: { id: project } },
          body: {},
        }),
      ),
  });
}

export function usePreview(project: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (selection: SelectionItem[]) =>
      call(() =>
        api.POST('/api/v1/projects/{id}/agent-builder/preview', {
          params: { path: { id: project } },
          body: { selection },
        }),
      ),
    onSettled: async () => {
      await queryClient.invalidateQueries({ queryKey: builderKeys.status(project) });
    },
  });
}

export function useCompile(project: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (selection: SelectionItem[]) =>
      call(() =>
        api.POST('/api/v1/projects/{id}/agent-builder/compile', {
          params: { path: { id: project } },
          body: { selection },
        }),
      ),
    onSettled: async () => {
      await queryClient.invalidateQueries({ queryKey: builderKeys.status(project) });
    },
  });
}

/** The lock's path in a repository (`forge-agent`). */
export const LOCK_PATH = '.gitstation/agent.lock.json';

/** The parts a lock pins (slug → version); `null` when the text is not a
 * lock this screen can read (the band is then not shown). */
export function lockedVersions(text: string): Map<string, string> | null {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch {
    return null;
  }
  if (typeof parsed !== 'object' || parsed === null) return null;
  const parts = (parsed as { parts?: unknown }).parts;
  if (!Array.isArray(parts)) return null;
  const out = new Map<string, string>();
  for (const p of parts) {
    if (typeof p !== 'object' || p === null) return null;
    const { slug, version } = p as { slug?: unknown; version?: unknown };
    if (typeof slug !== 'string' || typeof version !== 'string') return null;
    out.set(slug, version);
  }
  return out;
}

/** The recommended parts whose version differs from the lock's (the
 * update band). Parts the lock does not have are not updates. */
export function updatesOf(
  locked: Map<string, string>,
  recommended: readonly { slug: string; version: string }[],
): { slug: string; from: string; to: string }[] {
  const out: { slug: string; from: string; to: string }[] = [];
  for (const r of recommended) {
    const from = locked.get(r.slug);
    if (from !== undefined && from !== r.version) out.push({ slug: r.slug, from, to: r.version });
  }
  return out;
}
