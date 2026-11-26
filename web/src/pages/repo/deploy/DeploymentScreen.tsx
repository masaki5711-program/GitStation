// One deployment (`/-/deployments/{id}`): the one line saying why it waits
// and what to press next (plan 08), the approval's facts made by code
// (always, in full), the local LLM's summary apart from them as advice
// (plan 19 §3.4 / §4), and Approve for someone who may approve it. The
// approval is this web session's, a person's (the server refuses a token
// and, for prod, anything but a person); it never depends on the summary —
// the button is the same whether there is a summary, none, a stale one or
// a withheld one. Read again while it waits and the summary has not come.
import { useQuery } from '@tanstack/react-query';
import type { TFunction } from 'i18next';
import { useEffect, useId, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { useDeployApprovers } from '../../../api/approvers';
import { useApi } from '../../../api/context';
import {
  useApprovalSummary,
  useApproveDeployment,
  type ApprovalSummary,
  type Deployment,
} from '../../../api/deployments';
import { projectQuery } from '../../../api/namespaces';
import { Loading } from '../../../components/Loading';
import { QueryError } from '../../../components/QueryError';
import controls from '../../../components/controls.module.css';
import {
  approversOf,
  approverStanding,
  decisionKey,
  deployStatusLabel,
  environmentLabel,
  offersApprove,
  type ApproverStanding,
} from '../../../features/deployApproval';
import { formatDateTime, formatRelative } from '../../../features/labels';
import { commitUrl, deploymentsUrl } from '../../../features/repoUrls';
import { useDocumentTitle } from '../../../hooks/useDocumentTitle';
import { useSession } from '../../../session/session';
import { PageHeader } from '../../../ui/PageHeader';
import { RepoError } from '../common';
import { AdviceSummary } from './AdviceSummary';
import { ApprovalFacts } from './ApprovalFacts';
import { DeployBadge } from './DeployBadge';
import styles from './Deploy.module.css';

export function DeploymentScreen({ projectId, project, id }: { projectId: number; project: string; id: number }) {
  const { t } = useTranslation();
  const summary = useApprovalSummary(projectId, id);
  const title = t('deploy.number', { id });
  useDocumentTitle(title);
  const retry = () => {
    void summary.refetch();
  };
  if (summary.data === undefined) {
    if (summary.isError) {
      return (
        <>
          <PageHeader title={title} />
          <RepoError error={summary.error} onRetry={retry} />
          <p>
            <Link to={deploymentsUrl(project)}>{t('deploy.backToList')}</Link>
          </p>
        </>
      );
    }
    return <Loading />;
  }
  // A failed read again (a poll, a re-check) keeps what was shown, with the
  // failure above it: the approver does not lose the facts mid-decision.
  const recheck = () => summary.refetch().then((r) => (r.isError ? undefined : r.data));
  return (
    <>
      {summary.isError && <QueryError error={summary.error} onRetry={retry} />}
      <DeploymentView projectId={projectId} project={project} s={summary.data} title={title} recheck={recheck} />
    </>
  );
}

function DeploymentView({
  projectId,
  project,
  s,
  title,
  recheck,
}: {
  projectId: number;
  project: string;
  s: ApprovalSummary;
  title: string;
  recheck: () => Promise<ApprovalSummary | undefined>;
}) {
  const { t, i18n } = useTranslation();
  const api = useApi();
  const me = useSession().user.id;
  // The same query (and key) as the project's frame: no second request.
  const info = useQuery(projectQuery(api, projectId));
  const approvers = useDeployApprovers(projectId);
  const [approved, setApproved] = useState<Deployment | null>(null);
  const d = s.deployment;
  const env = approversOf(approvers.data?.environments, d.environment);
  const standing = approverStanding(d, me, info.data?.role, env);
  // Known once the approvers are read (an environment without a row: one).
  const required: number | null = approvers.isSuccess ? (env?.approvals_required ?? 1) : null;
  const status = deployStatusLabel(t, d.status);
  // Offered only once the role and the approvers are read: never on a guess.
  const offer = approvers.isSuccess && info.isSuccess && offersApprove(standing);

  return (
    <>
      <p className={styles.hint}>
        <Link to={deploymentsUrl(project)}>{t('deploy.backToList')}</Link>
      </p>
      <PageHeader
        title={
          <>
            {title} <DeployBadge status={d.status} />
          </>
        }
      >
        <p className={styles.meta}>
          <span data-testid="deploy-env">{environmentLabel(t, d.environment)}</span>
          <Link to={commitUrl(project, d.sha)} aria-label={t('deploy.openCommit', { sha: d.sha.slice(0, 8) })}>
            <code>{d.sha.slice(0, 8)}</code>
          </Link>
          {d.commit_subject != null && <span className={styles.subject}>{d.commit_subject}</span>}
          <time dateTime={d.created_at} title={formatDateTime(d.created_at, i18n.language)}>
            {formatRelative(d.created_at, i18n.language)}
          </time>
        </p>
      </PageHeader>
      {/* The status alone, announced when it changes (not the whole line). */}
      <p role="status" className={styles.srOnly}>
        {t('deploy.statusNow', { status })}
      </p>
      <StatusLine d={d} standing={standing} required={required} />

      <ApprovalFacts project={project} sha={d.sha} facts={s.facts} />
      <AdviceSummary advice={s.summary} />

      {(offer || approved !== null) && (
        <ApprovePanel
          key={d.id}
          projectId={projectId}
          s={s}
          required={required}
          recheck={recheck}
          approved={approved}
          onApproved={setApproved}
        />
      )}
      {approvers.isError && d.status === 'pending_approval' && (
        <section aria-labelledby="deploy-approvers-error-title" data-testid="deploy-approvers-error">
          <h2 id="deploy-approvers-error-title" className={styles.sectionTitle}>
            {t('deploy.approve.title')}
          </h2>
          <QueryError
            error={approvers.error}
            onRetry={() => {
              void approvers.refetch();
            }}
          />
        </section>
      )}
    </>
  );
}

function StatusLine({
  d,
  standing,
  required,
}: {
  d: Deployment;
  standing: ApproverStanding;
  required: number | null;
}) {
  const { t } = useTranslation();
  const tone =
    d.status === 'pending_approval'
      ? styles.statusWaiting
      : d.status === 'failed' || d.status === 'rollback_failed' || d.status === 'blocked'
        ? styles.statusFailed
        : styles.statusDone;
  const need = required ?? '?';
  let line: string;
  switch (standing) {
    case 'approver':
    case 'maybe':
      line = t('deploy.line.approve', { given: d.approvals, required: need });
      break;
    case 'requester':
      line = t('deploy.line.requester');
      break;
    case 'not_approver':
      line = t('deploy.line.notApprover', { given: d.approvals, required: need });
      break;
    default:
      line = settledLine(t, d.status);
  }
  return (
    <div className={`${styles.statusLine ?? ''} ${tone ?? ''}`} data-testid="deploy-status-line">
      <p>{line}</p>
      {d.error_code != null && (
        <p>
          {t('deploy.line.errorCode')} <code>{d.error_code}</code>
          {d.failed_target != null && (
            <>
              {' '}
              {t('deploy.line.failedTarget', { target: d.failed_target })}
            </>
          )}
        </p>
      )}
    </div>
  );
}

function settledLine(t: TFunction, status: string): string {
  switch (status) {
    case 'queued':
      return t('deploy.line.queued');
    case 'running':
      return t('deploy.line.running');
    case 'succeeded':
      return t('deploy.line.succeeded');
    case 'failed':
      return t('deploy.line.failed');
    case 'rolled_back':
      return t('deploy.line.rolled_back');
    case 'rollback_failed':
      return t('deploy.line.rollback_failed');
    case 'blocked':
      return t('deploy.line.blocked');
    default:
      return t('deploy.line.other');
  }
}

/** Approve, behind one confirmation. Pressing it first reads the facts
 * again: when the deployment or its facts changed since they were shown,
 * the new ones are shown and nothing is asked until it is pressed again.
 * The question repeats what weighs most (a freeze, failed checks, an
 * unread range). Stays mounted after the last approval so its result is
 * announced. */
function ApprovePanel({
  projectId,
  s,
  required,
  recheck,
  approved,
  onApproved,
}: {
  projectId: number;
  s: ApprovalSummary;
  required: number | null;
  recheck: () => Promise<ApprovalSummary | undefined>;
  approved: Deployment | null;
  onApproved: (d: Deployment) => void;
}) {
  const { t } = useTranslation();
  const d = s.deployment;
  const approve = useApproveDeployment(projectId, d.id);
  const [asking, setAsking] = useState(false);
  const [checking, setChecking] = useState(false);
  const [changed, setChanged] = useState(false);
  const yes = useRef<HTMLButtonElement>(null);
  const open = useRef<HTMLButtonElement>(null);
  const questionId = useId();
  const sha = d.sha.slice(0, 8);
  const env = environmentLabel(t, d.environment);
  const busy = approve.isPending || checking;
  const waiting = d.status === 'pending_approval';
  const f = s.facts;

  useEffect(() => {
    if (asking) yes.current?.focus();
  }, [asking]);

  const start = () => {
    approve.reset();
    setChanged(false);
    setChecking(true);
    const shown = decisionKey(s);
    void recheck().then((fresh) => {
      setChecking(false);
      // A failed read shows its failure above the page; nothing is asked.
      if (fresh === undefined) return;
      if (decisionKey(fresh) !== shown) {
        setChanged(true);
        return;
      }
      setAsking(true);
    });
  };
  const confirm = () => {
    setAsking(false);
    approve.mutate(undefined, {
      onSuccess: (after) => {
        onApproved(after);
      },
    });
  };
  return (
    <section aria-labelledby="deploy-approve-title" data-testid="deploy-approve" aria-busy={busy}>
      <h2 id="deploy-approve-title" className={styles.sectionTitle}>
        {t('deploy.approve.title')}
      </h2>
      {waiting && approved === null && (
        <>
          <p className={styles.hint}>{t('deploy.approve.lead')}</p>
          <p className={styles.actions}>
            <button ref={open} type="button" className={controls.primary} disabled={busy || asking} onClick={start}>
              {approve.isPending
                ? t('deploy.approve.sending')
                : checking
                  ? t('deploy.approve.checking')
                  : t('deploy.approve.button')}
            </button>
          </p>
        </>
      )}
      {changed && (
        <p className={styles.warning} role="alert" data-testid="approve-changed">
          {t('deploy.approve.changed')}
        </p>
      )}
      {asking && (
        <div role="group" aria-labelledby={questionId} className={styles.confirm} data-testid="approve-confirm">
          <p id={questionId}>{t('deploy.approve.confirm', { env, sha })}</p>
          {f.frozen_now && <p className={styles.warning}>{t('deploy.approve.warnFrozen')}</p>}
          {f.failed_checks.length > 0 && (
            <p className={styles.warning}>{t('deploy.approve.warnChecks', { count: f.failed_checks.length })}</p>
          )}
          {f.range === 'unavailable' && <p className={styles.warning}>{t('deploy.approve.warnRange')}</p>}
          <p className={styles.actions}>
            <button ref={yes} type="button" className={controls.primary} onClick={confirm}>
              {t('deploy.approve.yes')}
            </button>
            <button
              type="button"
              className={controls.button}
              onClick={() => {
                setAsking(false);
                window.setTimeout(() => open.current?.focus(), 0);
              }}
            >
              {t('deploy.approve.no')}
            </button>
          </p>
        </div>
      )}
      <p role="status" data-testid="approve-result">
        {approved !== null &&
          (approved.status === 'pending_approval'
            ? t('deploy.approve.doneMore', { given: approved.approvals, required: required ?? '?' })
            : t('deploy.approve.done'))}
      </p>
      {approve.isError && <QueryError error={approve.error} />}
    </section>
  );
}
