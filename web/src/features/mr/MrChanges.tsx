// The Changes tab: the latest version's diff (U1 shows that one only) through
// the one DiffView, with a comment button before every line of an expanded
// file and the threads of this version under their lines. A comment's
// position is the line as the diff shows it: a removed line is the old side
// (its old number, the old path), an added or context line the new side (its
// new number, the new path). A version whose refs were purged has no diff to
// show: only the explanation.
import { useState } from 'react';
import { useTranslation } from 'react-i18next';

import {
  useAddComment,
  useDiscussions,
  useVersionDiff,
  useVersions,
} from '../../api/mergeRequests';
import type { Discussion, DiffFile, Position, Version } from '../../api/types';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import { DiffView, type DiffAnnotations, type LineRow } from '../DiffView';
import { CommentForm } from './CommentForm';
import { DiscussionThread } from './DiscussionThread';
import styles from './Mr.module.css';

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
  const latest = list.data.versions.reduce<Version | null>(
    (best, v) => (best === null || v.version > best.version ? v : best),
    null,
  );
  if (latest === null) return <p className={styles.explain}>{t('mr.diff.noVersion')}</p>;
  if (latest.refs_purged) return <p className={styles.explain}>{t('mr.diff.purged')}</p>;
  return (
    <VersionChanges
      projectId={projectId}
      project={project}
      iid={iid}
      version={latest.version}
      targetBranch={targetBranch}
    />
  );
}

function VersionChanges({
  projectId,
  project,
  iid,
  version,
  targetBranch,
}: {
  projectId: number;
  project: string;
  iid: number;
  version: number;
  targetBranch: string;
}) {
  const { t } = useTranslation();
  const diff = useVersionDiff(projectId, iid, version);
  const threads = useDiscussions(projectId, iid);
  const comment = useAddComment(projectId, iid);
  /** The line whose comment box is open (its position key). */
  const [openAt, setOpenAt] = useState<string | null>(null);

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
          type="button"
          className={styles.lineButton}
          aria-label={t('mr.diff.commentOn', { where: where(p) })}
          aria-expanded={openAt === k}
          onClick={() => {
            comment.reset();
            setOpenAt(openAt === k ? null : k);
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
            />
          ))}
          {openAt === k && (
            <CommentForm
              label={t('mr.diff.yourComment', { where: where(p) })}
              submitLabel={t('mr.comment.submit')}
              pending={comment.isPending}
              error={comment.isError ? comment.error : null}
              autoFocus
              onCancel={() => {
                setOpenAt(null);
              }}
              onSend={(body, done) => {
                comment.mutate(
                  { body, position: p },
                  {
                    onSuccess: () => {
                      done();
                      setOpenAt(null);
                    },
                  },
                );
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
