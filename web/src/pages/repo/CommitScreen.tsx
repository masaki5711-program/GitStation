// One commit: message, author, parents and the diff against the first parent
// (a root commit: against nothing). A diff the server cut is said so.
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { useCommit } from '../../api/repo';
import { Loading } from '../../components/Loading';
import { CopyButton } from '../../components/CopyButton';
import { DiffView } from '../../features/DiffView';
import { formatDateTime } from '../../features/labels';
import { commitUrl, treeUrl } from '../../features/repoUrls';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { NotFound } from '../NotFound';
import { RepoError, shortOid } from './common';
import styles from './Repo.module.css';

const OID = /^[0-9a-fA-F]{40}(?:[0-9a-fA-F]{24})?$/;

export function CommitScreen({
  projectId,
  project,
  oid,
}: {
  projectId: number;
  project: string;
  oid: string;
}) {
  const valid = OID.test(oid);
  const commit = useCommit(projectId, oid.toLowerCase(), valid);
  const { t, i18n } = useTranslation();
  useDocumentTitle(commit.data?.commit.subject ?? null);
  if (!valid) return <NotFound />;
  if (commit.isPending) return <Loading />;
  if (commit.isError) {
    return (
      <RepoError
        error={commit.error}
        onRetry={() => {
          void commit.refetch();
        }}
      />
    );
  }
  const { commit: c, message, diff } = commit.data;
  const sameCommitter =
    c.committer_name === c.author_name &&
    c.committer_email === c.author_email &&
    c.committed_at === c.authored_at;
  return (
    <>
      <h1 className={styles.title}>{c.subject === '' ? t('commits.noSubject') : c.subject}</h1>
      <div className={styles.scroll} role="region" aria-label={t('commit.messageLabel')} tabIndex={0}>
        <pre className={styles.message}>{message}</pre>
      </div>
      <dl className={styles.facts}>
        <dt>{t('commit.author')}</dt>
        <dd>
          {c.author_name} {'·'} {formatDateTime(c.authored_at, i18n.language)}
        </dd>
        {!sameCommitter && (
          <>
            <dt>{t('commit.committer')}</dt>
            <dd>
              {c.committer_name} {'·'} {formatDateTime(c.committed_at, i18n.language)}
            </dd>
          </>
        )}
        <dt>{t('commit.id')}</dt>
        <dd>
          <code>{c.oid}</code> <CopyButton text={c.oid} label={t('commit.copyId')} />
        </dd>
        <dt>{t('commit.parents')}</dt>
        <dd>
          {c.parents.length === 0
            ? t('commit.rootCommit')
            : c.parents.map((p, i) => (
                <span key={p}>
                  {i > 0 && ' '}
                  <Link to={commitUrl(project, p)}>
                    <code>{shortOid(p)}</code>
                  </Link>
                </span>
              ))}
          {c.parents.length > 1 && <> {t('commit.mergeNote')}</>}
        </dd>
      </dl>
      <p>
        <Link to={treeUrl(project, c.oid)}>{t('commit.browseFiles')}</Link>
      </p>
      <h2 className={styles.sectionTitle}>{t('commit.changes')}</h2>
      <DiffView diff={diff} project={project} refName={c.oid} />
    </>
  );
}
