// A file's text as numbered lines in a table: the line number is a row header
// and a link to that line (`#L12`, reachable by keyboard, named "Line 12"),
// not selectable text, so copying the code never copies the numbers. Colours
// come from classes (`hljs-*`). The scroll area is a focusable region.
import type { ElementContent } from 'hast';
import { useEffect, useRef, useState, type ReactNode } from 'react';
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

/** The window belongs to one file at one commit: a new path or commit starts
 * again from the first window (the `key`). */
export function CodeView({
  path,
  text,
  label,
  commit,
}: {
  path: string;
  text: string;
  label?: string;
  /** The commit the text is of (resets the window when it changes). */
  commit?: string;
}) {
  return <CodeWindow key={`${path}\0${commit ?? ''}`} path={path} text={text} label={label} />;
}

function lineOfHash(): number {
  const line = /^#L(\d+)$/.exec(window.location.hash);
  return line === null ? 0 : Number(line[1]);
}

function CodeWindow({ path, text, label }: { path: string; text: string; label: string | undefined }) {
  const { t } = useTranslation();
  const highlighted = useHighlighted(path, text);
  const plain = highlighted === null ? plainLines(text) : null;
  const total = highlighted !== null ? highlighted.length : (plain?.length ?? 0);
  const [limit, setLimit] = useState(CODE_WINDOW);
  // Rows added by the last "Show more", for the focus move below.
  const [focusRow, setFocusRow] = useState<number | null>(null);
  const [goto, setGoto] = useState('');
  const regionRef = useRef<HTMLDivElement>(null);
  const [target, setTarget] = useState(lineOfHash);
  useEffect(() => {
    const onHash = () => {
      setTarget(lineOfHash());
    };
    window.addEventListener('hashchange', onHash);
    return () => {
      window.removeEventListener('hashchange', onHash);
    };
  }, []);
  // A `#L…` address opens its line (the window grows to it) ...
  useEffect(() => {
    if (target > limit && target <= total) setLimit(target + CODE_WINDOW);
  }, [target, limit, total]);
  // ... and scrolls it into view once the row is in the page.
  const shown = Math.min(limit, total);
  useEffect(() => {
    if (target > 0 && target <= shown) {
      document.getElementById(`L${target}`)?.scrollIntoView({ block: 'center' });
    }
  }, [target, shown]);
  useEffect(() => {
    if (focusRow === null) return;
    regionRef.current?.querySelector<HTMLElement>(`#L${focusRow + 1}`)?.focus();
    setFocusRow(null);
  }, [focusRow]);
  // Slice first, then map: only the rows in the page are made into nodes.
  const rows: ReactNode[] =
    highlighted !== null
      ? highlighted.slice(0, shown).map((line) => line.map(renderNode))
      : (plain ?? []).slice(0, shown);
  const left = total - shown;
  return (
    <>
      <form
        className={styles.goto}
        onSubmit={(e) => {
          e.preventDefault();
          const n = Number(goto);
          if (Number.isInteger(n) && n >= 1 && n <= total) {
            window.location.hash = `L${n}`;
          }
        }}
      >
        <label>
          {t('file.gotoLine')}{' '}
          <input
            type="number"
            min={1}
            max={total}
            value={goto}
            className={controls.input}
            onChange={(e) => {
              setGoto(e.target.value);
            }}
          />
        </label>{' '}
        <button type="submit" className={controls.button}>
          {t('file.gotoGo')}
        </button>
      </form>
      <div
        ref={regionRef}
        className={styles.scroll}
        role="region"
        aria-label={label ?? t('file.contentLabel')}
        tabIndex={0}
        data-testid="code"
        data-lines={total}
      >
        <table className={`${styles.code} ${styles.tokens}`}>
          <tbody>
            {rows.map((content, i) => (
              <tr key={i} id={`L${i + 1}`} className={styles.line} tabIndex={-1}>
                <th scope="row" className={styles.number}>
                  {/* One entry: the "go to line" control above; the links
                      are for sharing (`#L12`) and for the pointer. */}
                  <a href={`#L${i + 1}`} tabIndex={-1} aria-label={t('file.lineLabel', { n: i + 1 })}>
                    {i + 1}
                  </a>
                </th>
                <td className={styles.source}>{content}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <p>
        <span role="status" className={styles.hint}>
          {left > 0 ? t('blame.showing', { shown, total }) : ''}
        </span>
        {left > 0 && (
          <>
            {' '}
            <button
              type="button"
              className={controls.button}
              onClick={() => {
                // The button goes away with the last rows: focus moves to the
                // first new row instead of being lost.
                if (left <= CODE_WINDOW) setFocusRow(shown);
                setLimit(shown + CODE_WINDOW);
              }}
            >
              {t('blame.more', { count: left })}
            </button>
          </>
        )}
      </p>
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
