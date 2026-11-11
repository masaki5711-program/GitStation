// The side panel of one merge request (Overview): a short summary of what
// the API already returns, for whoever wants the numbers at a glance —
// approvals and who gave them, the required checks, the threads, the
// versions, the work item link and the last update. The detail (every
// reason, the actions) stays in the conditions panel; this panel adds no
// request of its own (the status, versions and threads queries are the ones
// the page already reads). Each source loads and fails on its own row: a
// placeholder while it is read, and on failure a compact InlineError (the
// server's user_message, the request_id, a retry). InlineError is not a
// live region, so the page keeps one announced alert per failure.
// Badges say the state in words, never by colour alone.
import type { TFunction } from 'i18next';
import type { ReactNode } from 'react';
import { useTranslation } from 'react-i18next';

import { latestVersion, useMergeStatus, useVersions, type useDiscussions } from '../../api/mergeRequests';
import type { MergeRequest, MergeStatus } from '../../api/types';
import { InlineError } from '../../components/InlineError';
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

/** Required checks: finished with success, finished otherwise, and not
 * finished (no report, still running, or a stale run — never a success). */
export function checkTotals(status: Pick<MergeStatus, 'checks'>): {
  passed: number;
  failed: number;
  pending: number;
} {
  let passed = 0;
  let failed = 0;
  let pending = 0;
  for (const c of status.checks) {
    if (c.status !== 'completed' || c.stale) pending += 1;
    else if (c.conclusion === 'success') passed += 1;
    else failed += 1;
  }
  return { passed, failed, pending };
}

/** The approvals badge: the words carry the state ("1/2 approved, 1 more needed"). */
export function approvalBadge(
  t: TFunction,
  a: { counted: number; required: number },
): { tone: BadgeTone; label: string } {
  if (a.counted >= a.required) return { tone: 'open', label: t('mr.side.approvalsMet', a) };
  return {
    tone: 'draft',
    label: t('mr.side.approvalsNeeded', { ...a, missing: a.required - a.counted }),
  };
}

/** The checks badge: failed first, then not finished, then all passed. */
export function checksBadge(
  t: TFunction,
  c: { passed: number; failed: number; pending: number },
): { tone: BadgeTone; label: string } {
  const total = c.passed + c.failed + c.pending;
  const values = { ...c, total };
  if (c.failed > 0) return { tone: 'unresolved', label: t('mr.side.checksFailed', values) };
  if (c.pending > 0) return { tone: 'draft', label: t('mr.side.checksPending', values) };
  return { tone: 'open', label: t('mr.side.checksPassed', values) };
}

function Row({ term, testId, children }: { term: string; testId: string; children: ReactNode }) {
  return (
    <div className={styles.sideRow}>
      <dt>{term}</dt>
      <dd data-testid={testId}>{children}</dd>
    </div>
  );
}

function Pending() {
  const { t } = useTranslation();
  return <span className={styles.nextHint}>{t('common.loading')}</span>;
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
      <dl className={styles.sideList}>
        {status.isPending && (
          <>
            <Row term={t('mr.side.approvals')} testId="mr-side-approvals">
              <Pending />
            </Row>
            <Row term={t('mr.side.checks')} testId="mr-side-checks">
              <Pending />
            </Row>
          </>
        )}
        {status.isError && (
          <Row term={t('mr.side.conditions')} testId="mr-side-status-error">
            <InlineError
              error={status.error}
              onRetry={() => {
                void status.refetch();
              }}
            />
          </Row>
        )}
        {status.isSuccess && <StatusRows status={status.data} />}
        <Row term={t('mr.side.threads')} testId="mr-side-threads">
          {threads.isPending && <Pending />}
          {threads.isError && (
            <InlineError
              error={threads.error}
              onRetry={() => {
                void threads.refetch();
              }}
            />
          )}
          {threads.isSuccess && (
            <span className={styles.number}>
              {all.length === 0
                ? t('mr.side.threadsNone')
                : t('mr.side.threadsCount', { open, total: all.length })}
            </span>
          )}
        </Row>
        <Row term={t('mr.side.versions')} testId="mr-side-versions">
          {versions.isPending && <Pending />}
          {versions.isError && (
            <InlineError
              error={versions.error}
              onRetry={() => {
                void versions.refetch();
              }}
            />
          )}
          {versions.isSuccess && (
            <>
              <span className={styles.number}>{versions.data.versions.length}</span>
              {latest !== null && (
                <span className={styles.sideNote}>
                  {t('mr.side.latestHead', { sha: shortOid(latest.head_sha) })}
                </span>
              )}
            </>
          )}
        </Row>
        <Row term={t('mr.side.updated')} testId="mr-side-updated">
          <time
            className={styles.number}
            dateTime={mr.updated_at}
            title={formatDateTime(mr.updated_at, i18n.language)}
          >
            {formatRelative(mr.updated_at, i18n.language)}
          </time>
        </Row>
      </dl>
    </aside>
  );
}

function StatusRows({ status }: { status: MergeStatus }) {
  const { t } = useTranslation();
  const approvals = approvalTotals(status);
  const given = status.approvals?.approvals ?? [];
  const checks = checkTotals(status);
  const aBadge = approvals.required === 0 ? null : approvalBadge(t, approvals);
  const cBadge = status.checks.length === 0 ? null : checksBadge(t, checks);
  return (
    <>
      <Row term={t('mr.side.approvals')} testId="mr-side-approvals">
        {aBadge === null ? (
          t('mr.side.approvalsNone')
        ) : (
          <StateBadge tone={aBadge.tone}>
            <span className={styles.number}>{aBadge.label}</span>
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
      </Row>
      <Row term={t('mr.side.checks')} testId="mr-side-checks">
        {cBadge === null ? (
          t('mr.side.checksNone')
        ) : (
          <StateBadge tone={cBadge.tone}>
            <span className={styles.number}>{cBadge.label}</span>
          </StateBadge>
        )}
      </Row>
      {status.work_item_link !== null && (
        <Row term={t('mr.side.workItem')} testId="mr-side-work-item">
          {status.work_item_link.linked ? t('mr.side.linked') : t('mr.side.notLinked')}
        </Row>
      )}
    </>
  );
}
