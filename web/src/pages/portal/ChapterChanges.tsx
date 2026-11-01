// What changed in one chapter between two versions. Added and removed lines
// are told apart by more than colour: a sign (+ / −) that is text, words for
// a screen reader ("Added line"), an underline for what was added and a
// strike-through for what was removed. Lines around a change are shown dim
// for orientation; where lines were left out a marked gap says so.
import { useMemo } from 'react';
import { useTranslation } from 'react-i18next';

import type { DiffFile } from '../../api/types';
import { parsePatch } from '../../features/DiffView';
import type { ChangeKind } from '../../features/portal';
import styles from './Portal.module.css';

/** A chapter longer than this shows its first rows only; the whole chapter
 * can be read, or downloaded, without the comparison. */
export const MAX_ROWS = 2000;

const PLUS = '+';
const MINUS = '\u2212';

export function ChapterChanges({ kind, file }: { kind: ChangeKind; file: DiffFile | null }) {
  const { t } = useTranslation();
  const rows = useMemo(
    () => (file?.patch === null || file?.patch === undefined ? [] : parsePatch(file.patch)),
    [file],
  );
  if (kind === 'same' || file === null) return <p>{t('portal.compare.unchanged')}</p>;

  let note: string | null = null;
  if (file.binary || file.omitted === 'not_utf8') note = t('portal.compare.noText');
  else if (file.omitted !== null) note = t('portal.compare.tooLarge');
  else if (file.patch === '') note = t('portal.compare.nameOnly');

  const shown = rows.slice(0, MAX_ROWS);
  return (
    <>
      {kind === 'added' && <p role="status">{t('portal.compare.chapterAdded')}</p>}
      {kind === 'removed' && <p role="status">{t('portal.compare.chapterRemoved')}</p>}
      {note !== null && (
        <p className={styles.note} role="status">
          {note}
        </p>
      )}
      {shown.length > 0 && (
        <div className={styles.changes} role="region" aria-label={t('portal.compare.linesLabel')} tabIndex={0}>
          <table className={styles.changeTable}>
            <tbody>
              {shown.map((row, i) => {
                if (row.kind === 'hunk') {
                  return i === 0 ? null : (
                    <tr key={i} className={styles.gap}>
                      <td colSpan={2}>
                        <span aria-hidden="true">…</span>
                        <span className={styles.srOnly}>{t('portal.compare.gap')}</span>
                      </td>
                    </tr>
                  );
                }
                if (row.kind === 'note') return null;
                const cls =
                  row.kind === 'add' ? styles.added : row.kind === 'del' ? styles.removed : styles.context;
                return (
                  <tr key={i} className={cls}>
                    <td className={styles.mark}>
                      {row.kind === 'add' && (
                        <>
                          <span aria-hidden="true">{PLUS}</span>
                          <span className={styles.srOnly}>{t('portal.compare.addedLine')}</span>
                        </>
                      )}
                      {row.kind === 'del' && (
                        <>
                          <span aria-hidden="true">{MINUS}</span>
                          <span className={styles.srOnly}>{t('portal.compare.removedLine')}</span>
                        </>
                      )}
                    </td>
                    <td className={styles.line}>{row.text === '' ? ' ' : row.text}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      {rows.length > MAX_ROWS && (
        <p className={styles.note} role="status">
          {t('portal.compare.rowsCut', { count: MAX_ROWS })}
        </p>
      )}
    </>
  );
}
