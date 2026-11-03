// Protected-ref patterns as the settings screens check them before sending.
/** `refs/heads/…` or `refs/tags/…`, at most 255 bytes, at most one `*`, only
 * in the last part (ADR-0008). The server has the last word. */
export function patternProblem(pattern: string): 'empty' | 'prefix' | 'length' | 'star' | null {
  if (pattern.trim() === '') return 'empty';
  if (!pattern.startsWith('refs/heads/') && !pattern.startsWith('refs/tags/')) return 'prefix';
  if (new TextEncoder().encode(pattern).length > 255) return 'length';
  const stars = pattern.split('*').length - 1;
  if (stars > 1 || (stars === 1 && pattern.slice(pattern.lastIndexOf('/') + 1).indexOf('*') < 0)) {
    return 'star';
  }
  return null;
}
