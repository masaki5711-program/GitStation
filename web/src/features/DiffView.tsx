// A diff: the files of a commit or a comparison, each with its hunks. The
// server sends a patch only when it is small enough and text; every other
// case is said in words: binary, too large, total cap, not UTF-8, and a
// diff the server cut (`truncated`). A change is shown by its sign (+ / -) as
// text as well as by colour.
import { useEffect, useId, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import type { DiffBody, DiffFile } from '../api/types';
import { blobUrl } from './repoUrls';
import styles from './DiffView.module.css';

export type DiffRow =
  | { kind: 'hunk'; text: string }
  | { kind: 'add' | 'del' | 'ctx'; oldNo: number | null; newNo: number | null; text: string }
  | { kind: 'note'; text: string };

const HUNK = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/;

/** The rows of a patch (hunks only, as the API returns it). Lines that are
 * not part of a hunk are ignored: the API never sends them. */
export function parsePatch(patch: string): DiffRow[] {
  const rows: DiffRow[] = [];
  let oldNo = 0;
  let newNo = 0;
  const lines = patch.split('\n');
  if (lines[lines.length - 1] === '') lines.pop();
  for (const line of lines) {
    const hunk = HUNK.exec(line);
    if (hunk !== null) {
      oldNo = Number(hunk[1]);
      newNo = Number(hunk[2]);
      rows.push({ kind: 'hunk', text: line });
      continue;
    }
    if (line.startsWith('\\')) {
      rows.push({ kind: 'note', text: line });
    } else if (line.startsWith('+')) {
      rows.push({ kind: 'add', oldNo: null, newNo, text: line.slice(1) });
      newNo += 1;
    } else if (line.startsWith('-')) {
      rows.push({ kind: 'del', oldNo, newNo: null, text: line.slice(1) });
      oldNo += 1;
    } else if (line.startsWith(' ')) {
      rows.push({ kind: 'ctx', oldNo, newNo, text: line.slice(1) });
      oldNo += 1;
      newNo += 1;
    }
  }
  return rows;
}

/** Past this many rows a file's patch is folded until asked for, and the
 * patches of one diff together open at most TOTAL_ROWS rows (a 500-file diff
 * must not put 100 000 rows in the page). */
export const FOLD_ROWS = 600;
export const TOTAL_ROWS = 2000;

function FileDiff({
  file,
  rows,
  defaultOpen,
  project,
  refName,
}: {
  file: DiffFile;
  rows: DiffRow[];
  defaultOpen: boolean;
  project: string;
  refName: string | null;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(defaultOpen);
  const regionId = useId();
  const region = useRef<HTMLDivElement>(null);
  const asked = useRef(false);
  // The button disappears when it is used: the focus goes to what it revealed.
  useEffect(() => {
    if (open && asked.current) region.current?.focus();
  }, [open]);
  const shownPath = file.new_path ?? file.old_path ?? '';
  const status =
    file.status === 'added'
      ? t('diff.added')
      : file.status === 'deleted'
        ? t('diff.deleted')
        : file.status === 'renamed'
          ? t('diff.renamed')
          : file.status === 'copied'
            ? t('diff.copied')
            : file.status === 'type_changed'
              ? t('diff.typeChanged')
              : t('diff.modified');

  let note: string | null = null;
  if (file.binary) note = t('diff.binary');
  else if (file.omitted === 'too_large') note = t('diff.tooLarge');
  else if (file.omitted === 'total_cap') note = t('diff.totalCap');
  else if (file.omitted === 'not_utf8') note = t('diff.notUtf8');
  else if (file.omitted !== null) note = t('diff.omitted');
  else if (file.patch === '' && file.status !== 'added' && file.status !== 'deleted') {
    note = file.old_path !== file.new_path ? t('diff.renameOnly') : t('diff.modeOnly');
  }

  const rename =
    file.old_path !== null && file.new_path !== null && file.old_path !== file.new_path
      ? `${file.old_path} → ${file.new_path}`
      : null;

  return (
    <div className={styles.file}>
      <h3 className={styles.fileName}>
        {file.new_path !== null && refName !== null ? (
          <Link to={blobUrl(project, refName, file.new_path)}>{rename ?? shownPath}</Link>
        ) : (
          <span>{rename ?? shownPath}</span>
        )}{' '}
        <span className={styles.status}>{status}</span>
        {file.similarity !== null && (
          <span className={styles.status}>{t('diff.similarity', { percent: file.similarity })}</span>
        )}
      </h3>
      {note !== null && (
        <p className={styles.note} data-testid="diff-note">
          {note}
        </p>
      )}
      {rows.length > 0 &&
        (open ? (
          <div
            id={regionId}
            ref={region}
            className={styles.scroll}
            role="region"
            aria-label={t('diff.patchLabel', { path: shownPath })}
            tabIndex={0}
          >
            <table className={styles.table}>
              <thead className={styles.srOnly}>
                <tr>
                  <th scope="col">{t('diff.oldLine')}</th>
                  <th scope="col">{t('diff.newLine')}</th>
                  <th scope="col">{t('diff.change')}</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((row, i) =>
                  row.kind === 'hunk' || row.kind === 'note' ? (
                    <tr key={i} className={row.kind === 'hunk' ? styles.hunk : styles.noNewline}>
                      <td colSpan={3} className={styles.text}>
                        {row.text}
                      </td>
                    </tr>
                  ) : (
                    <tr key={i} className={styles[row.kind]}>
                      <td className={styles.no}>{row.oldNo}</td>
                      <td className={styles.no}>{row.newNo}</td>
                      <td className={styles.text}>
                        <span aria-hidden="true" className={styles.sign}>
                          {row.kind === 'add' ? '+' : row.kind === 'del' ? '-' : ' '}
                        </span>
                        <span className={styles.srOnly}>
                          {row.kind === 'add'
                            ? t('diff.lineAdded')
                            : row.kind === 'del'
                              ? t('diff.lineRemoved')
                              : ''}
                        </span>
                        {row.text}
                      </td>
                    </tr>
                  ),
                )}
              </tbody>
            </table>
          </div>
        ) : (
          <button
            type="button"
            className={styles.fold}
            aria-expanded="false"
            aria-controls={regionId}
            onClick={() => {
              asked.current = true;
              setOpen(true);
            }}
          >
            {t('diff.show', { count: rows.length })}
          </button>
        ))}
    </div>
  );
}

export function DiffView({
  diff,
  project,
  refName,
}: {
  diff: DiffBody;
  project: string;
  /** Ref for the file links (the commit or the compare target); null: no links. */
  refName: string | null;
}) {
  const { t } = useTranslation();
  let budget = TOTAL_ROWS;
  const prepared = diff.files.map((file) => {
    const rows = file.patch === null || file.patch === '' ? [] : parsePatch(file.patch);
    const defaultOpen = rows.length <= FOLD_ROWS && rows.length <= budget;
    if (defaultOpen) budget -= rows.length;
    return { file, rows, defaultOpen };
  });
  return (
    <div data-testid="diff">
      <p className={styles.summary}>{t('diff.files', { count: diff.files.length })}</p>
      {diff.truncated && (
        <p className={styles.truncated} role="note" data-testid="diff-truncated">
          {t('diff.truncated')}
        </p>
      )}
      {prepared.map(({ file, rows, defaultOpen }, i) => (
        <FileDiff
          key={`${file.old_path ?? ''}\0${file.new_path ?? ''}\0${i}`}
          file={file}
          rows={rows}
          defaultOpen={defaultOpen}
          project={project}
          refName={refName}
        />
      ))}
    </div>
  );
}
