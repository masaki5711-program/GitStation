// The ja split (scripts/i18n-split.mjs): the main chunk carries only the ja
// strings the first load uses; the lazy screens wait for the rest.
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { I18nextProvider, useTranslation } from 'react-i18next';
import { afterEach, describe, expect, it, vi } from 'vitest';
import jaCore from 'virtual:i18n/ja-core';
import jaRest from 'virtual:i18n/ja-rest';

import {
  assertReadable,
  eagerFiles,
  keysInFiles,
  keysInSource,
  splitDictionary,
  splitJa,
  type Dict,
} from '../scripts/i18n-split.mjs';
import ja from '../src/i18n/ja.json';
import { createI18n, hasJaRest, loadJaRest, switchLanguage } from '../src/i18n';
import { loaders } from '../src/pages/screenLoaders';

const SRC = join(import.meta.dirname, '../src');
const PLURAL = /_(zero|one|two|few|many|other)$/;

function flatten(dict: Dict, prefix = ''): Map<string, string> {
  const out = new Map<string, string>();
  for (const [k, v] of Object.entries(dict)) {
    const key = prefix === '' ? k : `${prefix}.${k}`;
    if (typeof v === 'string') out.set(key, v);
    else for (const [kk, vv] of flatten(v, key)) out.set(kk, vv);
  }
  return out;
}

describe('splitDictionary', () => {
  it('puts the used keys and their plural forms in core, the rest in rest, nested as before', () => {
    const dict = {
      a: { title: 'A', count_one: '1', count_other: 'n', deep: { x: 'X' } },
      b: { only: 'B' },
    };
    const { core, rest } = splitDictionary(dict, new Set(['a.title', 'a.count']));
    expect(core).toEqual({ a: { title: 'A', count_one: '1', count_other: 'n' } });
    expect(rest).toEqual({ a: { deep: { x: 'X' } }, b: { only: 'B' } });
  });

  it('finds t() keys the way the dictionary gate does', () => {
    expect([...keysInSource(`t('a.b'); i18n.t( 'c.d_e', {}); format('x.y'); t(\`z.\${w}\`)`)]).toEqual([
      'a.b',
      'c.d_e',
    ]);
  });
});

describe('eagerFiles', () => {
  let root = '';
  afterEach(() => {
    if (root !== '') rmSync(root, { recursive: true, force: true });
  });

  function tree(files: Record<string, string>) {
    root = mkdtempSync(join(tmpdir(), 'i18n-split-'));
    for (const [name, body] of Object.entries(files)) {
      mkdirSync(dirname(join(root, name)), { recursive: true });
      writeFileSync(join(root, name), body);
    }
    return (name: string) => join(root, name);
  }

  it('follows static and other dynamic imports, but not the lazy screen loaders', () => {
    const p = tree({
      'main.tsx': `import './c';\nimport { A } from './a';\nimport './style.css';\nexport * from './b';\nconst m = () => import('./md');`,
      'a.tsx': `import type {\n  T,\n} from './types';\nimport { loaders } from './pages/screenLoaders';\nt('eager.a');`,
      'b/index.ts': `t('eager.b');`,
      'types.ts': `export type T = 1;`,
      'md.tsx': `t('eager.md');`,
      'pages/screenLoaders.ts': `export const loaders = { s: () => import('./Settings'), u: () => import('./Shared') };`,
      'pages/Settings.tsx': `import { x } from './OnlyLazy';\nt('lazy.settings');`,
      'pages/OnlyLazy.ts': `t('lazy.only');`,
      'pages/Shared.tsx': `t('shared.key');`,
      // A lazy screen that an eager file also imports is eager.
      'c.tsx': `import { S } from './pages/Shared';`,
    });
    const { eager, lazyRoots } = eagerFiles(p('main.tsx'), p('pages/screenLoaders.ts'));
    expect([...lazyRoots].sort()).toEqual([p('pages/Settings.tsx'), p('pages/Shared.tsx')].sort());
    expect(eager.has(p('pages/Settings.tsx'))).toBe(false);
    expect(eager.has(p('pages/OnlyLazy.ts'))).toBe(false);
    expect(eager.has(p('pages/Shared.tsx'))).toBe(true);
    expect([...keysInFiles(eager)].sort()).toEqual(['eager.a', 'eager.b', 'eager.md', 'shared.key']);
  });

  it('fails on a specifier or key it cannot read instead of leaving it out of core', () => {
    for (const body of [
      `const m = () => import("./md");`,
      'const m = () => import(`./md`);',
      `import { A } from "./a";`,
      `import {\n  A,\n} from "./a";`,
      `import "./a";`,
      `t("eager.a");`,
      't(`eager.${x}`);',
    ]) {
      const p = tree({ 'main.tsx': `// ok\n${body}`, 'pages/screenLoaders.ts': '' });
      expect(() => eagerFiles(p('main.tsx'), p('pages/screenLoaders.ts'))).toThrow(/main\.tsx:[24]: write a single-quoted/);
      rmSync(root, { recursive: true, force: true });
      root = '';
    }
    expect(() => {
      assertReadable('x.ts', `import { a } from './a';\n// Arriving from "next" on a tab\nconst s = "from here";\nt('a.b');`);
    }).not.toThrow();
  });
});

describe('the ja split of this app', () => {
  const { eager, lazyRoots } = eagerFiles(join(SRC, 'main.tsx'), join(SRC, 'pages/screenLoaders.ts'));
  const eagerKeys = keysInFiles(eager);
  const core = flatten(jaCore as Dict);
  const rest = flatten(jaRest as Dict);
  const base = (k: string) => k.replace(PLURAL, '');

  it('every loader in screenLoaders.ts is seen as a lazy screen', () => {
    expect(lazyRoots.size).toBe(Object.keys(loaders).length);
  });

  it('core and rest are ja, split without overlap', () => {
    const all = flatten(ja);
    expect([...core.keys()].filter((k) => rest.has(k))).toEqual([]);
    expect(new Map([...core, ...rest])).toEqual(all);
  });

  it('every key the first load uses is in core (a raw key never shows before the rest loads)', () => {
    const missing = [...eagerKeys].filter((k) => !core.has(k) && !core.has(`${k}_other`));
    expect(missing).toEqual([]);
  });

  it('core holds nothing else: a key only the lazy screens use is in rest', () => {
    const extra = [...core.keys()].filter((k) => !eagerKeys.has(k) && !eagerKeys.has(base(k)));
    expect(extra).toEqual([]);
  });

  it('the modules the app imports are this split', () => {
    const split = splitJa(SRC);
    expect(jaCore).toEqual(split.core);
    expect(jaRest).toEqual(split.rest);
    expect(rest.size).toBeGreaterThan(0);
  });
});

describe('loading the rest', () => {
  const [restKey, restValue] = [...flatten(jaRest as Dict)][0] ?? ['', ''];

  afterEach(() => {
    vi.doUnmock('virtual:i18n/ja-rest');
    vi.resetModules();
  });

  it('a new instance has only core; loadJaRest adds the rest, seen through any useTranslation wrapper', async () => {
    const i18n = await createI18n('ja');
    expect(i18n.exists(restKey)).toBe(false);
    function Probe() {
      const { i18n: wrapper } = useTranslation();
      return <p>{String(hasJaRest(wrapper))}</p>;
    }
    await loadJaRest(i18n);
    expect(i18n.t(restKey)).toBe(restValue);
    render(
      <I18nextProvider i18n={i18n}>
        <Probe />
      </I18nextProvider>,
    );
    expect(screen.getByText('true')).toBeTruthy();
  });

  it('a lazy screen whose strings have not loaded yet never renders a raw key', async () => {
    // Fresh modules: the rest chunk is not loaded yet (setup.ts preloads it).
    vi.resetModules();
    const fresh = await import('../src/i18n');
    const { lazyScreen } = await import('../src/pages/lazyScreens');
    const i18n = await fresh.createI18n('ja');
    expect(fresh.hasJaRest(i18n)).toBe(false);
    function Uses() {
      const { t } = useTranslation();
      return <p data-testid="text">{t(restKey)}</p>;
    }
    const Screen = lazyScreen(() => Promise.resolve(Uses));
    const seen: string[] = [];
    const observer = new MutationObserver(() => {
      seen.push(document.body.textContent);
    });
    observer.observe(document.body, { subtree: true, childList: true, characterData: true });
    render(
      <I18nextProvider i18n={i18n}>
        <Screen />
      </I18nextProvider>,
    );
    expect((await screen.findByTestId('text')).textContent).toBe(restValue);
    observer.disconnect();
    expect(seen.filter((text) => text.includes(restKey))).toEqual([]);
  });

  it('a rest chunk that does not load shows the error with a retry, and the retry loads it', async () => {
    vi.spyOn(console, 'error').mockImplementation(() => undefined);
    vi.resetModules();
    vi.doMock('virtual:i18n/ja-rest', () => {
      throw new Error('Failed to fetch dynamically imported module');
    });
    const fresh = await import('../src/i18n');
    const { lazyScreen } = await import('../src/pages/lazyScreens');
    const i18n = await fresh.createI18n('ja');
    function Uses() {
      const { t } = useTranslation();
      return <p data-testid="text">{t(restKey)}</p>;
    }
    const Screen = lazyScreen(() => Promise.resolve(Uses));
    render(
      <I18nextProvider i18n={i18n}>
        <Screen />
      </I18nextProvider>,
    );
    const retry = await screen.findByRole('button', { name: i18n.t('error.retry') });
    expect(screen.queryByTestId('text')).toBeNull();
    vi.doUnmock('virtual:i18n/ja-rest');
    fireEvent.click(retry);
    await waitFor(() => {
      expect(screen.getByTestId('text').textContent).toBe(restValue);
    });
  });

  it('switching to ja after a lazy screen asked for the rest has it', async () => {
    vi.resetModules();
    const fresh = await import('../src/i18n');
    const i18n = await fresh.createI18n('en');
    void fresh.loadJaRest(i18n);
    expect(await fresh.switchLanguage(i18n, 'ja')).toBe(true);
    expect(i18n.t(restKey)).toBe(restValue);
  });

  it('switching to ja without any lazy screen does not fetch the rest', async () => {
    const i18n = await createI18n('en');
    expect(await switchLanguage(i18n, 'ja')).toBe(true);
    expect(hasJaRest(i18n)).toBe(false);
  });
});
