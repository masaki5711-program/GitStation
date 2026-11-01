// The shell CSP allows no inline script or style (ADR-0022 §3): the built
// index.html must carry none. Used by compress.mjs (every build) and
// smoke.mjs; tests/inline-check.test.ts covers the rules.

/** Problems in an HTML document: an inline <script> (one without `src`),
 * any <style> element, a `style=` attribute, an `on*=` handler attribute,
 * or a `javascript:` URL. */
export function inlineProblems(html) {
  const problems = [];
  for (const m of html.matchAll(/<script\b([^>]*)>/gi)) {
    if (!/\bsrc\s*=/i.test(m[1] ?? '')) problems.push('inline <script> (no src)');
  }
  if (/<style\b/i.test(html)) problems.push('<style> element');
  if (/<[^>]+\sstyle\s*=/i.test(html)) problems.push('style= attribute');
  if (/<[^>]+\son[a-z]+\s*=/i.test(html)) problems.push('on*= event handler attribute');
  if (/javascript:/i.test(html)) problems.push('javascript: URL');
  return problems;
}
