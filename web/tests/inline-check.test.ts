import { describe, expect, it } from 'vitest';

import { inlineProblems } from '../scripts/inline-check.mjs';

const check = inlineProblems;

describe('the built shell has no inline script or style (ADR-0022 §3)', () => {
  it('accepts external scripts and stylesheets', () => {
    expect(
      check(
        '<script type="module" crossorigin src="/-/assets/a.js"></script>' +
          '<link rel="stylesheet" href="/-/assets/a.css">',
      ),
    ).toEqual([]);
  });

  it.each([
    ['<script>alert(1)</script>', 'inline <script> (no src)'],
    ['<style>body{}</style>', '<style> element'],
    ['<div style="color:red"></div>', 'style= attribute'],
    ['<img src="x" onerror="x()">', 'on*= event handler attribute'],
    ['<a href="javascript:x()">', 'javascript: URL'],
  ])('refuses %s', (html, problem) => {
    expect(check(html)).toContain(problem);
  });
});
