// The loading state of a list: placeholder rows in the list's card, shaped
// like the rows to come (lines per row and their widths, a button on the
// right), so the page hardly moves when they arrive. "Loading" is a status
// (as in Loading) beside the aria-busy placeholder, not inside it: a live
// region inside a busy one is held back until the wait ends.
import { useTranslation } from 'react-i18next';

import styles from './Skeleton.module.css';

export type SkeletonLine = 'long' | 'medium' | 'short';

/** A row's shape: its lines (top to bottom) and whether a button sits on the right. */
export interface SkeletonRow {
  lines: SkeletonLine[];
  action?: boolean;
}

const DEFAULT_ROW: SkeletonRow = { lines: ['long', 'short'] };
const KEYS = ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h'] as const;
const LINE_CLASS: Record<SkeletonLine, string | undefined> = {
  long: styles.long,
  medium: styles.medium,
  short: styles.short,
};

export function SkeletonList({ rows = 3, row = DEFAULT_ROW }: { rows?: number; row?: SkeletonRow }) {
  const { t } = useTranslation();
  const keys = KEYS.slice(0, Math.max(1, Math.min(rows, KEYS.length)));
  return (
    <div>
      <p role="status" className={styles.srOnly}>
        {t('common.loading')}
      </p>
      <div className={styles.list} aria-busy="true">
        {keys.map((k) => (
          <div key={k} className={styles.row} aria-hidden="true">
            <div className={styles.lines}>
              {row.lines.map((w, i) => (
                <span key={i} className={`${styles.line ?? ''} ${LINE_CLASS[w] ?? ''}`} />
              ))}
            </div>
            {row.action === true && <span className={styles.action} />}
          </div>
        ))}
      </div>
    </div>
  );
}
