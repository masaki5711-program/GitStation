// Synthetic backend for the M3 reads beside the other fakes: a commit's check
// runs (docs/api/checks.md), a merge request's predicted conflicts (X6), the
// document lint migration (plan 13 Q13), a project's integration bindings and
// the binding confirmation (I1a, ADR-0025 X3), the integrations administration,
// and the documents with their referrers (plan 13 §7). Read-only: the journey
// changes none of them. Shapes follow src/api/schema.gen.ts (CheckRunList,
// ConflictsBody, DocLintMigrationBody, IntegrationBindingList,
// BindingConfirmationBody, IntegrationList / IntegrationBody, DocumentListBody,
// DocumentBody, ReferrersBody); no real names, ids or paths.
const C2 = 'b'.repeat(40);

const run = (id, name, conclusion, summary, annotations = []) => ({
  id,
  name,
  head_sha: C2,
  status: 'completed',
  conclusion,
  summary,
  details_url: null,
  annotations,
  reported_by: { id: 21, username: 'ci-runner' },
  revision: 1,
  created_at: '2026-10-28T01:05:00.000000Z',
  updated_at: '2026-10-28T01:09:00.000000Z',
  completed_at: '2026-10-28T01:09:00.000000Z',
});

/** !1's head (the U1 merge request): one passing and one failing run with a note. */
const RUNS = [
  run(1, 'ci/build', 'success', 'Built in 41 s.'),
  run(2, 'ci/lint', 'failure', 'One finding.', [
    { path: 'src/lib.rs', start_line: 3, end_line: 3, level: 'failure', title: 'unused', message: 'A synthetic finding.' },
  ]),
];

const scan = (headSha) => ({
  status: 'scanned',
  reason: null,
  stale: false,
  peers: 1,
  pairs_tried: 1,
  pairs_skipped: 0,
  scanned_at: '2026-10-28T01:10:00Z',
  scanned_head_sha: headSha,
  scanned_target_sha: 'a'.repeat(40),
});

const BINDING = {
  integration_id: 4,
  kind: 'kanflow',
  project_id: 7,
  external_project_id: 'kf-demo-1',
  resolves_work_items: true,
  granted_membership: true,
  created_at: '2026-10-20T00:00:00Z',
  created_by: 5,
};

const INTEGRATION = {
  id: 4,
  kind: 'kanflow',
  enabled: true,
  event_types: ['merge_request'],
  service_account: { id: 21, username: 'kanflow-svc' },
  bindings: 1,
  created_at: '2026-10-19T00:00:00Z',
  created_by: 5,
  updated_at: '2026-10-19T00:00:00Z',
};

const DOC_ID = 'SPEC-PUMP-001';
const REV = { name: 'main', full_name: 'refs/heads/main', type: 'branch', commit: 'a'.repeat(40) };
const DOC_SUMMARY = {
  id: DOC_ID,
  path: 'specs/pump.md',
  title: 'Pump specification',
  doctype: 'spec',
  classification: 'internal',
  translations: [],
};

export function readsBackend() {
  /** Answers `{ status, body }` or `{ status, error }`; `null` when not one of these reads. */
  function answer(req, url) {
    if (req.method !== 'GET') return null;
    const path = url.pathname;
    const checks = /^\/api\/v1\/projects\/7\/commits\/([0-9a-f]{40})\/checks$/.exec(path);
    if (checks) {
      return { status: 200, body: { schema_version: 1, head_sha: checks[1], check_runs: checks[1] === C2 ? RUNS : [] } };
    }
    const conflicts = /^\/api\/v1\/projects\/7\/merge-requests\/(\d+)\/conflicts$/.exec(path);
    if (conflicts) {
      const iid = Number(conflicts[1]);
      // !1 and the merge request the journey creates (!2) both change src/lib.rs.
      const other = iid === 1 ? 2 : iid === 2 ? 1 : null;
      return {
        status: 200,
        body: {
          iid,
          head_sha: C2,
          scan: scan(C2),
          predictions:
            other === null
              ? []
              : [
                  {
                    iid: other,
                    head_sha: C2,
                    draft: false,
                    outcome: 'conflict',
                    reason: null,
                    conflicts: ['src/lib.rs'],
                    conflicts_truncated: false,
                    checked_at: '2026-10-28T01:10:00Z',
                  },
                ],
        },
      };
    }
    if (path === '/api/v1/projects/7/doc-lint/migration') {
      return {
        status: 200,
        body: {
          project_id: 7,
          warn_until: null,
          in_force: false,
          set_at: null,
          set_by: null,
          today: '2026-11-03',
          max_until: '2027-05-02',
          viewer: { can_manage: true },
        },
      };
    }
    if (path === '/api/v1/projects/7/integration-bindings') return { status: 200, body: { items: [BINDING] } };
    if (path === '/api/v1/projects/7/integration-bindings/act-as-confirmation') {
      return {
        status: 200,
        body: {
          bound: false,
          integration_id: 4,
          integration_kind: 'kanflow',
          integration_name: 'kanflow-svc',
          intent: url.searchParams.get('intent') ?? 'bind',
          kanflow_project_id: url.searchParams.get('kanflow_project_id') ?? 'kf-demo-1',
          person_display_name: 'Taro Yamada',
          person_username: 'taro',
          project_display_name: 'Forge Web',
          project_id: 7,
          project_path: 'acme/forge-web',
          resolves_work_items: url.searchParams.get('resolves_work_items') === 'true',
        },
      };
    }
    if (path === '/api/v1/admin/integrations') return { status: 200, body: { items: [INTEGRATION], truncated: false } };
    const integration = /^\/api\/v1\/admin\/integrations\/(\d+)(\/bindings)?$/.exec(path);
    if (integration) {
      if (Number(integration[1]) !== INTEGRATION.id) {
        return { status: 404, error: ['Not found.', 'req-integration-404', 'FG-WEB-0001'] };
      }
      return { status: 200, body: integration[2] === undefined ? INTEGRATION : { items: [BINDING] } };
    }
    if (path === '/api/v1/projects/7/documents') {
      return { status: 200, body: { documents: [DOC_SUMMARY], ref: REV, next_after: null, incomplete: null, withheld: null } };
    }
    const doc = /^\/api\/v1\/projects\/7\/documents\/([^/]+)(\/referenced-by)?$/.exec(path);
    if (doc) {
      if (decodeURIComponent(doc[1]) !== DOC_ID) {
        return { status: 404, error: ['Not found.', 'req-doc-404', 'FG-WEB-0001'] };
      }
      if (doc[2] !== undefined) {
        return {
          status: 200,
          body: { id: DOC_ID, block: url.searchParams.get('block'), ref: REV, referrers: [], total: 0, next_after: null, incomplete: null },
        };
      }
      return {
        status: 200,
        body: {
          ...DOC_SUMMARY,
          lang: null,
          owner: 'taro',
          version: 1,
          ref: REV,
          fields: [],
          sections: [],
          refs: [],
          problems: [],
          problem_count: 0,
          incomplete: null,
        },
      };
    }
    return null;
  }
  return { answer };
}
