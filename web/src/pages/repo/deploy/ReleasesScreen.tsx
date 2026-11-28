// A project's signed AI asset releases (`/-/deployments/releases`, plan 17
// P17-7b): per stage (shadow → canary → prod) what runs and the rollback
// it would make; per release its version, tag, commit, bench, signing key,
// its state on each stage, one line saying what comes next, and a
// withdrawal in words. The buttons — request the next stage, roll a stage
// back, withdraw — are shown only where the API says the caller may press
// them and the server would take it. A request waits for a person's
// approval on the deployment screen (linked); nothing is approved here.
import { useEffect, useId, useRef, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import {
  RELEASES_MAX,
  WITHDRAW_REASON_MAX,
  useAiReleases,
  useRequestStage,
  useRollbackStage,
  useWithdrawRelease,
  withdrawReasonValid,
  type Release,
  type ReleaseList,
  type Stage,
} from '../../../api/aiReleases';
import { CopyButton } from '../../../components/CopyButton';
import { Loading } from '../../../components/Loading';
import { QueryError } from '../../../components/QueryError';
import { TextField } from '../../../components/Field';
import controls from '../../../components/controls.module.css';
import { formatDateTime, formatRelative } from '../../../features/labels';
import { releaseLine, releaseTone, stageLabel } from '../../../features/aiReleases';
import { commitUrl, deploymentUrl, deploymentsUrl } from '../../../features/repoUrls';
import { useDocumentTitle } from '../../../hooks/useDocumentTitle';
import { PageHeader } from '../../../ui/PageHeader';
import { RepoError } from '../common';
import { DeployBadge } from './DeployBadge';
import styles from './Deploy.module.css';

export function ReleasesScreen({ projectId, project }: { projectId: number; project: string }) {
  const { t } = useTranslation();
  const list = useAiReleases(projectId);
  const title = t('aiRelease.title');
  useDocumentTitle(title);
  return (
    <>
      <p className={styles.hint}>
        <Link to={deploymentsUrl(project)}>{t('aiRelease.backToDeployments')}</Link>
      </p>
      <PageHeader title={title} subtitle={t('aiRelease.subtitle')} />
      {list.isPending && <Loading />}
      {list.isError && list.data === undefined && (
        <RepoError
          error={list.error}
          onRetry={() => {
            void list.refetch();
          }}
        />
      )}
      {list.data !== undefined && (
        <>
          {list.isError && (
            <QueryError
              error={list.error}
              onRetry={() => {
                void list.refetch();
              }}
            />
          )}
          <Board projectId={projectId} project={project} data={list.data} />
        </>
      )}
    </>
  );
}

function Board({ projectId, project, data }: { projectId: number; project: string; data: ReleaseList }) {
  const { t } = useTranslation();
  return (
    <>
      <section aria-labelledby="ai-release-stages-title">
        <h2 id="ai-release-stages-title" className={styles.sectionTitle}>
          {t('aiRelease.stagesTitle')}
        </h2>
        <ol className={styles.stages} data-testid="ai-release-stages">
          {data.stages.map((s) => (
            <StageCard
              key={s.environment}
              projectId={projectId}
              project={project}
              stage={s}
              canRequest={data.viewer.can_request}
            />
          ))}
        </ol>
      </section>
      <section aria-labelledby="ai-release-list-title">
        <h2 id="ai-release-list-title" className={styles.sectionTitle}>
          {t('aiRelease.listTitle')}
        </h2>
        {data.releases.length === 0 ? (
          <div data-testid="ai-releases-empty" className={styles.empty}>
            <p>{t('aiRelease.empty')}</p>
            <p>{t('aiRelease.emptyAction')}</p>
            <p className={styles.actions}>
              <code>{t('aiRelease.emptyCommand', { id: projectId })}</code>
              <CopyButton text={t('aiRelease.emptyCommand', { id: projectId })} label={t('aiRelease.copyCommand')} />
            </p>
          </div>
        ) : (
          <>
            <ol className={styles.list} data-testid="ai-release-list" aria-label={t('aiRelease.listTitle')}>
              {data.releases.map((r) => (
                <ReleaseItem
                  key={r.id}
                  projectId={projectId}
                  project={project}
                  r={r}
                  canRequest={data.viewer.can_request}
                  canWithdraw={data.viewer.can_withdraw}
                />
              ))}
            </ol>
            {data.releases.length >= RELEASES_MAX && (
              <p className={styles.hint}>{t('aiRelease.capped', { count: RELEASES_MAX })}</p>
            )}
          </>
        )}
      </section>
    </>
  );
}

/** Yes / Cancel under a question; the focus goes to Yes, back to the
 * opener on Cancel. */
function Confirm({
  question,
  yes,
  danger = false,
  onYes,
  onNo,
  children,
}: {
  question: string;
  yes: string;
  danger?: boolean;
  onYes: () => void;
  onNo: () => void;
  children?: ReactNode;
}) {
  const { t } = useTranslation();
  const id = useId();
  const button = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    button.current?.focus();
  }, []);
  return (
    <div role="group" aria-labelledby={id} className={styles.confirm}>
      <p id={id}>{question}</p>
      {children}
      <p className={styles.actions}>
        <button ref={button} type="button" className={danger ? controls.danger : controls.primary} onClick={onYes}>
          {yes}
        </button>
        <button type="button" className={controls.button} onClick={onNo}>
          {t('aiRelease.cancel')}
        </button>
      </p>
    </div>
  );
}

function StageCard({
  projectId,
  project,
  stage,
  canRequest,
}: {
  projectId: number;
  project: string;
  stage: Stage;
  canRequest: boolean;
}) {
  const { t } = useTranslation();
  const rollback = useRollbackStage(projectId);
  const [asking, setAsking] = useState(false);
  const open = useRef<HTMLButtonElement>(null);
  const name = stageLabel(t, stage.environment);
  const target = stage.rollback_version;
  return (
    <li className={styles.stage} data-testid={`ai-stage-${stage.environment}`}>
      <h3 className={styles.stageName}>{name}</h3>
      <p>
        {stage.running_version == null
          ? t('aiRelease.stageNothing')
          : t('aiRelease.stageRunning', { version: stage.running_version })}
      </p>
      {canRequest && target != null && !asking && rollback.data === undefined && (
        <p className={styles.actions}>
          <button
            ref={open}
            type="button"
            className={controls.button}
            disabled={rollback.isPending}
            onClick={() => {
              rollback.reset();
              setAsking(true);
            }}
          >
            {rollback.isPending ? t('aiRelease.sending') : t('aiRelease.rollback', { version: target })}
          </button>
        </p>
      )}
      {stage.running_version != null && target == null && (
        <p className={styles.notYet}>{t('aiRelease.noRollback')}</p>
      )}
      {asking && target != null && (
        <Confirm
          question={t('aiRelease.rollbackConfirm', { stage: name, version: target })}
          yes={t('aiRelease.rollbackYes')}
          onYes={() => {
            setAsking(false);
            rollback.mutate(stage.environment);
          }}
          onNo={() => {
            setAsking(false);
            window.setTimeout(() => open.current?.focus(), 0);
          }}
        />
      )}
      <p role="status" className={styles.result}>
        {rollback.data !== undefined && (
          <>
            {t('aiRelease.rollbackDone', { version: rollback.data.release.version })}{' '}
            <Link to={deploymentUrl(project, rollback.data.deployment.id)}>
              {t('aiRelease.approveThere', { id: rollback.data.deployment.id })}
            </Link>
          </>
        )}
      </p>
      {rollback.isError && <QueryError error={rollback.error} />}
    </li>
  );
}

const TONE = {
  blocked: styles.lineBlocked,
  waiting: styles.lineWaiting,
  next: styles.line,
  done: styles.lineDone,
} as const;

function ReleaseItem({
  projectId,
  project,
  r,
  canRequest,
  canWithdraw,
}: {
  projectId: number;
  project: string;
  r: Release;
  canRequest: boolean;
  canWithdraw: boolean;
}) {
  const { t, i18n } = useTranslation();
  const request = useRequestStage(projectId);
  const withdraw = useWithdrawRelease(projectId);
  const [panel, setPanel] = useState<'request' | 'withdraw' | null>(null);
  const [reason, setReason] = useState('');
  const [tried, setTried] = useState(false);
  const opener = useRef<HTMLButtonElement>(null);
  const headingId = useId();
  const next = r.next_stage;
  const withdrawn = r.withdrawal != null;
  const close = () => {
    setPanel(null);
    window.setTimeout(() => opener.current?.focus(), 0);
  };
  const line = releaseLine(t, r);
  const reasonOk = withdrawReasonValid(reason);
  return (
    <li className={styles.release} aria-labelledby={headingId} data-testid={`ai-release-${r.version}`}>
      <h3 id={headingId} className={styles.releaseTitle}>
        {r.version}
        {withdrawn && <span className={styles.withdrawnMark}>{t('aiRelease.withdrawnMark')}</span>}
      </h3>
      <p className={TONE[releaseTone(r)]} data-tone={releaseTone(r)}>
        {line}
      </p>
      {r.withdrawal != null && (
        <p className={styles.withdrawal} data-testid="ai-release-withdrawal">
          {t('aiRelease.withdrawnReason', { reason: r.withdrawal.reason })}{' '}
          <time dateTime={r.withdrawal.withdrawn_at} title={formatDateTime(r.withdrawal.withdrawn_at, i18n.language)}>
            {formatRelative(r.withdrawal.withdrawn_at, i18n.language)}
          </time>
        </p>
      )}
      <dl className={styles.facts}>
        <dt>{t('aiRelease.tag')}</dt>
        <dd>
          <code>{r.tag}</code>
        </dd>
        <dt>{t('aiRelease.commit')}</dt>
        <dd>
          <Link to={commitUrl(project, r.commit_sha)} aria-label={t('aiRelease.openCommit', { sha: r.commit_sha.slice(0, 8) })}>
            <code>{r.commit_sha.slice(0, 8)}</code>
          </Link>
        </dd>
        <dt>{t('aiRelease.bench')}</dt>
        <dd>{t('aiRelease.benchPassed', { id: r.bench_check_run_id })}</dd>
        <dt>{t('aiRelease.key')}</dt>
        <dd>
          {t('aiRelease.keyId', { kid: r.key_id })}
          {r.key_revoked === true && <> {t('aiRelease.keyRevoked')}</>}
        </dd>
        <dt>{t('aiRelease.files')}</dt>
        <dd>{t('aiRelease.fileCount', { count: r.files })}</dd>
        <dt>{t('aiRelease.made')}</dt>
        <dd>
          <time dateTime={r.created_at} title={formatDateTime(r.created_at, i18n.language)}>
            {formatRelative(r.created_at, i18n.language)}
          </time>
        </dd>
      </dl>
      <ul className={styles.releaseStages} aria-label={t('aiRelease.stagesOf', { version: r.version })}>
        {(r.stages ?? []).map((s) => (
          <li key={s.environment}>
            <span className={styles.stageWord}>{stageLabel(t, s.environment)}</span>{' '}
            {s.status == null ? (
              <span className={styles.notYet}>{t('aiRelease.notDeployed')}</span>
            ) : (
              <>
                <DeployBadge status={s.status} />
                {s.deployment_id != null && (
                  <>
                    {' '}
                    <Link to={deploymentUrl(project, s.deployment_id)}>
                      {s.status === 'pending_approval'
                        ? t('aiRelease.approveThere', { id: s.deployment_id })
                        : t('aiRelease.deployment', { id: s.deployment_id })}
                    </Link>
                  </>
                )}
                {s.approved_success && <> {t('aiRelease.approvedSuccess')}</>}
              </>
            )}
          </li>
        ))}
      </ul>
      {panel === null && ((canRequest && next != null) || (canWithdraw && !withdrawn)) && (
        <p className={styles.actions}>
          {canRequest && next != null && (
            <button
              ref={opener}
              type="button"
              className={controls.primary}
              disabled={request.isPending}
              onClick={() => {
                request.reset();
                setPanel('request');
              }}
            >
              {request.isPending ? t('aiRelease.sending') : t('aiRelease.request', { stage: stageLabel(t, next) })}
            </button>
          )}
          {canWithdraw && !withdrawn && (
            <button
              ref={canRequest && next != null ? undefined : opener}
              type="button"
              className={controls.ghost}
              disabled={withdraw.isPending}
              onClick={() => {
                withdraw.reset();
                setTried(false);
                setPanel('withdraw');
              }}
            >
              {t('aiRelease.withdraw')}
            </button>
          )}
        </p>
      )}
      {panel === 'request' && next != null && (
        <Confirm
          question={t('aiRelease.requestConfirm', { version: r.version, stage: stageLabel(t, next) })}
          yes={t('aiRelease.requestYes')}
          onYes={() => {
            setPanel(null);
            request.mutate({ version: r.version, environment: next });
          }}
          onNo={close}
        />
      )}
      {panel === 'withdraw' && (
        <Confirm
          question={t('aiRelease.withdrawConfirm', { version: r.version })}
          yes={t('aiRelease.withdrawYes')}
          danger
          onYes={() => {
            setTried(true);
            if (!reasonOk) return;
            setPanel(null);
            withdraw.mutate({ version: r.version, reason: reason.trim() });
          }}
          onNo={close}
        >
          <TextField
            label={t('aiRelease.reasonLabel')}
            value={reason}
            onChange={setReason}
            required
            help={t('aiRelease.reasonHelp', { max: WITHDRAW_REASON_MAX })}
            error={tried && !reasonOk ? t('aiRelease.reasonInvalid', { max: WITHDRAW_REASON_MAX }) : null}
          />
        </Confirm>
      )}
      <p role="status" className={styles.result}>
        {request.data !== undefined && (
          <>
            {t('aiRelease.requested', { stage: stageLabel(t, request.variables.environment) })}{' '}
            <Link to={deploymentUrl(project, request.data.id)}>{t('aiRelease.approveThere', { id: request.data.id })}</Link>
          </>
        )}
        {withdraw.data !== undefined && t('aiRelease.withdrawn', { version: withdraw.data.version })}
      </p>
      {request.isError && <QueryError error={request.error} />}
      {withdraw.isError && <QueryError error={withdraw.error} />}
    </li>
  );
}

