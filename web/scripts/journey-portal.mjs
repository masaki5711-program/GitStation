// Synthetic specification for the browser journey (project 8, `acme/press-spec`):
// the same shapes as tests/portal-fixtures.ts (change one, change the other).
// A working version and two confirmed versions, chapters under docs/specs, and
// the comparison between the confirmed versions.
const A = 'a'.repeat(40);
const B = 'b'.repeat(40);
const C = 'c'.repeat(40);

export const PRESS_SPEC = {
  id: 8,
  namespace_id: 11,
  path: 'acme/press-spec',
  name: 'press-spec',
  display_name: 'Press Spec',
  description: 'The press line specification',
  visibility: 'internal',
  kind: 'spec',
  created_at: '2026-10-01T00:00:00Z',
  role: 'developer',
};

const tag = (name, commit) => ({ name, full_name: `refs/tags/${name}`, type: 'tag', target: commit, commit });
export const SPEC_REFS = {
  default_branch: 'main',
  truncated: false,
  refs: [
    { name: 'main', full_name: 'refs/heads/main', type: 'branch', target: A, commit: A },
    tag('rev-1', B),
    tag('rev-2', C),
  ],
};

const rev = (commit) => ({ type: 'commit', name: commit, full_name: null, commit });
const entry = (name) => ({
  name,
  path: `docs/specs/${name}`,
  type: 'blob',
  mode: '100644',
  oid: 'e'.repeat(40),
  size: 100,
});
const CHAPTERS = {
  [A]: ['01-scope.md', '02-parts.md', '03-notes.md'],
  [B]: ['01-scope.md'],
  [C]: ['01-scope.md', '02-parts.md'],
};
const MOMENTS = {
  [A]: ['Working draft of the next issue', '2026-10-31T09:00:00+09:00'],
  [B]: ['First issue for approval', '2026-10-01T09:00:00+09:00'],
  [C]: ['Second issue with the parts list', '2026-10-20T09:00:00+09:00'],
};
const SCOPE = [
  '# Scope of the press line',
  '',
  'See the [parts list](02-parts.md) and an [outside note](https://example.com/x).',
  '',
  '| item | value |',
  '|------|-------|',
  '| load | 40 t |',
].join('\n');

const file = (over) => ({
  status: 'modified',
  old_path: 'docs/specs/01-scope.md',
  new_path: 'docs/specs/01-scope.md',
  old_mode: '100644',
  new_mode: '100644',
  old_oid: '1'.repeat(40),
  new_oid: '2'.repeat(40),
  similarity: null,
  binary: false,
  patch: ['@@ -1,3 +1,3 @@', ' # Scope of the press line', '-The press load is 30 t.', '+The press load is 40 t.', ' '].join('\n'),
  omitted: null,
  ...over,
});

const notFound = { status: 404, error: ['指定された場所が見つかりません。', 'req-spec-404', 'FG-REPO-0023'] };

/** `rest` is the path after `/projects/8/repository/`. */
export function browseSpec(rest, url) {
  const q = (k) => url.searchParams.get(k) ?? '';
  switch (rest) {
    case 'refs':
      return { body: SPEC_REFS };
    case 'commits': {
      const m = MOMENTS[q('ref')];
      if (m === undefined) return notFound;
      return {
        body: {
          ref: rev(q('ref')),
          head: q('ref'),
          path: '',
          commits: [
            {
              oid: q('ref'),
              parents: [],
              author_name: 'Taro Yamada',
              author_email: 'taro@example.invalid',
              authored_at: m[1],
              committer_name: 'Taro Yamada',
              committer_email: 'taro@example.invalid',
              committed_at: m[1],
              subject: m[0],
            },
          ],
          next_cursor: null,
          has_more: false,
        },
      };
    }
    case 'tree': {
      const names = CHAPTERS[q('ref')];
      if (names === undefined || q('path') !== 'docs/specs') return notFound;
      return {
        body: { ref: rev(q('ref')), path: 'docs/specs', oid: 'f'.repeat(40), truncated: false, entries: names.map(entry) },
      };
    }
    case 'blob': {
      const path = q('path');
      const name = path.split('/').pop();
      const content = name === '01-scope.md' ? SCOPE : `# ${name}\n\nSynthetic text of ${name}.`;
      return {
        body: {
          ref: rev(q('ref')), path, name, type: 'blob', mode: '100644', oid: 'e'.repeat(40),
          size: content.length, binary: false, content, truncated: false,
        },
      };
    }
    case 'compare':
      return {
        body: {
          from: rev(q('from')),
          to: rev(q('to')),
          commits: [],
          commits_truncated: false,
          diff: {
            truncated: false,
            files: [file({}), file({ status: 'added', old_path: null, new_path: 'docs/specs/02-parts.md', patch: '@@ -0,0 +1,2 @@\n+# Parts\n+frame, ram' })],
          },
        },
      };
    default:
      return null;
  }
}


