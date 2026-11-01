// A file's text as numbered lines. Colours come from classes (`hljs-*`), the
// numbers from a CSS counter (so copying the code never copies them); a line
// has an id (`L12`) a link can point at. The scroll area is a focusable
// region (keyboard users can scroll it).
import type { ElementContent } from 'hast';
import { useEffect, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';

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

/** The highlighted lines of a text; `null` until they are ready or when the
 * file has no grammar (the caller then shows plain lines). A failure to load
 * a grammar is logged once and the file stays plain. */
export function useHighlighted(path: string, text: string): Line[] | null {
  const [state, setState] = useState<{ key: string; lines: Line[] | null }>({
    key: '',
    lines: null,
  });
  const key = `${path}\0${text.length}`;
  useEffect(() => {
    let cancelled = false;
    highlightLines(path, text).then(
      (lines) => {
        if (!cancelled) setState({ key, lines });
      },
      (error: unknown) => {
        console.error('gitstation: syntax colouring failed', error);
      },
    );
    return () => {
      cancelled = true;
    };
  }, [path, text, key]);
  return state.key === key ? state.lines : null;
}

export function CodeView({ path, text, label }: { path: string; text: string; label?: string }) {
  const { t } = useTranslation();
  const highlighted = useHighlighted(path, text);
  const plain = highlighted === null ? plainLines(text) : null;
  const count = highlighted?.length ?? plain?.length ?? 0;
  return (
    <div
      className={styles.scroll}
      role="region"
      aria-label={label ?? t('file.contentLabel')}
      tabIndex={0}
      data-testid="code"
      data-lines={count}
    >
      <pre className={`${styles.code} ${styles.tokens}`}>
        <code>
          {highlighted !== null
            ? highlighted.map((line, i) => (
                <span key={i} id={`L${i + 1}`} className={styles.line}>
                  {line.map(renderNode)}
                </span>
              ))
            : (plain ?? []).map((line, i) => (
                <span key={i} id={`L${i + 1}`} className={styles.line}>
                  {line}
                </span>
              ))}
        </code>
      </pre>
    </div>
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
