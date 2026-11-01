// Who last changed each line. The server blames up to 10 000 lines
// (`truncated`) and sends no lines for a binary file; both are said in words.
import { useTranslation } from 'react-i18next';
import { Link, Navigate } from 'react-router';

import { refParam, useBlame } from '../../api/repo';
import type { BlameBody, Rev } from '../../api/types';
import { Loading } from '../../components/Loading';
import { tokensClass, useLineNodes } from '../../features/CodeView';
import { formatEpoch } from '../../features/labels';
import { baseName, blobUrl, commitUrl, treeUrl } from '../../features/repoUrls';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { PathCrumbs, RepoError, shortOid, WithSpec } from './common';
import styles from './Repo.module.css';

export function BlameScreen({
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
      {(s) => <Blame projectId={projectId} project={project} rev={s.rev} path={s.path} />}
    </WithSpec>
  );
}

function Blame({
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
  const blame = useBlame(projectId, refParam(rev), path);
  useDocumentTitle(path === '' ? null : `${baseName(path)} (${t('blame.title')})`);
  if (path === '') return <Navigate replace to={treeUrl(project, rev.name)} />;
  if (blame.isPending) return <Loading />;
  if (blame.isError) {
    return (
      <RepoError
        error={blame.error}
        onRetry={() => {
          void blame.refetch();
        }}
      />
    );
  }
  return <BlameTable project={project} rev={rev} path={path} blame={blame.data} />;
}

function BlameTable({
  project,
  rev,
  path,
  blame,
}: {
  project: string;
  rev: Rev;
  path: string;
  blame: BlameBody;
}) {
  const { t, i18n } = useTranslation();
  const all = blame.hunks.flatMap((h) => h.lines);
  const nodes = useLineNodes(path, all);
  const byId = new Map(blame.commits.map((c) => [c.oid, c]));
  let offset = 0;
  return (
    <>
      <PathCrumbs project={project} rev={rev.name} path={path} />
      <h1 className={styles.title}>
        {baseName(path)} <span className={styles.titleNote}>{t('blame.title')}</span>
      </h1>
      <p className={styles.actions}>
        <Link to={blobUrl(project, rev.name, path)}>{t('blame.backToFile')}</Link>
      </p>
      {blame.binary && (
        <p className={styles.warning} role="note" data-testid="blame-binary">
          {t('blame.binary')}
        </p>
      )}
      {blame.truncated && (
        <p className={styles.warning} role="note" data-testid="blame-truncated">
          {t('blame.truncated', { count: all.length })}
        </p>
      )}
      {!blame.binary && blame.hunks.length === 0 && <p>{t('file.emptyFile')}</p>}
      {blame.hunks.length > 0 && (
        <div
          className={styles.scroll}
          role="region"
          aria-label={t('blame.tableLabel')}
          tabIndex={0}
        >
          <table className={styles.blame}>
            <thead>
              <tr>
                <th scope="col">{t('blame.commit')}</th>
                <th scope="col" className={styles.num}>
                  {t('blame.line')}
                </th>
                <th scope="col">{t('blame.code')}</th>
              </tr>
            </thead>
            <tbody>
              {blame.hunks.map((hunk) => {
                const commit = byId.get(hunk.commit);
                const first = offset;
                offset += hunk.lines.length;
                return hunk.lines.map((_, i) => (
                  <tr key={`${hunk.start}-${i}`} className={i === 0 ? styles.hunkStart : undefined}>
                    {i === 0 && (
                      <th scope="row" rowSpan={hunk.lines.length} className={styles.blameCommit}>
                        <Link to={commitUrl(project, hunk.commit)}>{shortOid(hunk.commit)}</Link>
                        {commit !== undefined && (
                          <>
                            {' '}
                            <span>{commit.summary}</span>
                            <br />
                            <span className={styles.hint}>
                              {commit.author_name} {'·'} {formatEpoch(commit.author_time, i18n.language)}
                            </span>
                          </>
                        )}
                      </th>
                    )}
                    <td className={styles.num}>{hunk.start + i}</td>
                    <td className={`${styles.blameCode} ${tokensClass}`}>{nodes[first + i]}</td>
                  </tr>
                ));
              })}
            </tbody>
          </table>
        </div>
      )}
    </>
  );
}
