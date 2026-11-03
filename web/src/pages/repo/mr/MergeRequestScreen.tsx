// One merge request (`/-/pull/{iid}[/changes]`): the header, the one-line
// status (why it is stopped, what next), and the tabs Overview (description
// and threads) and Changes. The status line and the threads read on their
// own: a failure of one shows in its place only, with its request_id.
// Approve and merge are U2.
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { useAddComment, useDiscussions, useMergeRequest } from '../../../api/mergeRequests';
import type { MergeRequest } from '../../../api/types';
import { Loading } from '../../../components/Loading';
import { QueryError } from '../../../components/QueryError';
import { formatDate, mrStateLabel } from '../../../features/labels';
import { MarkdownView } from '../../../features/MarkdownView';
import { CommentForm } from '../../../features/mr/CommentForm';
import { DiscussionThread, threadElementId } from '../../../features/mr/DiscussionThread';
import { StatusLine } from '../../../features/mr/StatusLine';
import { compareUrl, pullUrl } from '../../../features/repoUrls';
import { useDocumentTitle } from '../../../hooks/useDocumentTitle';
import { RepoError } from '../common';
import repo from '../Repo.module.css';
import styles from './Mr.module.css';

export type MrTab = 'overview' | 'changes';

export function MergeRequestScreen({
  projectId,
  project,
  iid,
  tab,
}: {
  projectId: number;
  project: string;
  /** Parsed once from the address (`parseIid`); never an invalid number. */
  iid: number;
  tab: MrTab;
}) {
  const { t } = useTranslation();
  const mr = useMergeRequest(projectId, iid);
  const threads = useDiscussions(projectId, iid);
  useDocumentTitle(mr.data === undefined ? null : `${mr.data.title} ${t('mr.number', { iid })}`);

  if (mr.isPending) return <Loading />;
  if (mr.isError) {
    return (
      <RepoError
        error={mr.error}
        onRetry={() => {
          void mr.refetch();
        }}
      />
    );
  }

  const firstUnresolved = threads.data?.discussions.find((d) => !d.resolved);
  const focusFirstUnresolved =
    firstUnresolved === undefined
      ? null
      : () => {
          document.getElementById(threadElementId(firstUnresolved.id))?.focus();
        };

  return (
    <>
      <Header mr={mr.data} />
      <StatusLine
        projectId={projectId}
        project={project}
        iid={iid}
        onFirstUnresolved={tab === 'overview' ? focusFirstUnresolved : null}
      />
      <nav aria-label={t('mr.view.tabsLabel')} className={styles.filter}>
        <ul className={styles.filterList}>
          <li>
            <Link
              to={pullUrl(project, iid)}
              className={styles.filterLink}
              {...(tab === 'overview' ? { 'aria-current': 'page' as const } : {})}
            >
              {t('mr.view.overview')}
            </Link>
          </li>
          <li>
            <Link
              to={pullUrl(project, iid, 'changes')}
              className={styles.filterLink}
              {...(tab === 'changes' ? { 'aria-current': 'page' as const } : {})}
            >
              {t('mr.view.changes')}
            </Link>
          </li>
        </ul>
      </nav>
      {tab === 'overview' ? (
        <Overview projectId={projectId} project={project} mr={mr.data} threads={threads} />
      ) : (
        <section aria-labelledby="mr-changes-title">
          <h2 id="mr-changes-title" className={repo.sectionTitle}>
            {t('mr.view.changes')}
          </h2>
          <p>{t('mr.view.changesLater')}</p>
          <p>
            <Link to={compareUrl(project, mr.data.target_branch, mr.data.source_branch)}>
              {t('mr.view.compareLink')}
            </Link>
          </p>
        </section>
      )}
    </>
  );
}

function Header({ mr }: { mr: MergeRequest }) {
  const { t, i18n } = useTranslation();
  return (
    <header>
      <h1 className={repo.title}>
        {mr.title} <span className={repo.titleNote}>{t('mr.number', { iid: mr.iid })}</span>
      </h1>
      <p className={repo.meta}>
        <span className={repo.badge}>{mrStateLabel(t, mr.state)}</span>
        {mr.draft && <span className={repo.badge}>{t('mr.draft')}</span>}
        <span>
          {t('mr.opened', {
            author: mr.author.display_name,
            date: formatDate(mr.created_at, i18n.language),
          })}
        </span>
        <span>{t('mr.branches', { source: mr.source_branch, target: mr.target_branch })}</span>
      </p>
    </header>
  );
}

function Overview({
  projectId,
  project,
  mr,
  threads,
}: {
  projectId: number;
  project: string;
  mr: MergeRequest;
  threads: ReturnType<typeof useDiscussions>;
}) {
  const { t } = useTranslation();
  const comment = useAddComment(projectId, mr.iid);
  return (
    <>
      <section aria-labelledby="mr-description-title">
        <h2 id="mr-description-title" className={repo.sectionTitle}>
          {t('mr.view.description')}
        </h2>
        {mr.description.trim() === '' ? (
          <p className={repo.hint}>{t('mr.view.noDescription')}</p>
        ) : (
          <MarkdownView
            source={mr.description}
            project={project}
            refName={mr.target_branch}
            dir=""
            headingOffset={2}
          />
        )}
      </section>
      <section aria-labelledby="mr-threads-title">
        <h2 id="mr-threads-title" className={repo.sectionTitle}>
          {t('mr.threads.title')}
        </h2>
        {threads.isPending && <Loading />}
        {threads.isError && (
          <QueryError
            error={threads.error}
            onRetry={() => {
              void threads.refetch();
            }}
          />
        )}
        {threads.isSuccess && (
          <>
            {threads.data.truncated && (
              <p className={repo.warning} role="status" data-testid="mr-threads-truncated">
                {t('mr.threads.truncated')}
              </p>
            )}
            {threads.data.discussions.length === 0 ? (
              <p className={repo.hint}>{t('mr.threads.empty')}</p>
            ) : (
              threads.data.discussions.map((d) => (
                <DiscussionThread
                  key={d.id}
                  projectId={projectId}
                  iid={mr.iid}
                  project={project}
                  refName={mr.target_branch}
                  discussion={d}
                />
              ))
            )}
          </>
        )}
        <CommentForm
          label={t('mr.comment.label')}
          submitLabel={t('mr.comment.submit')}
          pending={comment.isPending}
          error={comment.isError ? comment.error : null}
          onSend={(body, done) => {
            comment.mutate({ body }, { onSuccess: done });
          }}
        />
      </section>
    </>
  );
}
