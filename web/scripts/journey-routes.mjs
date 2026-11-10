// Which API routes the UI calls, and whether the journey's fake answers them.
// The UI's calls are read from the source (`api.GET('/api/v1/…'` in src/,
// tests and the generated types left out), so a screen that starts calling a
// new route fails here by name instead of as a 500 deep inside a journey step.
// Only GET is probed: reads leave the stateful fakes as they are, and the
// probe runs after the browser journey, so a read it makes cannot move a step.
import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const src = fileURLToPath(new URL('../src', import.meta.url));

function* sources(dir) {
  for (const e of readdirSync(dir, { withFileTypes: true })) {
    const p = join(dir, e.name);
    if (e.isDirectory()) yield* sources(p);
    else if (/\.tsx?$/.test(e.name) && !/\.(test|spec)\.tsx?$/.test(e.name) && e.name !== 'schema.gen.ts') yield p;
  }
}

/** `[method, template]` pairs the UI calls, sorted and unique. */
export function uiRoutes() {
  const found = new Set();
  for (const file of sources(src)) {
    for (const m of readFileSync(file, 'utf8').matchAll(/\bapi\.(GET|POST|PUT|PATCH|DELETE)\(\s*'([^']+)'/g)) {
      found.add(`${m[1]} ${m[2]}`);
    }
  }
  return [...found].sort().map((k) => k.split(' '));
}

/** A path parameter's value: the ids, numbers and commits the fakes hold. */
const PARAMS = {
  sha: 'b'.repeat(40),
  oid: 'b'.repeat(40),
  iid: '1',
  version: '1',
  public_id: '0123456789abcdef',
  name: 'default',
  integration_id: '4',
  doc_id: 'SPEC-PUMP-001',
};

/** Where a route's fake holds other values than PARAMS (method and template). */
const OVERRIDES = {
  // The merge attempt is !5's (journey-merge.mjs); the commit view is C1's (journey-repo.mjs).
  'GET /api/v1/projects/{id}/merge-requests/{iid}/merge': { iid: '5' },
  'GET /api/v1/projects/{id}/repository/commits/{oid}': { oid: 'a'.repeat(40) },
};

function concrete(method, template) {
  const own = OVERRIDES[`${method} ${template}`] ?? {};
  return template.replace(/\{(\w+)\}/g, (_, name, at) => {
    if (name in own) return own[name];
    if (name in PARAMS) return PARAMS[name];
    // `{id}` is the object the path segment before it names.
    const before = template.slice(0, at);
    if (before.endsWith('/projects/')) return '7';
    if (before.endsWith('/groups/')) return '3';
    if (before.endsWith('/integrations/')) return '4';
    return '1';
  });
}

/** Sends every UI GET through `handle` (signed in) and returns the ones it did
 * not expect. `unexpected` is the fake's list of requests it fell through on. */
export function probeGets(handle, unexpected, cookie) {
  const missing = [];
  for (const [method, template] of uiRoutes()) {
    if (method !== 'GET') continue;
    const url = new URL(concrete(method, template), 'http://x');
    const before = unexpected.length;
    const res = {
      writeHead() {
        return this;
      },
      setHeader() {},
      end() {},
    };
    handle({ method, headers: { cookie }, url: url.pathname }, res, url, '');
    if (unexpected.length > before) {
      unexpected.length = before;
      missing.push(`${method} ${template}`);
    }
  }
  return missing;
}
