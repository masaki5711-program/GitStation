// Synthetic repository data for the browse screens (no real names, ids or
// paths): a small project with a README, a source folder, an image, a binary
// file, two commits and a blame. `repoRoutes()` is the route table the page
// tests and the axe check share.
import { json } from './render';
import { apiError, PROJECT, REFS_FILLED, signedIn, type Handler } from './mock';

export const C1 = 'a'.repeat(40);
export const C2 = 'b'.repeat(40);
export const MAIN = {
  type: 'branch',
  name: 'main',
  full_name: 'refs/heads/main',
  commit: C1,
};

const entry = (name: string, path: string, type: string, size: number | null) => ({
  name,
  path,
  type,
  mode: type === 'tree' ? '040000' : '100644',
  oid: 'e'.repeat(40),
  size,
});

export const TREE_ROOT = {
  ref: MAIN,
  path: '',
  oid: 'f'.repeat(40),
  truncated: false,
  entries: [
    entry('README.md', 'README.md', 'blob', 120),
    entry('src', 'src', 'tree', null),
    entry('logo.png', 'logo.png', 'blob', 2048),
    entry('data.bin', 'data.bin', 'blob', 4096),
    entry('vendor', 'vendor', 'submodule', null),
  ],
};

export const TREE_SRC = {
  ref: MAIN,
  path: 'src',
  oid: 'f'.repeat(40),
  truncated: false,
  entries: [entry('lib.rs', 'src/lib.rs', 'blob', 40), entry('util', 'src/util', 'tree', null)],
};

export const README = [
  '# Press Line Spec',
  '',
  'Synthetic readme with a [guide](docs/guide.md), an [outside link](https://example.com/x),',
  'and a table:',
  '',
  '| a | b |',
  '|---|---|',
  '| 1 | 2 |',
  '',
  '```rust',
  'fn main() {}',
  '```',
  '',
  '![logo](logo.png)',
].join('\n');

export const blob = (path: string, content: string | null, over: Record<string, unknown> = {}) => ({
  ref: MAIN,
  path,
  name: path.split('/').pop() ?? path,
  type: 'blob',
  mode: '100644',
  oid: 'e'.repeat(40),
  size: content === null ? 4096 : content.length,
  binary: content === null,
  content,
  truncated: false,
  ...over,
});

export const LIB_RS = 'pub fn add(a: i32, b: i32) -> i32 {\n    // sum\n    a + b\n}\n';

export const commit = (oid: string, subject: string, parents: string[]) => ({
  oid,
  parents,
  author_name: 'Taro Yamada',
  author_email: 'taro@example.invalid',
  authored_at: '2026-10-30T09:00:00+09:00',
  committer_name: 'Taro Yamada',
  committer_email: 'taro@example.invalid',
  committed_at: '2026-10-30T09:00:00+09:00',
  subject,
});

export const COMMIT_1 = commit(C1, 'Add the add function', [C2]);
export const COMMIT_2 = commit(C2, 'Initial commit', []);

export const PATCH = [
  '@@ -1,2 +1,3 @@',
  ' pub fn add(a: i32, b: i32) -> i32 {',
  '-    a - b',
  '+    // sum',
  '+    a + b',
  ' }',
].join('\n');

const diffFile = (over: Record<string, unknown>) => ({
  status: 'modified',
  old_path: 'src/lib.rs',
  new_path: 'src/lib.rs',
  old_mode: '100644',
  new_mode: '100644',
  old_oid: '1'.repeat(40),
  new_oid: '2'.repeat(40),
  similarity: null,
  binary: false,
  patch: PATCH,
  omitted: null,
  ...over,
});

export const DIFF = {
  truncated: false,
  files: [
    diffFile({}),
    diffFile({ status: 'modified', old_path: 'data.bin', new_path: 'data.bin', binary: true, patch: null }),
    diffFile({ status: 'modified', old_path: 'big.sql', new_path: 'big.sql', patch: null, omitted: 'too_large' }),
  ],
};

export const COMMIT_DETAIL = {
  commit: COMMIT_1,
  message: 'Add the add function\n\nSynthetic body line.',
  diff: DIFF,
};

export const BLAME = {
  ref: MAIN,
  path: 'src/lib.rs',
  oid: 'e'.repeat(40),
  size: LIB_RS.length,
  binary: false,
  truncated: false,
  commits: [
    {
      oid: C2,
      author_name: 'Taro Yamada',
      author_email: 'taro@example.invalid',
      author_time: 1790000000,
      author_tz: '+0900',
      summary: 'Initial commit',
      boundary: true,
      path: 'src/lib.rs',
    },
    {
      oid: C1,
      author_name: 'Hanako Sato',
      author_email: 'hanako@example.invalid',
      author_time: 1790100000,
      author_tz: '+0900',
      summary: 'Add the add function',
      boundary: false,
      path: 'src/lib.rs',
    },
  ],
  hunks: [
    { commit: C2, start: 1, orig_start: 1, lines: ['pub fn add(a: i32, b: i32) -> i32 {'] },
    { commit: C1, start: 2, orig_start: 2, lines: ['    // sum', '    a + b'] },
    { commit: C2, start: 4, orig_start: 3, lines: ['}'] },
  ],
};

export const COMPARE = {
  from: { type: 'branch', name: 'main', full_name: 'refs/heads/main', commit: C2 },
  to: { type: 'branch', name: 'dev', full_name: 'refs/heads/dev', commit: C1 },
  commits: [COMMIT_1],
  commits_truncated: false,
  diff: DIFF,
};

/** Every browse route, answered with the fixtures above. Override per test. */
export function repoRoutes(over: Record<string, Handler> = {}): Record<string, Handler> {
  return {
    ...signedIn(),
    'GET /api/v1/namespaces/resolve': () =>
      json(200, {
        canonical_path: 'acme/forge-web',
        kind: 'project',
        moved: false,
        namespace_id: 11,
        project_id: 7,
      }),
    'GET /api/v1/projects/7': () => json(200, PROJECT),
    'GET /api/v1/projects/7/repository/refs': () => json(200, REFS_FILLED),
    'GET /api/v1/projects/7/repository/resolve': (c) => {
      const spec = c.query.get('spec') ?? '';
      const [ref = '', ...rest] = spec.split('/');
      if (ref !== 'main' && ref !== 'dev' && ref !== C1) {
        return apiError(404, '指定された場所が見つかりません。', 'req-404', 'FG-REPO-0023');
      }
      const full = ref === C1 ? null : `refs/heads/${ref}`;
      return json(200, {
        ref: { type: ref === C1 ? 'commit' : 'branch', name: ref, full_name: full, commit: ref === 'dev' ? C2 : C1 },
        path: rest.join('/'),
      });
    },
    'GET /api/v1/projects/7/repository/tree': (c) => {
      const path = c.query.get('path') ?? '';
      if (path === '') return json(200, TREE_ROOT);
      if (path === 'src') return json(200, TREE_SRC);
      return apiError(404, '指定された場所が見つかりません。', 'req-404', 'FG-REPO-0023');
    },
    'GET /api/v1/projects/7/repository/blob': (c) => {
      const path = c.query.get('path') ?? '';
      if (path === 'README.md') return json(200, blob(path, README));
      if (path === 'src/lib.rs') return json(200, blob(path, LIB_RS));
      if (path === 'logo.png') return json(200, blob(path, null));
      if (path === 'data.bin') return json(200, blob(path, null));
      return apiError(404, '指定された場所が見つかりません。', 'req-404', 'FG-REPO-0023');
    },
    'GET /api/v1/projects/7/repository/commits': () =>
      json(200, {
        ref: MAIN,
        head: C1,
        path: '',
        commits: [COMMIT_1, COMMIT_2],
        next_cursor: null,
        has_more: false,
      }),
    [`GET /api/v1/projects/7/repository/commits/${C1}`]: () => json(200, COMMIT_DETAIL),
    'GET /api/v1/projects/7/repository/blame': () => json(200, BLAME),
    'GET /api/v1/projects/7/repository/compare': () => json(200, COMPARE),
    ...over,
  };
}
