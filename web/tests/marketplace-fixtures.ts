// Fixtures of plan 16 A8: agent parts, versions and builds as the API
// answers them (synthetic).
import type { Handler } from './mock';
import { GROUP, RESOLVE_GROUP, RESOLVE_PROJECT, signedIn } from './mock';
import { json } from './render';

export const PARTS_PROJECT = {
  id: 50,
  namespace_id: 51,
  path: 'acme/agent-parts',
  name: 'agent-parts',
  display_name: 'Agent parts',
  description: '',
  visibility: 'internal',
  created_at: '2026-10-01T00:00:00Z',
  role: 'reporter',
};

export function version(over: Record<string, unknown> = {}) {
  const slug = (over.slug as string | undefined) ?? 'review-skill';
  const v = (over.version as string | undefined) ?? '1.2.0';
  return {
    capabilities: [] as string[],
    channel: 'stable',
    ci_pipeline_id: 900,
    created_at: '2026-11-20T01:02:03Z',
    digest: `sha256:${'d'.repeat(64)}`,
    key_id: 'k1',
    kind: 'skill',
    manifest: {},
    published_by: 9,
    revoked_at: null,
    revoked_by: null,
    revoked_reason: null,
    signature: 'c2ln',
    slug,
    source_project_id: 50,
    source_sha: 'e'.repeat(40),
    state: 'published',
    tag: `${slug}/v${v}`,
    version: v,
    ...over,
  };
}

export function summary(over: Record<string, unknown> = {}, latest: Record<string, unknown> = {}) {
  const slug = (over.slug as string | undefined) ?? 'review-skill';
  const kind = (over.kind as string | undefined) ?? 'skill';
  return { deprecated: false, kind, slug, latest: version({ slug, kind, ...latest }), ...over };
}

export const CATALOG = [
  summary({ slug: 'review-skill' }, { capabilities: [] }),
  summary({ slug: 'fmt-hook', kind: 'hook' }, { capabilities: ['exec', 'permissions'], channel: 'beta', version: '0.3.0' }),
  summary({ slug: 'kanflow-mcp', kind: 'mcp' }, { capabilities: ['network'], version: '2.0.0' }),
];

/** Resolve answers by the asked path (the project screens and the
 * publishing group). */
export const resolveBy =
  (extra: Record<string, unknown> = {}): Handler =>
  (c) => {
    const path = c.query.get('path');
    if (path === 'acme') return json(200, RESOLVE_GROUP);
    if (path !== null && path in extra) return json(200, extra[path]);
    return json(200, RESOLVE_PROJECT);
  };

/** The marketplace's routes; `groupRole`: the caller's role on the
 * publishing group (`acme`). */
export function marketRoutes(groupRole: string | null, over: Record<string, Handler> = {}): Record<string, Handler> {
  return {
    ...signedIn(),
    'GET /api/v1/agent-parts': () => json(200, { parts: CATALOG }),
    'GET /api/v1/projects/50': () => json(200, PARTS_PROJECT),
    'GET /api/v1/namespaces/resolve': resolveBy(),
    'GET /api/v1/groups/3': () => json(200, { ...GROUP, role: groupRole }),
    ...over,
  };
}
