// Branches or tags, with the archive links of each. The server lists 5000
// refs at most (`truncated`). A tag of a tree or a blob has no commit: it
// cannot be browsed or archived as a commit, and says so.
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { useRefs } from '../../api/repo';
import { Loading } from '../../components/Loading';
import { archiveUrl, commitUrl, treeUrl } from '../../features/repoUrls';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { RepoError, shortOid } from './common';
import styles from './Repo.module.css';

export function RefsScreen({
  projectId,
  project,
  type,
}: {
  projectId: number;
  project: string;
  type: 'branch' | 'tag';
}) {
  const { t } = useTranslation();
  const refs = useRefs(projectId);
  const title = type === 'branch' ? t('repo.branches') : t('repo.tags');
  useDocumentTitle(title);
  if (refs.isPending) return <Loading />;
  if (refs.isError) {
    return (
      <RepoError
        error={refs.error}
        onRetry={() => {
          void refs.refetch();
        }}
      />
    );
  }
  const list = refs.data.refs.filter((r) => r.type === type);
  // A branch and a tag with one name: the short name would mean the branch.
  const shared = (name: string) => refs.data.refs.filter((r) => r.name === name).length > 1;
  return (
    <>
      <h1 className={styles.title}>{title}</h1>
      {refs.data.truncated && (
        <p className={styles.warning} role="note" data-testid="refs-truncated">
          {t('repo.refsTruncated')}
        </p>
      )}
      {list.length === 0 ? (
        <p>{type === 'branch' ? t('refs.noBranches') : t('refs.noTags')}</p>
      ) : (
        <table className={styles.table}>
          <caption className={styles.srOnly}>{title}</caption>
          <thead>
            <tr>
              <th scope="col">{t('refs.name')}</th>
              <th scope="col">{t('refs.commit')}</th>
              <th scope="col">{t('refs.download')}</th>
            </tr>
          </thead>
          <tbody>
            {list.map((r) => (
              <tr key={r.full_name}>
                <th scope="row" className={styles.entry}>
                  {r.commit !== null ? (
                    <Link to={treeUrl(project, r.name)}>{r.name}</Link>
                  ) : (
                    <span>{r.name}</span>
                  )}
                  {type === 'branch' && r.name === refs.data.default_branch && (
                    <span className={styles.badge}>{t('refs.default')}</span>
                  )}
                </th>
                <td>
                  {r.commit !== null ? (
                    <Link to={commitUrl(project, r.commit)}>
                      <code>{shortOid(r.commit)}</code>
                    </Link>
                  ) : (
                    <span>{t('refs.notCommit')}</span>
                  )}
                </td>
                <td>
                  {r.commit !== null && (
                    <span className={styles.links}>
                      <a href={archiveUrl(project, shared(r.name) ? r.commit : r.name, 'zip')}>
                        {t('refs.zip')}
                        <span className={styles.srOnly}> {r.name}</span>
                      </a>
                      <a href={archiveUrl(project, shared(r.name) ? r.commit : r.name, 'tar.gz')}>
                        {t('refs.targz')}
                        <span className={styles.srOnly}> {r.name}</span>
                      </a>
                    </span>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </>
  );
}
