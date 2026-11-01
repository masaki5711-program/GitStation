// The history of a ref (optionally of one path), newest first. The server
// pages with a keyset cursor: "load more" asks for the next page from the
// commit the first page was read at, so a push in between shifts nothing.
import { useEffect, useRef, useState, type Ref } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { refParam, useCommits } from '../../api/repo';
import type { CommitSummary, Rev } from '../../api/types';
import { Loading } from '../../components/Loading';
import controls from '../../components/controls.module.css';
import { formatDateTime } from '../../features/labels';
import { commitUrl, commitsUrl } from '../../features/repoUrls';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { RefSwitcher, RepoError, shortOid, WithSpec } from './common';
import styles from './Repo.module.css';

export function CommitList({
  project,
  commits,
  listRef,
}: {
  project: string;
  commits: CommitSummary[];
  listRef?: Ref<HTMLOListElement>;
}) {
  const { t, i18n } = useTranslation();
  return (
    <ol className={styles.commits} data-testid="commit-list" ref={listRef}>
      {commits.map((c) => (
        <li key={c.oid} className={styles.commit}>
          <Link to={commitUrl(project, c.oid)} className={styles.subject}>
            {c.subject === '' ? t('commits.noSubject') : c.subject}
          </Link>
          <span className={styles.hint}>
            {c.author_name} {'·'} {formatDateTime(c.authored_at, i18n.language)}
            {c.parents.length > 1 && (
              <>
                {' '}
                {'·'} <span className={styles.badge}>{t('commits.merge')}</span>
              </>
            )}
          </span>
          <code className={styles.oid}>{shortOid(c.oid)}</code>
        </li>
      ))}
    </ol>
  );
}

export function CommitsScreen({
  projectId,
  project,
  spec,
}: {
  projectId: number;
  project: string;
  spec: string;
}) {
  return (
    <WithSpec projectId={projectId} project={project} spec={spec}>
      {(s) => <History projectId={projectId} project={project} rev={s.rev} path={s.path} />}
    </WithSpec>
  );
}

function History({
  projectId,
  project,
  rev,
  path,
}: {
  projectId: number;
  project: string;
  rev: Rev;
  path: string;
}) {
  const { t } = useTranslation();
  const commits = useCommits(projectId, refParam(rev), path);
  useDocumentTitle(t('commits.title'));
  const list = useRef<HTMLOListElement>(null);
  const before = useRef<number | null>(null);
  const [announce, setAnnounce] = useState('');
  const total = commits.data?.pages.reduce((n, p) => n + p.commits.length, 0) ?? 0;
  // After "Load more" the focus goes to the first new commit, and a
  // permanent live region says how many came (the button may be gone).
  useEffect(() => {
    const from = before.current;
    if (from === null || total <= from) return;
    list.current?.querySelectorAll('li')[from]?.querySelector('a')?.focus();
    setAnnounce(t('commits.loaded', { count: total - from }));
    before.current = null;
  }, [total, t]);
  if (commits.isPending) return <Loading />;
  if (commits.isError && commits.data === undefined) {
    return (
      <RepoError
        error={commits.error}
        onRetry={() => {
          void commits.refetch();
        }}
      />
    );
  }
  const all = commits.data.pages.flatMap((p) => p.commits);
  return (
    <>
      <h1 className={styles.title}>{t('commits.title')}</h1>
      <RefSwitcher
        projectId={projectId}
        current={rev.name}
        to={(ref) => commitsUrl(project, ref)}
      />
      {path !== '' && (
        <p>
          {t('commits.onlyPath')} <code>{path}</code>{' '}
          <Link to={commitsUrl(project, rev.name)}>{t('commits.allPaths')}</Link>
        </p>
      )}
      {all.length === 0 ? <p>{t('commits.empty')}</p> : <CommitList project={project} commits={all} listRef={list} />}
      <p role="status" className={styles.srOnly}>
        {announce}
      </p>
      {commits.isError && (
        <RepoError
          error={commits.error}
          onRetry={() => {
            before.current = all.length;
            void commits.fetchNextPage();
          }}
        />
      )}
      {commits.hasNextPage ? (
        <p>
          <button
            type="button"
            className={controls.button}
            aria-disabled={commits.isFetchingNextPage ? 'true' : undefined}
            onClick={() => {
              if (commits.isFetchingNextPage) return;
              before.current = all.length;
              void commits.fetchNextPage();
            }}
          >
            {commits.isFetchingNextPage ? t('common.loading') : t('commits.more')}
          </button>
        </p>
      ) : (
        all.length > 0 && <p className={styles.hint}>{t('commits.end')}</p>
      )}
    </>
  );
}
