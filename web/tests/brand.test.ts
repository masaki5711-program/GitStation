// The logo and icon are copies of docs/brand/ (ADR-0022 §4: never redrawn);
// a drifted copy fails here.
import { createHash } from 'node:crypto';
import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { describe, expect, it } from 'vitest';

const copies = join(import.meta.dirname, '../src/assets/brand');
const source = join(import.meta.dirname, '../../docs/brand');

const sha256 = (path: string) => createHash('sha256').update(readFileSync(path)).digest('hex');

describe('brand assets', () => {
  const files = readdirSync(copies).filter((f) => f.endsWith('.svg'));

  it('copies at least the icon and the header logo', () => {
    expect(files).toEqual(
      expect.arrayContaining(['gitstation-v1-icon.svg', 'gitstation-v1-logo-dark.svg']),
    );
  });

  it.each(files)('%s is byte-identical to docs/brand/', (name) => {
    expect(sha256(join(copies, name))).toBe(sha256(join(source, name)));
  });
});
