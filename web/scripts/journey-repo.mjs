// Synthetic repository data for the browser journey: the same shapes the
// vitest fixtures (tests/repo-fixtures.ts) use, answered as the browse API
// does (ADR-0015 amendments MR7 / MR8). Project 7 only.
const C1 = 'a'.repeat(40);
const C2 = 'b'.repeat(40);
const MAIN = { type: 'branch', name: 'main', full_name: 'refs/heads/main', commit: C1 };
const DEV = { type: 'branch', name: 'dev', full_name: 'refs/heads/dev', commit: C2 };

export const REFS = {
  default_branch: 'main',
  truncated: false,
  refs: [
    { name: 'main', full_name: 'refs/heads/main', type: 'branch', target: C1, commit: C1 },
    { name: 'dev', full_name: 'refs/heads/dev', type: 'branch', target: C2, commit: C2 },
    { name: 'v1', full_name: 'refs/tags/v1', type: 'tag', target: C1, commit: C1 },
  ],
};

const entry = (name, path, type, size) => ({
  name,
  path,
  type,
  mode: type === 'tree' ? '040000' : '100644',
  oid: 'e'.repeat(40),
  size,
});

const README = [
  '# Press Line Spec',
  '',
  'A synthetic readme with a [guide](docs/guide.md), an [outside link](https://example.com/x)',
  'and an image: ![logo](logo.png)',
  '',
  '## Parts',
  '',
  '| part | note |',
  '|------|------|',
  '| frame | steel |',
  '',
  '```rust',
  'fn main() {}',
  '```',
  '',
  '<script>window.__pwned = 1</script>',
  '[bad](javascript:alert(1))',
].join('\n');

const LIB = ['pub fn add(a: i32, b: i32) -> i32 {', '    // sum', '    a + b', '}', ''].join('\n');

const blob = (path, content, over = {}) => ({
  ref: MAIN,
  path,
  name: path.split('/').pop(),
  type: 'blob',
  mode: '100644',
  oid: 'e'.repeat(40),
  size: content === null ? 4096 : content.length,
  binary: content === null,
  content,
  truncated: false,
  ...over,
});

const commit = (oid, subject, parents) => ({
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

const PATCH = [
  '@@ -1,3 +1,4 @@',
  ' pub fn add(a: i32, b: i32) -> i32 {',
  '-    a - b',
  '+    // sum',
  '+    a + b',
  ' }',
].join('\n');

const file = (over) => ({
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

const DIFF = {
  truncated: true,
  files: [
    file({}),
    file({ old_path: 'data.bin', new_path: 'data.bin', binary: true, patch: null }),
    file({ old_path: 'big.sql', new_path: 'big.sql', patch: null, omitted: 'too_large' }),
  ],
};

const notFound = { status: 404, error: ['指定された場所が見つかりません。', 'req-repo-404', 'FG-REPO-0023'] };

/** `rest` is the path after `/repository/`; returns `{ body }` or `{ status, error }`. */
export function browse(rest, url) {
  const q = (k) => url.searchParams.get(k) ?? '';
  switch (rest) {
    case 'refs':
      return { body: REFS };
    case 'resolve': {
      const [ref = '', ...tail] = q('spec').split('/');
      if (ref !== 'main' && ref !== 'dev') return notFound;
      return { body: { ref: ref === 'main' ? MAIN : DEV, path: tail.join('/') } };
    }
    case 'tree': {
      const path = q('path');
      if (path === '') {
        return {
          body: {
            ref: MAIN,
            path: '',
            oid: 'f'.repeat(40),
            truncated: false,
            entries: [
              entry('README.md', 'README.md', 'blob', 400),
              entry('src', 'src', 'tree', null),
              entry('logo.png', 'logo.png', 'blob', 70),
            ],
          },
        };
      }
      if (path === 'src') {
        return {
          body: {
            ref: MAIN,
            path,
            oid: 'f'.repeat(40),
            truncated: false,
            entries: [entry('lib.rs', 'src/lib.rs', 'blob', LIB.length)],
          },
        };
      }
      return notFound;
    }
    case 'blob': {
      const path = q('path');
      if (path === 'README.md') return { body: blob(path, README) };
      if (path === 'src/lib.rs') return { body: blob(path, LIB) };
      return notFound;
    }
    case 'commits':
      return {
        body: {
          ref: MAIN,
          head: C1,
          path: q('path'),
          commits: [commit(C1, 'Add the add function', [C2]), commit(C2, 'Initial commit', [])],
          next_cursor: null,
          has_more: false,
        },
      };
    case `commits/${C1}`:
      return {
        body: {
          commit: commit(C1, 'Add the add function', [C2]),
          message: 'Add the add function\n\nSynthetic body line.',
          diff: DIFF,
        },
      };
    case 'blame':
      return {
        body: {
          ref: MAIN,
          path: 'src/lib.rs',
          oid: 'e'.repeat(40),
          size: LIB.length,
          binary: false,
          truncated: false,
          commits: [
            { oid: C2, author_name: 'Taro Yamada', author_email: 'taro@example.invalid', author_time: 1790000000, author_tz: '+0900', summary: 'Initial commit', boundary: true, path: 'src/lib.rs' },
            { oid: C1, author_name: 'Hanako Sato', author_email: 'hanako@example.invalid', author_time: 1790100000, author_tz: '+0900', summary: 'Add the add function', boundary: false, path: 'src/lib.rs' },
          ],
          hunks: [
            { commit: C2, start: 1, orig_start: 1, lines: ['pub fn add(a: i32, b: i32) -> i32 {'] },
            { commit: C1, start: 2, orig_start: 2, lines: ['    // sum', '    a + b'] },
            { commit: C2, start: 4, orig_start: 3, lines: ['}'] },
          ],
        },
      };
    case 'compare':
      return {
        body: {
          from: MAIN,
          to: DEV,
          commits: [commit(C1, 'Add the add function', [C2])],
          commits_truncated: false,
          diff: DIFF,
        },
      };
    default:
      return null;
  }
}

/** A 1x1 transparent PNG (the README's image), served with the raw route's headers. */
export const PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==',
  'base64',
);
