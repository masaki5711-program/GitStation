// The agent marketplace and the project's agent builder (plan 16 A8, §6):
// the catalog (`GET /api/v1/agent-parts`), one part and one version, the
// publishing actions (publish, promote, revoke), and a project's builder
// (`…/agent-builder`: status, recommend, preview, compile, updates, update).
// Keys start with ['agentParts'] or ['agentBuilder', projectId].
//
// Who sees which button is decided from roles the API already exposes,
// never guessed, and the server decides again on every request:
// - publish, promote, revoke: the caller's role on the publishing group
//   (the parts project's parent group, `GET /api/v1/groups/{id}`), a
//   Maintainer or an administrator; promoting a part that uses the
//   network: an administrator (forge-auth `AgentPart*`);
// - recommend: a Reporter of the project; Dry-Run, status, updates and "Create the
//   merge request": a Developer (forge-auth `AgentBuilder*`). A web session
//   is always a person, so the people-only rule of compile holds here;
//   "Create the update merge request" follows the API's own `can_update`.
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
export type SuggestedPart = S['SuggestedBody'];
export type RecommendLlm = S['RecommendLlmBody'];
export type Updates = S['UpdatesBody'];
export type UpdatePart = S['UpdatePartBody'];
export type UpdateQueued = S['UpdateQueuedBody'];
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
  updates: (project: number) => ['agentBuilder', project, 'updates'] as const,
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

/** How often a recommendation whose AI reasons are `pending` is read again
 * (the worker answers within about 20 s; the server turns a request
 * unanswered for 6 minutes into `unavailable`, which stops this). */
export const RECOMMEND_PENDING_REFETCH_MS = 20_000;

/** The recommendation: a read (it writes nothing), asked as a POST with
 * `{}`. The rules' selection, with the local LLM's reasons when the worker
 * has them (plan 16 A11); read again by itself while they are `pending`. */
export function useRecommendation(project: number, enabled: boolean) {
  const api = useApi();
  return useQuery({
    queryKey: builderKeys.recommend(project),
    enabled,
    refetchInterval: (q: { state: { data: Recommendation | undefined } }) =>
      q.state.data?.llm.state === 'pending' ? RECOMMEND_PENDING_REFETCH_MS : false,
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


// ---- The update (plan 16 A9 routes, A12 screen) ---------------------

/** `GET …/agent-builder/updates`: each locked part, whether its version is
 * revoked, what the update picks and the update's Dry-Run; `can_update`
 * says whether the caller may ask for the update merge request (the
 * route's own trial decision; the server decides again). A Developer's
 * read. */
export function useUpdates(project: number, enabled: boolean) {
  const api = useApi();
  return useQuery({
    queryKey: builderKeys.updates(project),
    enabled,
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/agent-builder/updates', { signal, params: { path: { id: project } } }),
      ),
  });
}

/** `POST …/agent-builder/update` `{}`: queue the update merge request
 * (`202`; the bot's commit and the merge request come from a job). */
export function useAskUpdate(project: number) {
  const api = useApi();
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: () =>
      call(() =>
        api.POST('/api/v1/projects/{id}/agent-builder/update', {
          params: { path: { id: project } },
          body: {},
        }),
      ),
    onSettled: async () => {
      await queryClient.invalidateQueries({ queryKey: builderKeys.status(project) });
      await queryClient.invalidateQueries({ queryKey: builderKeys.updates(project) });
    },
  });
}

/** The parts an update changes: revoked versions first, then the rest in
 * the server's order; a part whose version stays (and is not revoked) is
 * left out. */
export function updateChanges(parts: readonly UpdatePart[]): UpdatePart[] {
  const changed = parts.filter((p) => p.locked_revoked || p.locked_version !== p.next_version);
  return [...changed.filter((p) => p.locked_revoked), ...changed.filter((p) => !p.locked_revoked)];
}
