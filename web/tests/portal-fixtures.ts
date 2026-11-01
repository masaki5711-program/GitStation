// Synthetic data for the document portal: a specification with a working
// version and two confirmed versions, chapters under docs/specs, and the
// comparison between the two confirmed versions. No real names or paths.
import { apiError, SPEC, signedIn, page, type Handler } from './mock';
import { json } from './render';

export const A = 'a'.repeat(40); // latest work in progress
export const B = 'b'.repeat(40); // rev-1
export const C = 'c'.repeat(40); // rev-2

export const RESOLVE_SPEC = {
  canonical_path: 'acme/press-spec',
  kind: 'project',
  moved: false,
  namespace_id: 11,
  project_id: 8,
};

const tag = (name: string, commit: string) => ({
  name,
  full_name: `refs/tags/${name}`,
  type: 'tag',
  target: commit,
  commit,
});

export const REFS = {
  default_branch: 'main',
  truncated: false,
  refs: [
    { name: 'main', full_name: 'refs/heads/main', type: 'branch', target: A, commit: A },
    tag('rev-1', B),
    tag('rev-2', C),
  ],
};

const rev = (commit: string) => ({ type: 'commit', name: commit, full_name: null, commit });

const entry = (name: string, path: string, type = 'blob', size: number | null = 100) => ({
  name,
  path,
  type,
  mode: type === 'tree' ? '040000' : '100644',
  oid: 'e'.repeat(40),
  size,
});

const tree = (commit: string, path: string, entries: unknown[], truncated = false) => ({
  ref: rev(commit),
  path,
  oid: 'f'.repeat(40),
  entries,
  truncated,
});

const SPECS = 'docs/specs';
export const CHAPTERS: Record<string, string[]> = {
  [A]: ['01-scope.md', '02-parts.md', '03-notes.md'],
  [B]: ['01-scope.md'],
  [C]: ['01-scope.md', '02-parts.md'],
};

export const BODY_SCOPE = [
  '# Scope of the press line',
  '',
  'See the [parts list](02-parts.md) and an [outside note](https://example.com/x).',
  '',
  '| item | value |',
  '|------|-------|',
  '| load | 40 t |',
].join('\n');

export const commitBody = (commit: string, subject: string, at: string) => ({
  ref: rev(commit),
  head: commit,
  path: '',
  commits: [
    {
      oid: commit,
      parents: [],
      author_name: 'Taro Yamada',
      author_email: 'taro@example.invalid',
      authored_at: at,
      committer_name: 'Taro Yamada',
      committer_email: 'taro@example.invalid',
      committed_at: at,
      subject,
    },
  ],
  next_cursor: null,
  has_more: false,
});

export const MOMENTS: Record<string, { subject: string; at: string }> = {
  [A]: { subject: 'Working draft of the next issue', at: '2026-10-31T09:00:00+09:00' },
  [B]: { subject: 'First issue for approval', at: '2026-10-01T09:00:00+09:00' },
  [C]: { subject: 'Second issue with the parts list', at: '2026-10-20T09:00:00+09:00' },
};

export const PATCH_SCOPE = [
  '@@ -1,3 +1,3 @@',
  ' # Scope of the press line',
  '-The press load is 30 t.',
  '+The press load is 40 t.',
  ' ',
].join('\n');

export const PATCH_PARTS = ['@@ -0,0 +1,2 @@', '+# Parts', '+frame, ram'].join('\n');

const diffFile = (over: Record<string, unknown>) => ({
  status: 'modified',
  old_path: `${SPECS}/01-scope.md`,
  new_path: `${SPECS}/01-scope.md`,
  old_mode: '100644',
  new_mode: '100644',
  old_oid: '1'.repeat(40),
  new_oid: '2'.repeat(40),
  similarity: null,
  binary: false,
  patch: PATCH_SCOPE,
  omitted: null,
  ...over,
});

export const COMPARE_B_C = {
  from: rev(B),
  to: rev(C),
  commits: [],
  commits_truncated: false,
  diff: {
    truncated: false,
    files: [
      diffFile({}),
      diffFile({
        status: 'added',
        old_path: null,
        new_path: `${SPECS}/02-parts.md`,
        patch: PATCH_PARTS,
      }),
      diffFile({ status: 'modified', old_path: 'data/limits.csv', new_path: 'data/limits.csv', patch: '@@ -1 +1 @@\n-1\n+2' }),
    ],
  },
};

export interface PortalOptions {
  refs?: unknown;
  /** Chapters per commit (file names under docs/specs); a missing key: no docs/specs. */
  chapters?: Record<string, string[]>;
  /** Top-level files per commit when docs/specs is missing. */
  topLevel?: Record<string, string[]>;
  treeTruncated?: boolean;
  blob?: (commit: string, path: string) => Record<string, unknown> | null;
  compare?: unknown;
}

export const defaultBlob = (commit: string, path: string) => {
  const name = path.split('/').pop() ?? path;
  const content = name === '01-scope.md' ? BODY_SCOPE : `# ${name}\n\nSynthetic text of ${name}.`;
  return {
    ref: rev(commit),
    path,
    name,
    type: 'blob',
    mode: '100644',
    oid: 'e'.repeat(40),
    size: content.length,
    binary: false,
    content,
    truncated: false,
  };
};

/** The route table of a signed-in reader of the specification (project 8). */
export function portalRoutes(options: PortalOptions = {}): Record<string, Handler> {
  const chapters = options.chapters ?? CHAPTERS;
  const blobOf = options.blob ?? defaultBlob;
  return {
    ...signedIn(),
    'GET /api/v1/namespaces/resolve': () => json(200, RESOLVE_SPEC),
    'GET /api/v1/projects/8': () => json(200, SPEC),
    'GET /api/v1/projects': () => page([SPEC]),
    'GET /api/v1/groups': () => page([]),
    'GET /api/v1/projects/8/repository/refs': () => json(200, options.refs ?? REFS),
    'GET /api/v1/projects/8/repository/commits': (c) => {
      const ref = c.query.get('ref') ?? '';
      const m = MOMENTS[ref];
      return m === undefined
        ? apiError(404, 'No such revision.', 'req-nocommit')
        : json(200, commitBody(ref, m.subject, m.at));
    },
    'GET /api/v1/projects/8/repository/tree': (c) => {
      const ref = c.query.get('ref') ?? '';
      const path = c.query.get('path') ?? '';
      if (path === SPECS) {
        const names = chapters[ref];
        return names === undefined
          ? apiError(404, 'That path does not exist.', 'req-nopath')
          : json(
              200,
              tree(ref, SPECS, names.map((n) => entry(n, `${SPECS}/${n}`)), options.treeTruncated ?? false),
            );
      }
      if (path === '') {
        const names = options.topLevel?.[ref] ?? ['README.txt'];
        return json(200, tree(ref, '', names.map((n) => entry(n, n))));
      }
      return apiError(404, 'That path does not exist.', 'req-nopath');
    },
    'GET /api/v1/projects/8/repository/blob': (c) => {
      const body = blobOf(c.query.get('ref') ?? '', c.query.get('path') ?? '');
      return body === null ? apiError(404, 'That path does not exist.', 'req-noblob') : json(200, body);
    },
    'GET /api/v1/projects/8/repository/compare': () => json(200, options.compare ?? COMPARE_B_C),
  };
}
