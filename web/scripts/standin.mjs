// A stand-in for `forge serve` that serves web/dist the way forge-web does
// (ADR-0022 §2 / §3): the same shell bytes for every page path,
// /-/assets/{file} for the build, the shell CSP read from
// crates/forge-web/src/security_headers.rs (one source for the policy).
// The API behind it is a function the caller supplies: smoke.mjs answers
// "signed out", journey.mjs a small stateful fake.
import { existsSync, readFileSync } from 'node:fs';
import { createServer } from 'node:http';
import { join } from 'node:path';

const root = new URL('..', import.meta.url).pathname;
export const dist = join(root, 'dist');

export function shellCsp() {
  const src = readFileSync(join(root, '../crates/forge-web/src/security_headers.rs'), 'utf8');
  const m = src.match(/const CSP_SHELL_BASE: &str = "([\s\S]*?)";/);
  if (!m) throw new Error('CSP_SHELL_BASE not found in security_headers.rs');
  return m[1].replace(/\\\n\s*/g, '');
}

const TYPES = {
  js: 'text/javascript; charset=utf-8',
  css: 'text/css; charset=utf-8',
  svg: 'image/svg+xml',
  png: 'image/png',
  json: 'application/json',
};

const COMMON = {
  'X-Content-Type-Options': 'nosniff',
  'Referrer-Policy': 'same-origin',
  'Cross-Origin-Opener-Policy': 'same-origin',
};

/** `handle(req, res, url, body)` answers /api/ and /-/auth/ requests and
 * returns true when it did. `body` is the request body as text. */
export function standIn(csp, handle) {
  const index = readFileSync(join(dist, 'index.html'));
  return createServer((req, res) => {
    const chunks = [];
    req.on('data', (c) => chunks.push(c));
    req.on('end', () => {
      const url = new URL(req.url ?? '/', 'http://x');
      const path = url.pathname;
      if (path.startsWith('/api/') || path.startsWith('/-/auth/')) {
        if (handle(req, res, url, Buffer.concat(chunks).toString('utf8'))) return;
        res.writeHead(404, COMMON);
        res.end();
        return;
      }
      if (path.startsWith('/-/assets/')) {
        const name = path.slice('/-/assets/'.length);
        const file = join(dist, name);
        const ext = name.slice(name.lastIndexOf('.') + 1);
        if (name.includes('/') || name === 'index.html' || !TYPES[ext] || !existsSync(file)) {
          res.writeHead(404, COMMON);
          res.end();
          return;
        }
        res.writeHead(200, { ...COMMON, 'Content-Type': TYPES[ext], 'Content-Security-Policy': csp });
        res.end(readFileSync(file));
        return;
      }
      res.writeHead(200, {
        ...COMMON,
        'Content-Type': 'text/html; charset=utf-8',
        'Content-Security-Policy': csp,
        'Cache-Control': 'no-cache',
      });
      res.end(index);
    });
  });
}

/** JSON answer with the API's CSP. */
export function sendJson(res, status, body, extra = {}) {
  res.writeHead(status, {
    ...COMMON,
    'Content-Type': 'application/json',
    'Content-Security-Policy': "default-src 'none'; frame-ancestors 'none'",
    'Cache-Control': 'no-store',
    ...extra,
  });
  res.end(JSON.stringify(body));
}

export function sendError(res, status, userMessage, requestId, code = 'FG-TEST-0001') {
  sendJson(res, status, { error_code: code, user_message: userMessage, request_id: requestId });
}

export function redirect(res, status, location, extra = {}) {
  res.writeHead(status, { ...COMMON, Location: location, 'Cache-Control': 'no-store', ...extra });
  res.end();
}

export async function listen(server) {
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  return `http://127.0.0.1:${server.address().port}`;
}
