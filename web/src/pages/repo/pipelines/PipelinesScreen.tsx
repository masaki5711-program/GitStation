// A project's CI pipelines, newest first (`/-/pipelines?before=…`). The page
// is in the address (the server pages by id: `next_before`). Read again
// every few seconds while a pipeline on the page is still moving.
import { useTranslation } from 'react-i18next';
import { Link, useSearchParams } from 'react-router';

import { branchOf, parseId, usePipelines, type Pipeline } from '../../../api/pipelines';
import { Loading } from '../../../components/Loading';
import { formatDateTime, formatRelative } from '../../../features/labels';
import { StateBadge } from '../../../features/mr/StateBadge';
import {
  formatDuration,
  pipelineSourceLabel,
  pipelineStatusLabel,
  pipelineTone,
  triggeredByLabel,
} from '../../../features/pipelineLabels';
import { commitUrl, encodePath, pipelineUrl, pipelinesUrl } from '../../../features/repoUrls';
import { useDocumentTitle } from '../../../hooks/useDocumentTitle';
import { useSession } from '../../../session/session';
import { RepoError } from '../common';
import repo from '../Repo.module.css';
import styles from './Pipelines.module.css';

export function PipelinesScreen({ projectId, project }: { projectId: number; project: string }) {
  const { t } = useTranslation();
  const [params] = useSearchParams();
  const before = parseId(params.get('before')) ?? undefined;
  const page = usePipelines(projectId, before);
  const title = t('pipelines.title');
  useDocumentTitle(title);

  return (
    <>
      <h1 className={repo.title}>{title}</h1>
      {page.isPending && <Loading />}
      {page.isError && (
        <RepoError
          error={page.error}
          onRetry={() => {
            void page.refetch();
          }}
        />
      )}
      {page.isSuccess &&
        (page.data.pipelines.length === 0 ? (
          <section data-testid="pipelines-empty" className={styles.empty}>
            {before === undefined ? (
              <>
                <p>{t('pipelines.empty')}</p>
                <p>
                  <Link to={`/${encodePath(project)}/-/tree`}>{t('pipelines.emptyAction')}</Link>
                </p>
              </>
            ) : (
              <>
                <p>{t('pipelines.emptyPage')}</p>
                <p>
                  <Link to={pipelinesUrl(project)}>{t('pipelines.newest')}</Link>
                </p>
              </>
            )}
          </section>
        ) : (
          <>
            <PipelineList project={project} items={page.data.pipelines} />
            <nav aria-label={t('pipelines.pagerLabel')} className={styles.pager}>
              {before !== undefined && <Link to={pipelinesUrl(project)}>{t('pipelines.newest')}</Link>}
              {page.data.next_before !== null && (
                <Link to={pipelinesUrl(project, { before: page.data.next_before })}>
                  {t('pipelines.older')}
                </Link>
              )}
            </nav>
          </>
        ))}
    </>
  );
}

function PipelineList({ project, items }: { project: string; items: Pipeline[] }) {
  const { t, i18n } = useTranslation();
  const me = useSession().user.id;
  return (
    <ol className={styles.list} data-testid="pipeline-list" aria-label={t('pipelines.listLabel')}>
      {items.map((p) => {
        const duration = formatDuration(t, p.created_at, p.finished_at);
        return (
          <li key={p.id} className={styles.item}>
            <span className={styles.itemMain}>
              <StateBadge tone={pipelineTone(p.status)}>{pipelineStatusLabel(t, p.status)}</StateBadge>
              <Link to={pipelineUrl(project, p.id)} className={styles.number}>
                {t('pipelines.number', { id: p.id })}
              </Link>
              <code className={styles.ref}>{branchOf(p.ref)}</code>
              <Link
                to={commitUrl(project, p.sha)}
                aria-label={t('pipelines.openCommit', { sha: p.sha.slice(0, 8) })}
              >
                <code>{p.sha.slice(0, 8)}</code>
              </Link>
            </span>
            <span className={styles.meta}>
              <span>{pipelineSourceLabel(t, p.source)}</span>
              <span>{triggeredByLabel(t, p.triggered_by, me)}</span>
              {duration !== null && (
                <span className={styles.numeric}>{t('pipelines.took', { duration })}</span>
              )}
              <time dateTime={p.created_at} title={formatDateTime(p.created_at, i18n.language)}>
                {formatRelative(p.created_at, i18n.language)}
              </time>
            </span>
          </li>
        );
      })}
    </ol>
  );
}
