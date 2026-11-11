// The side panel of one merge request (Overview): a short summary of what
// the API already returns, for whoever wants the numbers at a glance —
// approvals and who gave them, the required checks, the threads, the
// versions, the work item link and the last update. The detail (every
// reason, the actions) stays in the conditions panel; this panel adds no
// request of its own (the status, versions and threads queries are the ones
// the page already reads). A failed status or versions read is shown with
// its request_id elsewhere on the page (one alert per failure), so here only
// a pointer to it.
import { useTranslation } from 'react-i18next';

import { latestVersion, useMergeStatus, useVersions, type useDiscussions } from '../../api/mergeRequests';
import type { MergeRequest, MergeStatus } from '../../api/types';
import { Loading } from '../../components/Loading';
import { shortOid } from '../../pages/repo/common';
import { formatDateTime, formatRelative } from '../labels';
import { StateBadge, type BadgeTone } from './StateBadge';
import styles from './Mr.module.css';

/** Approvals over the rules that apply: each rule counts up to what it needs. */
export function approvalTotals(status: Pick<MergeStatus, 'approvals'>): {
  counted: number;
  required: number;
} {
  const rules = status.approvals?.rules.filter((r) => r.applies) ?? [];
  return rules.reduce(
    (sum, r) => ({
      counted: sum.counted + Math.min(r.counted, r.required),
      required: sum.required + r.required,
    }),
    { counted: 0, required: 0 },
  );
}

/** Required checks that finished with success (no report is never a
 * success; a stale run does not count) and those that finished otherwise. */
export function checkTotals(status: Pick<MergeStatus, 'checks'>): { passed: number; failed: number } {
  let passed = 0;
  let failed = 0;
  for (const c of status.checks) {
    if (c.status !== 'completed' || c.stale) continue;
    if (c.conclusion === 'success') passed += 1;
    else failed += 1;
  }
  return { passed, failed };
}

export function MrSidePanel({
  projectId,
  mr,
  threads,
}: {
  projectId: number;
  mr: MergeRequest;
  threads: ReturnType<typeof useDiscussions>;
}) {
  const { t, i18n } = useTranslation();
  const status = useMergeStatus(projectId, mr.iid);
  const versions = useVersions(projectId, mr.iid);
  const latest = versions.isSuccess ? latestVersion(versions.data.versions) : null;
  const all = threads.data?.discussions ?? [];
  const open = all.filter((d) => !d.resolved).length;

  return (
    <aside aria-labelledby="mr-side-title" className={styles.side} data-testid="mr-side">
      <h2 id="mr-side-title" className={styles.sideTitle}>
        {t('mr.side.title')}
      </h2>
      {status.isPending && <Loading />}
      {status.isError && <p className={styles.nextHint}>{t('mr.side.unread')}</p>}
      <dl className={styles.sideList}>
        {status.isSuccess && <StatusRows status={status.data} />}
        {threads.isSuccess && (
          <div className={styles.sideRow}>
            <dt>{t('mr.side.threads')}</dt>
            <dd className={styles.number} data-testid="mr-side-threads">
              {all.length === 0
                ? t('mr.side.threadsNone')
                : t('mr.side.threadsCount', { open, total: all.length })}
            </dd>
          </div>
        )}
        {versions.isSuccess && (
          <div className={styles.sideRow}>
            <dt>{t('mr.side.versions')}</dt>
            <dd className={styles.number} data-testid="mr-side-versions">
              {versions.data.versions.length}
              {latest !== null && (
                <span className={styles.sideNote}>
                  {t('mr.side.latestHead', { sha: shortOid(latest.head_sha) })}
                </span>
              )}
            </dd>
          </div>
        )}
        <div className={styles.sideRow}>
          <dt>{t('mr.side.updated')}</dt>
          <dd>
            <time
              className={styles.number}
              dateTime={mr.updated_at}
              title={formatDateTime(mr.updated_at, i18n.language)}
            >
              {formatRelative(mr.updated_at, i18n.language)}
            </time>
          </dd>
        </div>
      </dl>
      {versions.isError && !status.isError && <p className={styles.nextHint}>{t('mr.side.unread')}</p>}
    </aside>
  );
}

function StatusRows({ status }: { status: MergeStatus }) {
  const { t } = useTranslation();
  const approvals = approvalTotals(status);
  const given = status.approvals?.approvals ?? [];
  const { passed, failed } = checkTotals(status);
  const total = status.checks.length;
  const checkTone: BadgeTone = passed === total ? 'open' : failed > 0 ? 'unresolved' : 'draft';
  return (
    <>
      <div className={styles.sideRow}>
        <dt>{t('mr.side.approvals')}</dt>
        <dd data-testid="mr-side-approvals">
          {approvals.required === 0 ? (
            t('mr.side.approvalsNone')
          ) : (
            <StateBadge tone={approvals.counted >= approvals.required ? 'open' : 'draft'}>
              <span className={styles.number}>
                {t('mr.side.approvalsCount', approvals)}
              </span>
            </StateBadge>
          )}
          {given.length > 0 && (
            <ul className={styles.sidePeople} aria-label={t('mr.side.approvedBy')}>
              {given.map((g) => (
                <li key={g.approver_id}>
                  {g.counted ? g.username : t('mr.side.notCounted', { name: g.username })}
                </li>
              ))}
            </ul>
          )}
        </dd>
      </div>
      <div className={styles.sideRow}>
        <dt>{t('mr.side.checks')}</dt>
        <dd data-testid="mr-side-checks">
          {total === 0 ? (
            t('mr.side.checksNone')
          ) : (
            <StateBadge tone={checkTone}>
              <span className={styles.number}>{t('mr.side.checksCount', { passed, total })}</span>
            </StateBadge>
          )}
        </dd>
      </div>
      {status.work_item_link !== null && (
        <div className={styles.sideRow}>
          <dt>{t('mr.side.workItem')}</dt>
          <dd data-testid="mr-side-work-item">
            {status.work_item_link.linked ? t('mr.side.linked') : t('mr.side.notLinked')}
          </dd>
        </div>
      )}
    </>
  );
}
