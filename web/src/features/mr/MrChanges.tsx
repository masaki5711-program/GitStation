// The Changes tab: the latest version's diff (U1 shows that one only) through
// the one DiffView, with a comment button before every line of an expanded
// file and the threads of this version under their lines. A comment's
// position is the line as the diff shows it: a removed line is the old side
// (its old number, the old path), an added or context line the new side (its
// new number, the new path). A version whose refs were purged has no diff to
// show: only the explanation.
//
// Each open comment box has its own mutation (keyed by its line), so one
// line's sending state never shows on another; when a box closes (cancel or
// sent) the focus goes back to that line's button.
import { useCallback, useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import {
  useAddComment,
  useDiscussions,
  useVersionDiff,
  useVersions,
  latestVersion,
} from '../../api/mergeRequests';
import type { Discussion, DiffFile, Position, Version } from '../../api/types';
import repo from '../../pages/repo/Repo.module.css';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import { DiffView, type DiffAnnotations, type LineRow } from '../DiffView';
import { CommentForm } from './CommentForm';
import { DiscussionThread } from './DiscussionThread';
import { useThreadToggle } from './useThreadToggle';
import styles from './Mr.module.css';
import { useViewedHead } from './viewedHead';

/** Where a comment on this line goes (null: the line has no side to name). */
export function positionOf(file: DiffFile, row: LineRow, version: number): Position | null {
  const side = row.kind === 'del' ? 'old' : 'new';
  const path = side === 'old' ? file.old_path : file.new_path;
  const line = side === 'old' ? row.oldNo : row.newNo;
  if (path === null || line === null) return null;
  return { path, line, side, version };
}

const keyOf = (p: Pick<Position, 'side' | 'path' | 'line'>) => `${p.side}\0${p.path}\0${String(p.line)}`;

export function MrChanges({
  projectId,
  project,
  iid,
  targetBranch,
}: {
  projectId: number;
  project: string;
  iid: number;
  targetBranch: string;
}) {
  const { t } = useTranslation();
  const list = useVersions(projectId, iid);
  const viewed = useViewedHead();
  const shown = useRef(false);
  /** A version's diff was shown: its head counts as seen. Once per opening of
   * the tab, so a version a later refetch brings is not taken unseen. */
  const onShown = useCallback(
    (head: string) => {
      if (shown.current) return;
      shown.current = true;
      viewed.accept(head);
    },
    [viewed],
  );
  if (list.isPending) return <Loading />;
  if (list.isError) {
    return (
      <QueryError
        error={list.error}
        onRetry={() => {
          void list.refetch();
        }}
      />
    );
  }
  const latest: Version | null = latestVersion(list.data.versions);
  if (latest === null) return <p className={styles.explain}>{t('mr.diff.noVersion')}</p>;
  if (latest.refs_purged) return <p className={styles.explain}>{t('mr.diff.purged')}</p>;
  return (
    <VersionChanges
      projectId={projectId}
      project={project}
      iid={iid}
      version={latest.version}
      head={latest.head_sha}
      targetBranch={targetBranch}
      onShown={onShown}
    />
  );
}

function VersionChanges({
  projectId,
  project,
  iid,
  version,
  head,
  targetBranch,
  onShown,
}: {
  projectId: number;
  project: string;
  iid: number;
  version: number;
  head: string;
  targetBranch: string;
  /** The diff of this version was shown (not when it is loading or failed). */
  onShown: (head: string) => void;
}) {
  const { t } = useTranslation();
  const diff = useVersionDiff(projectId, iid, version);
  useEffect(() => {
    if (diff.isSuccess) onShown(head);
  }, [diff.isSuccess, head, onShown]);
  const threads = useDiscussions(projectId, iid);
  const toggle = useThreadToggle(projectId, iid);
  /** The line whose comment box is open (its position key). */
  const [openAt, setOpenAt] = useState<string | null>(null);
  /** The line whose button takes the focus once its box has closed. */
  const [focusAt, setFocusAt] = useState<string | null>(null);
  const buttons = useRef(new Map<string, HTMLButtonElement>());
  useEffect(() => {
    if (focusAt === null || openAt === focusAt) return;
    buttons.current.get(focusAt)?.focus();
    setFocusAt(null);
  }, [focusAt, openAt]);
  const close = (k: string) => {
    setOpenAt(null);
    setFocusAt(k);
  };

  if (diff.isPending) return <Loading />;
  if (diff.isError) {
    return (
      <QueryError
        error={diff.error}
        onRetry={() => {
          void diff.refetch();
        }}
      />
    );
  }

  const byLine = new Map<string, Discussion[]>();
  for (const d of threads.data?.discussions ?? []) {
    if (d.position === null || d.position.version !== version) continue;
    const k = keyOf(d.position);
    byLine.set(k, [...(byLine.get(k) ?? []), d]);
  }

  const where = (p: Position) =>
    p.side === 'old'
      ? t('mr.diff.lineOld', { path: p.path, line: p.line })
      : t('mr.diff.lineNew', { path: p.path, line: p.line });

  const annotations: DiffAnnotations = {
    columnLabel: t('mr.diff.column'),
    action: (file, row) => {
      const p = positionOf(file, row, version);
      if (p === null) return null;
      const k = keyOf(p);
      return (
        <button
          ref={(el) => {
            if (el === null) buttons.current.delete(k);
            else buttons.current.set(k, el);
          }}
          type="button"
          className={styles.lineButton}
          aria-label={t('mr.diff.commentOn', { where: where(p) })}
          aria-expanded={openAt === k}
          onClick={() => {
            if (openAt === k) close(k);
            else setOpenAt(k);
          }}
        />
      );
    },
    below: (file, row) => {
      const p = positionOf(file, row, version);
      if (p === null) return null;
      const k = keyOf(p);
      const here = byLine.get(k) ?? [];
      if (here.length === 0 && openAt !== k) return null;
      return (
        <>
          {here.map((d) => (
            <DiscussionThread
              key={d.id}
              projectId={projectId}
              iid={iid}
              project={project}
              refName={targetBranch}
              discussion={d}
              headingLevel={4}
              toggle={toggle}
            />
          ))}
          {openAt === k && (
            <LineCommentBox
              key={k}
              projectId={projectId}
              iid={iid}
              position={p}
              label={t('mr.diff.yourComment', { where: where(p) })}
              onClose={() => {
                close(k);
              }}
            />
          )}
        </>
      );
    },
  };

  return (
    <>
      <p className={styles.explain}>{t('mr.diff.version', { version })}</p>
      {threads.data?.truncated === true && (
        <p className={repo.warning} role="status" data-testid="mr-threads-truncated">
          {t('mr.threads.truncated')}
        </p>
      )}
      {threads.isError && (
        <QueryError
          error={threads.error}
          onRetry={() => {
            void threads.refetch();
          }}
        />
      )}
      <DiffView
        diff={diff.data.diff}
        project={project}
        refName={diff.data.to}
        annotations={annotations}
      />
    </>
  );
}

/** One line's comment box with its own mutation: its pending and error
 * state belong to this line only. */
function LineCommentBox({
  projectId,
  iid,
  position,
  label,
  onClose,
}: {
  projectId: number;
  iid: number;
  position: Position;
  label: string;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const comment = useAddComment(projectId, iid);
  return (
    <CommentForm
      label={label}
      submitLabel={t('mr.comment.submit')}
      pending={comment.isPending}
      error={comment.isError ? comment.error : null}
      autoFocus
      onCancel={onClose}
      onSend={(body, done) => {
        comment.mutate(
          { body, position },
          {
            onSuccess: () => {
              done();
              onClose();
            },
          },
        );
      }}
    />
  );
}
