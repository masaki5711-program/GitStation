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
  if (summary.isPending) return <Loading />;
  if (summary.isError) {
    return (
      <>
        <PageHeader title={title} />
        <RepoError
          error={summary.error}
          onRetry={() => {
            void summary.refetch();
          }}
        />
        <p>
          <Link to={deploymentsUrl(project)}>{t('deploy.backToList')}</Link>
        </p>
      </>
    );
  }
  return <DeploymentView projectId={projectId} project={project} s={summary.data} title={title} />;
}

function DeploymentView({
  projectId,
  project,
  s,
  title,
}: {
  projectId: number;
  project: string;
  s: ApprovalSummary;
  title: string;
}) {
  const { t, i18n } = useTranslation();
  const api = useApi();
  const me = useSession().user.id;
  // The same query (and key) as the project's frame: no second request.
  const info = useQuery(projectQuery(api, projectId));
  const approvers = useDeployApprovers(projectId);
  const d = s.deployment;
  const env = approversOf(approvers.data?.environments, d.environment);
  const standing = approverStanding(d, me, info.data?.role, env);
  const required = env?.approvals_required ?? 1;
  const status = deployStatusLabel(t, d.status);

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

      {offersApprove(standing) && (
        <ApprovePanel key={d.id} projectId={projectId} d={d} required={required} />
      )}
    </>
  );
}

function StatusLine({ d, standing, required }: { d: Deployment; standing: ApproverStanding; required: number }) {
  const { t } = useTranslation();
  const tone =
    d.status === 'pending_approval'
      ? styles.statusWaiting
      : d.status === 'failed' || d.status === 'rollback_failed' || d.status === 'blocked'
        ? styles.statusFailed
        : styles.statusDone;
  let line: string;
  switch (standing) {
    case 'approver':
    case 'maybe':
      line = t('deploy.line.approve', { given: d.approvals, required });
      break;
    case 'requester':
      line = t('deploy.line.requester');
      break;
    case 'not_approver':
      line = t('deploy.line.notApprover', { given: d.approvals, required });
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

function ApprovePanel({ projectId, d, required }: { projectId: number; d: Deployment; required: number }) {
  const { t } = useTranslation();
  const approve = useApproveDeployment(projectId, d.id);
  const [asking, setAsking] = useState(false);
  const [done, setDone] = useState<Deployment | null>(null);
  const yes = useRef<HTMLButtonElement>(null);
  const open = useRef<HTMLButtonElement>(null);
  const questionId = useId();
  const sha = d.sha.slice(0, 8);
  const env = environmentLabel(t, d.environment);

  useEffect(() => {
    if (asking) yes.current?.focus();
  }, [asking]);

  const confirm = () => {
    setAsking(false);
    approve.mutate(undefined, { onSuccess: (after) => { setDone(after); } });
  };
  return (
    <section aria-labelledby="deploy-approve-title" data-testid="deploy-approve" aria-busy={approve.isPending}>
      <h2 id="deploy-approve-title" className={styles.sectionTitle}>
        {t('deploy.approve.title')}
      </h2>
      <p className={styles.hint}>{t('deploy.approve.lead')}</p>
      <p className={styles.actions}>
        <button
          ref={open}
          type="button"
          className={controls.primary}
          disabled={approve.isPending || asking || done !== null}
          onClick={() => {
            approve.reset();
            setDone(null);
            setAsking(true);
          }}
        >
          {approve.isPending ? t('deploy.approve.sending') : t('deploy.approve.button')}
        </button>
      </p>
      {asking && (
        <div role="group" aria-labelledby={questionId} className={styles.confirm} data-testid="approve-confirm">
          <p id={questionId}>{t('deploy.approve.confirm', { env, sha })}</p>
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
        {done !== null &&
          (done.status === 'pending_approval'
            ? t('deploy.approve.doneMore', { given: done.approvals, required })
            : t('deploy.approve.done'))}
      </p>
      {approve.isError && <QueryError error={approve.error} />}
    </section>
  );
}
