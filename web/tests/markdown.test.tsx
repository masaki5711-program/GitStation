// Markdown must never produce active content (ADR-0022 §7, plan 10 MR10):
// whatever the source says, the output has no script, no event handler, no
// style attribute, no javascript:/data: link and no foreign image.
import { render } from '@testing-library/react';
import { I18nextProvider } from 'react-i18next';
import { MemoryRouter } from 'react-router';
import { describe, expect, it } from 'vitest';

import { Markdown, headingId, resolveRepoPath, safeUrl } from '../src/features/Markdown';
import { createI18n } from '../src/i18n';

async function draw(source: string, dir = '') {
  const i18n = await createI18n('en');
  const view = render(
    <I18nextProvider i18n={i18n}>
      <MemoryRouter>
        <Markdown source={source} project="acme/forge-web" refName="main" dir={dir} />
      </MemoryRouter>
    </I18nextProvider>,
  );
  return view.container;
}

const FORBIDDEN_TAGS = [
  'script', 'iframe', 'object', 'embed', 'style', 'form', 'link', 'meta', 'base', 'svg', 'math',
  'video', 'audio', 'source', 'frame', 'frameset', 'applet', 'template', 'noscript', 'dialog',
];

/** The whole output is inert: only tags, attributes and URLs that cannot run. */
function expectInert(container: HTMLElement) {
  for (const tag of FORBIDDEN_TAGS) {
    expect(container.querySelector(tag), `<${tag}>`).toBeNull();
  }
  for (const el of container.querySelectorAll('*')) {
    for (const attr of el.getAttributeNames()) {
      expect(attr.startsWith('on'), `${el.tagName} ${attr}`).toBe(false);
      expect(attr, `${el.tagName} style attribute`).not.toBe('style');
      expect(attr, `${el.tagName} srcdoc`).not.toBe('srcdoc');
    }
    for (const attr of ['href', 'src', 'action', 'formaction', 'xlink:href', 'data', 'poster']) {
      const value = el.getAttribute(attr);
      if (value === null) continue;
      expect(value, `${el.tagName} ${attr}`).not.toMatch(/^\s*(javascript|data|vbscript|file|blob):/i);
      expect(value.startsWith('//'), `${el.tagName} ${attr}`).toBe(false);
    }
  }
}

describe('Markdown: raw HTML never becomes active content', () => {
  const vectors: [string, string][] = [
    ['script block', '<script>window.pwned = 1</script>\n\ntext'],
    ['inline script', 'a <script>alert(1)</script> b'],
    ['img onerror', 'x <img src=x onerror=alert(1)> y'],
    ['svg onload', '<svg onload=alert(1)><circle/></svg>'],
    ['raw anchor', '<a href="javascript:alert(1)">click</a>'],
    ['div onclick block', '<div onclick="alert(1)">hi</div>\n\nmore'],
    ['iframe', '<iframe src="https://evil.example/"></iframe>'],
    ['iframe srcdoc', '<iframe srcdoc="<script>alert(1)</script>"></iframe>'],
    ['style block', '<style>body{display:none}</style>'],
    ['style attribute', '<p style="position:fixed;inset:0">overlay</p>'],
    ['form', '<form action="https://evil.example/"><input name=p><button>go</button></form>'],
    ['meta refresh', '<meta http-equiv="refresh" content="0;url=https://evil.example/">'],
    ['base', '<base href="https://evil.example/">'],
    ['object', '<object data="javascript:alert(1)"></object>'],
    ['math', '<math><mtext><script>alert(1)</script></mtext></math>'],
    ['html comment with script', '<!-- --><script>alert(1)</script>'],
    ['details ontoggle', '<details open ontoggle=alert(1)>x</details>'],
    ['entity-encoded script', '&lt;script&gt;alert(1)&lt;/script&gt;'],
  ];
  it.each(vectors)('%s', async (_name, source) => {
    const container = await draw(source);
    expectInert(container);
    expect((window as unknown as { pwned?: number }).pwned).toBeUndefined();
  });

  it('a raw HTML block is dropped, not shown as text either', async () => {
    const container = await draw('<script>alert(1)</script>\n\nafter');
    expect(container.textContent).toContain('after');
    expect(container.innerHTML).not.toContain('<script');
  });

  it('entity-encoded markup stays text', async () => {
    const container = await draw('&lt;script&gt;alert(1)&lt;/script&gt;');
    expect(container.querySelector('script')).toBeNull();
    expect(container.textContent).toContain('<script>alert(1)</script>');
  });
});

describe('Markdown: links', () => {
  const dangerous = [
    '[x](javascript:alert(1))',
    '[x](JaVaScRiPt:alert(1))',
    '[x](  javascript:alert(1))',
    '[x](java&#10;script:alert(1))',
    '[x](java&#9;script:alert(1))',
    '[x](&#106;avascript:alert(1))',
    '[x](data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==)',
    '[x](data:text/html,<script>alert(1)</script>)',
    '[x](vbscript:msgbox(1))',
    '[x](file:///etc/passwd)',
    '[x](blob:https://example.com/x)',
    '[x](//evil.example/x)',
    '[x](\\\\evil.example\\x)',
    '[x][ref]\n\n[ref]: javascript:alert(1)',
    '<javascript:alert(1)>',
  ];
  it.each(dangerous)('%s is not a link', async (source) => {
    const container = await draw(source);
    expectInert(container);
    for (const a of container.querySelectorAll('a')) {
      expect(a.getAttribute('href') ?? '').not.toMatch(/javascript|data:|vbscript|file:|blob:|^\/\//i);
    }
  });

  it('http, https and mailto links are kept; outside ones are rel=noopener noreferrer', async () => {
    const container = await draw('[a](https://example.com/a) [b](http://example.com/b) [c](mailto:x@example.com)');
    const links = [...container.querySelectorAll('a')];
    expect(links.map((a) => a.getAttribute('href'))).toEqual([
      'https://example.com/a',
      'http://example.com/b',
      'mailto:x@example.com',
    ]);
    for (const a of links) expect(a.getAttribute('rel')).toBe('noopener noreferrer');
  });

  it('a relative link goes to the file screen of the same ref, a folder to the tree', async () => {
    const container = await draw('[g](guide.md) [up](../README.md#top) [dir](sub/) [abs](/docs/x.md)', 'docs');
    const hrefs = [...container.querySelectorAll('a')].map((a) => a.getAttribute('href'));
    expect(hrefs).toEqual([
      '/acme/forge-web/-/blob/main/docs/guide.md',
      '/acme/forge-web/-/blob/main/README.md#top',
      '/acme/forge-web/-/tree/main/docs/sub',
      '/acme/forge-web/-/blob/main/docs/x.md',
    ]);
  });

  it('a relative link that climbs out of the repository is not a link', async () => {
    const container = await draw('[x](../../etc/passwd) [y](%2e%2e/%2e%2e/x)', 'docs');
    expect(container.querySelectorAll('a')).toHaveLength(0);
  });

  it('a fragment link is mapped to the user-content ids', async () => {
    const container = await draw('# Intro Part\n\n[go](#intro-part) [again](#user-content-intro-part)');
    const h1 = container.querySelector('h1');
    expect(h1?.id).toBe('user-content-intro-part');
    const hrefs = [...container.querySelectorAll('a')].map((a) => a.getAttribute('href'));
    expect(hrefs).toEqual(['#user-content-intro-part', '#user-content-intro-part']);
  });
});

describe('Markdown: images', () => {
  it('only the repository\'s own png / jpeg / gif / webp, through the raw route', async () => {
    const container = await draw('![a](logo.png) ![b](img/p.JPG) ![c](/shots/s.webp)', 'docs');
    const srcs = [...container.querySelectorAll('img')].map((i) => i.getAttribute('src'));
    expect(srcs).toEqual([
      '/acme/forge-web/-/raw/main/docs/logo.png',
      '/acme/forge-web/-/raw/main/docs/img/p.JPG',
      '/acme/forge-web/-/raw/main/shots/s.webp',
    ]);
    expectInert(container);
  });

  const notDrawn = [
    '![x](https://evil.example/track.png)',
    '![x](http://evil.example/track.png)',
    '![x](//evil.example/track.png)',
    '![x](data:image/svg+xml;base64,PHN2ZyBvbmxvYWQ9YWxlcnQoMSk+)',
    '![x](data:image/png;base64,AAAA)',
    '![x](javascript:alert(1))',
    '![x](logo.svg)',
    '![x](page.html)',
    '![x](doc.pdf)',
    '![x](../../outside.png)',
    '![x]()',
    '[![x](https://evil.example/a.png)](https://evil.example/)',
  ];
  it.each(notDrawn)('%s draws no image', async (source) => {
    const container = await draw(source);
    expect(container.querySelectorAll('img')).toHaveLength(0);
    expectInert(container);
  });

  it('an external image becomes a plain link (rel noopener), never fetched by the page', async () => {
    const container = await draw('![chart](https://example.com/c.png)');
    const a = container.querySelector('a');
    expect(a?.getAttribute('href')).toBe('https://example.com/c.png');
    expect(a?.getAttribute('rel')).toBe('noopener noreferrer');
    expect(a?.textContent).toBe('chart');
  });
});

describe('Markdown: the rest of the syntax still works', () => {
  it('headings, emphasis, lists, tables, task lists, code, strikethrough', async () => {
    const source = [
      '# T', '', '**b** *i* ~~s~~', '', '- [x] done', '- [ ] todo', '',
      '| a | b |', '|---|---|', '| 1 | 2 |', '', '```js', 'const a = 1;', '```', '', '`inline`',
    ].join('\n');
    const container = await draw(source);
    expect(container.querySelector('strong')?.textContent).toBe('b');
    expect(container.querySelector('del')?.textContent).toBe('s');
    expect(container.querySelectorAll('input[type=checkbox]')).toHaveLength(2);
    expect(container.querySelector('input[type=checkbox]')?.hasAttribute('disabled')).toBe(true);
    expect(container.querySelector('table')).not.toBeNull();
    expect(container.querySelector('pre code')?.textContent).toContain('const a = 1;');
    // Scrolling parts are focusable regions with a name.
    for (const region of container.querySelectorAll('[role=region]')) {
      expect(region.getAttribute('tabindex')).toBe('0');
      expect(region.getAttribute('aria-label')).toBeTruthy();
    }
    expectInert(container);
  });

  it('headingOffset moves every heading down (the page has its own h1)', async () => {
    const i18n = await createI18n('en');
    const { container } = render(
      <I18nextProvider i18n={i18n}>
        <MemoryRouter>
          <Markdown source={'# a\n\n## b\n\n###### z'} project="p" refName="main" dir="" headingOffset={2} />
        </MemoryRouter>
      </I18nextProvider>,
    );
    expect([...container.querySelectorAll('h1,h2,h3,h4,h5,h6')].map((h) => h.tagName)).toEqual([
      'H3',
      'H4',
      'H6',
    ]);
  });

  it('does not add a <style> to the page', async () => {
    const before = document.querySelectorAll('style').length;
    await draw('# a\n\n```rust\nfn main() {}\n```\n');
    expect(document.querySelectorAll('style').length).toBe(before);
  });
});

describe('the inertness check itself', () => {
  it('detects active content when it is there (control)', () => {
    const bad = document.createElement('div');
    bad.innerHTML = '<img src="x" onerror="alert(1)">';
    expect(() => {
      expectInert(bad);
    }).toThrow();
    const link = document.createElement('div');
    link.innerHTML = '<a href="javascript:alert(1)">x</a>';
    expect(() => {
      expectInert(link);
    }).toThrow();
  });
});

describe('Markdown helpers', () => {
  it('safeUrl keeps http(s), mailto, fragments and relative paths only', () => {
    expect(safeUrl('https://a.example/x?y=1#z')).toBe('https://a.example/x?y=1#z');
    expect(safeUrl('MAILTO:a@example.com')).toBe('MAILTO:a@example.com');
    expect(safeUrl('#frag')).toBe('#frag');
    expect(safeUrl('a/b.md')).toBe('a/b.md');
    expect(safeUrl('javascript:1')).toBeNull();
    expect(safeUrl('data:text/html,x')).toBeNull();
    expect(safeUrl('ftp://x')).toBeNull();
    expect(safeUrl('//x.example')).toBeNull();
    expect(safeUrl('a\\b')).toBeNull();
    expect(safeUrl('a\u0000b')).toBeNull();
    expect(safeUrl('')).toBeNull();
  });

  it('resolveRepoPath normalises and refuses to leave the repository', () => {
    expect(resolveRepoPath('a/b', '../c.md')).toBe('a/c.md');
    expect(resolveRepoPath('a/b', './x/./y')).toBe('a/b/x/y');
    expect(resolveRepoPath('a/b', '/top.md')).toBe('top.md');
    expect(resolveRepoPath('', '../x')).toBeNull();
    expect(resolveRepoPath('a', '../../x')).toBeNull();
    expect(resolveRepoPath('a', 'x%2Fy')).toBeNull();
    expect(resolveRepoPath('a', '%E0%A4%A')).toBeNull();
    expect(resolveRepoPath('a', 'my%20file.md')).toBe('a/my file.md');
  });

  it('headingId prefixes and slugs, keeping non-ASCII letters', () => {
    expect(headingId(' Hello,  World ')).toBe('user-content-hello-world');
    expect(headingId('設計 メモ')).toBe('user-content-設計-メモ');
    expect(headingId('name')).toBe('user-content-name');
  });
});
