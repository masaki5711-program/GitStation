// Synthetic data for the merge sections of the project settings (U2): a
// protected ref with the R6 fields, an approval rule, and the route table of
// the settings screen as a Maintainer (every viewer flag on).
import { mergeSettings } from './mr-fixtures';
import { page, PROJECT, RESOLVE_PROJECT, signedIn, type Handler } from './mock';
import { json } from './render';

const API = '/api/v1/projects/7';
const MANAGER = { can_manage: true, can_loosen: true, can_suspend_codeowners: true };

export function protectedRef(id: number, pattern: string, over: Record<string, unknown> = {}) {
  return {
    id,
    pattern,
    push_min_role: 'maintainer',
    created_at: '2026-10-01T00:00:00Z',
    updated_at: '2026-10-01T00:00:00Z',
    merge_rule: null,
    warnings: [],
    ...over,
  };
}

export const RULE_STAMP = '2026-11-03T02:00:00.000001Z';

/** An approval rule as the API answers it: approvers by id and by name. */
export const RULE = {
  approval_rule_id: 1,
  name: 'leads',
  target_pattern: 'refs/heads/main',
  required_approvals: 2,
  user_ids: [6],
  group_ids: [3],
  users: [{ id: 6, username: 'jiro', display_name: 'Jiro Suzuki' }],
  groups: [{ id: 3, full_path: 'acme', display_name: 'Acme Plant' }],
  updated_at: RULE_STAMP,
};

/** The document lint migration (plan 13 Q13) as the API answers it: none. */
export function docLintMigration(over: Record<string, unknown> = {}) {
  return {
    project_id: 7,
    warn_until: null,
    in_force: false,
    set_by: null,
    set_at: null,
    today: '2026-11-10',
    max_until: '2027-05-09',
    viewer: { can_manage: false },
    ...over,
  };
}

export function settingsRoutes(over: Record<string, Handler> = {}): Record<string, Handler> {
  return {
    ...signedIn(),
    'GET /api/v1/namespaces/resolve': () => json(200, RESOLVE_PROJECT),
    [`GET ${API}`]: () => json(200, { ...PROJECT, role: 'maintainer' }),
    [`GET ${API}/protected-refs`]: () => json(200, { rules: [] }),
    [`GET ${API}/merge-settings`]: () => json(200, mergeSettings({ viewer: MANAGER })),
    [`GET ${API}/approval-rules`]: () => json(200, { rules: [] }),
    // The kanflow bindings (IntegrationBindings.tsx): none.
    [`GET ${API}/integration-bindings`]: () => json(200, { items: [] }),
    // Plan 13 Q13: the document lint migration (none).
    [`GET ${API}/doc-lint/migration`]: () => json(200, docLintMigration({ viewer: { can_manage: true } })),
    // The rules name their approvers: no member list is read for them.
    'GET /api/v1/groups': () =>
      page([
        { id: 3, namespace_id: 30, path: 'acme', name: 'acme', display_name: 'Acme Plant', description: '', created_at: '2026-09-01T00:00:00Z', role: 'owner' },
      ]),
    'GET /api/v1/projects': () => page([]),
    ...over,
  };
}
