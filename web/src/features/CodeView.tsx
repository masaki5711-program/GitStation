// A file's text as numbered lines in a table: the line number is a row header
// and a link to that line (`#L12`, reachable by keyboard, named "Line 12"),
// not selectable text, so copying the code never copies the numbers. Colours
// come from classes (`hljs-*`). The scroll area is a focusable region.
import type { ElementContent } from 'hast';
import { useEffect, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';

import controls from '../components/controls.module.css';
import { highlightLines, plainLines, type Line } from './highlight';
import styles from './CodeView.module.css';

function renderNode(node: ElementContent, key: number): ReactNode {
  if (node.type === 'text') return node.value;
  if (node.type !== 'element') return null;
  const classes = Array.isArray(node.properties.className)
    ? node.properties.className.filter(
        (c): c is string => typeof c === 'string' && /^hljs-[a-z_-]+$/.test(c),
      )
    : [];
  return (
    <span key={key} className={classes.join(' ')}>
      {node.children.map(renderNode)}
    </span>
  );
}

/** The highlighted lines of a text; `null` until they are ready, when the
 * file has no grammar, or when loading the grammar failed (the caller then
 * shows the current text as plain lines). The result belongs to the exact
 * path and text it was made for: a new text never shows the old colours. A
 * failure is logged once, as unexpected failures of the page are. */
export function useHighlighted(path: string, text: string): Line[] | null {
  const [state, setState] = useState<{ path: string; text: string; lines: Line[] | null } | null>(
    null,
  );
  useEffect(() => {
    let cancelled = false;
    highlightLines(path, text).then(
      (lines) => {
        if (!cancelled) setState({ path, text, lines });
      },
      (error: unknown) => {
        console.error('gitstation: syntax colouring failed', error);
        if (!cancelled) setState({ path, text, lines: null });
      },
    );
    return () => {
      cancelled = true;
    };
  }, [path, text]);
  return state !== null && state.path === path && state.text === text ? state.lines : null;
}

/** Lines in the page at first, and added by each "Show more" (a 20 000-line
 * file is 20 000 rows with a link each). A `#L…` address opens its line. */
export const CODE_WINDOW = 3000;

export function CodeView({ path, text, label }: { path: string; text: string; label?: string }) {
  const { t } = useTranslation();
  const highlighted = useHighlighted(path, text);
  const all: ReactNode[] =
    highlighted !== null ? highlighted.map((line) => line.map(renderNode)) : plainLines(text);
  const [limit, setLimit] = useState(CODE_WINDOW);
  useEffect(() => {
    const line = /^#L(\d+)$/.exec(window.location.hash);
    const wanted = line === null ? 0 : Number(line[1]);
    if (wanted > limit) setLimit(wanted + CODE_WINDOW);
  }, [limit]);
  const rows = all.slice(0, limit);
  return (
    <>
    <div
      className={styles.scroll}
      role="region"
      aria-label={label ?? t('file.contentLabel')}
      tabIndex={0}
      data-testid="code"
      data-lines={all.length}
    >
      <table className={`${styles.code} ${styles.tokens}`}>
        <tbody>
          {rows.map((content, i) => (
            <tr key={i} id={`L${i + 1}`} className={styles.line}>
              <th scope="row" className={styles.number}>
                <a href={`#L${i + 1}`} aria-label={t('file.lineLabel', { n: i + 1 })}>
                  {i + 1}
                </a>
              </th>
              <td className={styles.source}>{content}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
    {rows.length < all.length && (
      <p>
        <span role="status" className={styles.hint}>
          {t('blame.showing', { shown: rows.length, total: all.length })}
        </span>{' '}
        <button
          type="button"
          className={controls.button}
          onClick={() => {
            setLimit(rows.length + CODE_WINDOW);
          }}
        >
          {t('blame.more', { count: all.length - rows.length })}
        </button>
      </p>
    )}
    </>
  );
}

/** Lines of a text for tables that need a cell per line (blame). */
export function useLineNodes(path: string, lines: string[]): ReactNode[] {
  const text = lines.join('\n');
  const highlighted = useHighlighted(path, text);
  if (highlighted === null || highlighted.length !== lines.length) return lines;
  return highlighted.map((line) => line.map(renderNode));
}
export const tokensClass = styles.tokens;
