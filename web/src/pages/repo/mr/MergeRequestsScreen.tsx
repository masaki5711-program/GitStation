// A project's merge requests, newest first (`/-/pulls?state=…&before=…`).
// The filter and the page are in the address, so back / forward and a shared
// link show the same list; the server pages by iid (`next_before`).
import { useTranslation } from 'react-i18next';
import { Link, useSearchParams } from 'react-router';

import { useMergeRequests, type MrState } from '../../../api/mergeRequests';
import type { MergeRequest } from '../../../api/types';
import { Loading } from '../../../components/Loading';
import controls from '../../../components/controls.module.css';
import { formatDate, mrStateLabel } from '../../../features/labels';
import { newPullUrl, pullUrl, pullsUrl } from '../../../features/repoUrls';
import { useDocumentTitle } from '../../../hooks/useDocumentTitle';
import { RepoError } from '../common';
import repo from '../Repo.module.css';
import styles from './Mr.module.css';

const STATES: readonly MrState[] = ['open', 'merged', 'closed', 'all'];

function stateOf(raw: string | null): MrState {
  return STATES.find((s) => s === raw) ?? 'open';
}

/** `before` from the address: a positive integer, or absent. */
function beforeOf(raw: string | null): number | undefined {
  if (raw === null || !/^[1-9][0-9]{0,15}$/.test(raw)) return undefined;
  return Number(raw);
}

export function MergeRequestsScreen({ projectId, project }: { projectId: number; project: string }) {
  const { t } = useTranslation();
  const [params] = useSearchParams();
  const state = stateOf(params.get('state'));
  const before = beforeOf(params.get('before'));
  const page = useMergeRequests(projectId, { state, ...(before === undefined ? {} : { before }) });
  const title = t('mr.list.title');
  useDocumentTitle(title);

  return (
    <>
      <div className={repo.toolbar}>
        <h1 className={repo.title}>{title}</h1>
        <Link to={newPullUrl(project)} className={controls.primary}>
          {t('mr.list.new')}
        </Link>
      </div>
      <nav aria-label={t('mr.list.filterLabel')} className={styles.filter}>
        <ul className={styles.filterList}>
          {STATES.map((s) => (
            <li key={s}>
              <Link
                to={pullsUrl(project, { state: s })}
                className={styles.filterLink}
                {...(s === state ? { 'aria-current': 'page' as const } : {})}
              >
                {mrStateLabel(t, s)}
              </Link>
            </li>
          ))}
        </ul>
      </nav>
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
        (page.data.merge_requests.length === 0 ? (
          <section data-testid="mr-empty" className={styles.empty}>
            <p>{t('mr.list.empty')}</p>
            <p>{t('mr.list.emptyLead')}</p>
            <p>
              <Link to={newPullUrl(project)}>{t('mr.list.new')}</Link>
            </p>
          </section>
        ) : (
          <>
            <MrList project={project} items={page.data.merge_requests} />
            <p className={styles.pager}>
              {before !== undefined && (
                <Link to={pullsUrl(project, { state })}>{t('mr.list.newest')}</Link>
              )}
              {page.data.next_before != null && (
                <Link to={pullsUrl(project, { state, before: page.data.next_before })}>
                  {t('mr.list.older')}
                </Link>
              )}
            </p>
          </>
        ))}
    </>
  );
}

function MrList({ project, items }: { project: string; items: MergeRequest[] }) {
  const { t, i18n } = useTranslation();
  // Newest first, as the server sends them; sorted again so a page that
  // arrives in another order cannot mislead.
  const sorted = [...items].sort((a, b) => b.iid - a.iid);
  return (
    <ol className={styles.list} data-testid="mr-list">
      {sorted.map((m) => (
        <li key={m.iid} className={styles.item}>
          <span className={styles.itemTitle}>
            <Link to={pullUrl(project, m.iid)} className={repo.subject}>
              {m.title}
            </Link>
            {m.state !== 'open' && <span className={repo.badge}>{mrStateLabel(t, m.state)}</span>}
            {m.draft && <span className={repo.badge}>{t('mr.draft')}</span>}
          </span>
          <span className={repo.hint}>
            {t('mr.number', { iid: m.iid })} {'·'}{' '}
            {t('mr.opened', {
              author: m.author.display_name,
              date: formatDate(m.created_at, i18n.language),
            })}{' '}
            {'·'} {t('mr.branches', { source: m.source_branch, target: m.target_branch })}
          </span>
        </li>
      ))}
    </ol>
  );
}
