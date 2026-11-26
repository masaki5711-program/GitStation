// A project's deployments, newest first (`/-/deployments`, at most 100):
// environment, status in words, the commit, when. One waiting for an
// approval links to its approval screen like any other. Read again while
// one of them still moves.
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { useDeployments, type Deployment } from '../../../api/deployments';
import { Loading } from '../../../components/Loading';
import { environmentLabel } from '../../../features/deployApproval';
import { formatDateTime, formatRelative } from '../../../features/labels';
import { commitUrl, deploymentUrl, pipelinesUrl } from '../../../features/repoUrls';
import { useDocumentTitle } from '../../../hooks/useDocumentTitle';
import { PageHeader } from '../../../ui/PageHeader';
import { RepoError } from '../common';
import { DeployBadge } from './DeployBadge';
import styles from './Deploy.module.css';

export function DeploymentsScreen({ projectId, project }: { projectId: number; project: string }) {
  const { t } = useTranslation();
  const list = useDeployments(projectId);
  const title = t('deploy.title');
  useDocumentTitle(title);
  return (
    <>
      <PageHeader title={title} subtitle={t('deploy.subtitle')} />
      {list.isPending && <Loading />}
      {list.isError && (
        <RepoError
          error={list.error}
          onRetry={() => {
            void list.refetch();
          }}
        />
      )}
      {list.isSuccess &&
        (list.data.deployments.length === 0 ? (
          <section data-testid="deployments-empty" className={styles.empty}>
            <p>{t('deploy.empty')}</p>
            <p>
              <Link to={pipelinesUrl(project)}>{t('deploy.emptyAction')}</Link>
            </p>
          </section>
        ) : (
          <DeploymentList project={project} items={list.data.deployments} />
        ))}
    </>
  );
}

function DeploymentList({ project, items }: { project: string; items: Deployment[] }) {
  const { t, i18n } = useTranslation();
  return (
    <ol className={styles.list} data-testid="deployment-list" aria-label={t('deploy.listLabel')}>
      {items.map((d) => (
        <li key={d.id} className={styles.item}>
          <span className={styles.itemMain}>
            <DeployBadge status={d.status} />
            <Link to={deploymentUrl(project, d.id)} className={styles.number}>
              {t('deploy.number', { id: d.id })}
            </Link>
            <span>{environmentLabel(t, d.environment)}</span>
            <Link to={commitUrl(project, d.sha)} aria-label={t('deploy.openCommit', { sha: d.sha.slice(0, 8) })}>
              <code>{d.sha.slice(0, 8)}</code>
            </Link>
            {d.commit_subject != null && <span className={styles.subject}>{d.commit_subject}</span>}
          </span>
          <span className={styles.meta}>
            {d.error_code != null && <code>{d.error_code}</code>}
            <time dateTime={d.created_at} title={formatDateTime(d.created_at, i18n.language)}>
              {formatRelative(d.created_at, i18n.language)}
            </time>
          </span>
        </li>
      ))}
    </ol>
  );
}
