// The merge conditions of one merge request (U2, plan 08 §1.6: the summary
// line first, the detail for whoever opens it): every reason of the status
// contract in the server's order, the approval counts with why an approval
// does not count, the required checks, and the actions the viewer flags allow
// (approve / withdraw / merge) and the progress of a merge. A button is shown only when the server said
// the request would be allowed (`viewer`), never guessed from a role; the
// server decides again when it is pressed.
import type { TFunction } from 'i18next';
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { useApprove, useMergeStatus, useUnapprove } from '../../api/mergeRequests';
import type { ApprovalAnswer, MergeStatus } from '../../api/types';
import controls from '../../components/controls.module.css';
import { formatDateTime } from '../labels';
import { ActionError } from './ActionError';
import { reasonLine, uncountedKey } from './blockedReason';
import { MergeButton, MergeProgress } from './MergeControl';
import styles from './Mr.module.css';

export function ConditionsPanel({
  projectId,
  iid,
  sourceBranch,
}: {
  projectId: number;
  iid: number;
  sourceBranch: string;
}) {
  const { t } = useTranslation();
  const status = useMergeStatus(projectId, iid);
  /** When the screen began following a merge attempt (null: not following). */
  const [since, setSince] = useState<number | null>(null);
  const [startedHere, setStartedHere] = useState(false);
  const running = status.data?.reasons[0]?.reason === 'merge_in_progress';
  // A merge already running when the page opens (or started elsewhere) is followed too.
  useEffect(() => {
    if (running && since === null) setSince(Date.now());
  }, [running, since]);
  return (
    <section aria-labelledby="mr-conditions-title" className={styles.conditions}>
      <h2 id="mr-conditions-title" tabIndex={-1} className={styles.conditionsTitle}>
        {t('mr.conditions.title')}
      </h2>
      {/* The status line above already shows a failed or pending read. */}
      {status.isSuccess && (
        <>
          <Reasons status={status.data} />
          <Approvals status={status.data} />
          <Checks status={status.data} />
          <Actions
            projectId={projectId}
            iid={iid}
            status={status.data}
            sourceBranch={sourceBranch}
            onMergeStarted={() => {
              setStartedHere(true);
              setSince(Date.now());
            }}
          />
        </>
      )}
      <MergeProgress projectId={projectId} iid={iid} since={since} focusOnStart={startedHere} />
    </section>
  );
}

function Reasons({ status }: { status: MergeStatus }) {
  const { t } = useTranslation();
  if (status.mergeable) return <p className={styles.conditionsMet}>{t('mr.conditions.met')}</p>;
  return (
    <ul className={styles.reasonList} data-testid="mr-conditions">
      {status.reasons.map((r, i) => {
        const line = reasonLine(r);
        // Reasons may repeat (two rules); the position keeps the keys apart.
        return <li key={`${String(i)}-${r.reason}`}>{t(line.key, line.values)}</li>;
      })}
    </ul>
  );
}

function Approvals({ status }: { status: MergeStatus }) {
  const { t, i18n } = useTranslation();
  const a = status.approvals;
  const rules = a?.rules.filter((r) => r.applies) ?? [];
  const given = a?.approvals ?? [];
  const suspended = !status.codeowners.enforced && status.codeowners.suspended_until !== null;
  if (rules.length === 0 && given.length === 0 && !suspended) return null;
  return (
    <>
      <h3 className={styles.conditionsSub}>{t('mr.conditions.approvals')}</h3>
      {rules.length > 0 && (
        <ul className={styles.plainList}>
          {rules.map((r) => (
            <li key={r.name}>
              {r.name === '@protected_branch'
                ? t('mr.conditions.ruleProtected', { counted: r.counted, required: r.required })
                : t('mr.conditions.rule', { name: r.name, counted: r.counted, required: r.required })}
            </li>
          ))}
        </ul>
      )}
      {given.length > 0 && (
        <ul className={styles.plainList} aria-label={t('mr.conditions.given', { count: given.length })}>
          {given.map((g) => (
            <li key={g.approver_id}>
              {g.counted
                ? t('mr.conditions.counts', { name: g.username })
                : t('mr.conditions.notCounted', {
                    name: g.username,
                    why: t(uncountedKey(g.uncounted_reason)),
                  })}
            </li>
          ))}
        </ul>
      )}
      {suspended && (
        <p className={styles.nextHint}>
          {t('mr.conditions.codeownersSuspended', {
            until: formatDateTime(status.codeowners.suspended_until ?? '', i18n.language),
          })}
        </p>
      )}
    </>
  );
}

function Checks({ status }: { status: MergeStatus }) {
  const { t } = useTranslation();
  if (status.checks.length === 0) return null;
  return (
    <>
      <h3 className={styles.conditionsSub}>{t('mr.conditions.checks')}</h3>
      <ul className={styles.plainList}>
        {status.checks.map((c) => (
          <li key={c.name}>
            {c.status === null
              ? t('mr.conditions.checkNone', { name: c.name })
              : t('mr.conditions.checkRun', {
                  name: c.name,
                  status: c.status,
                  conclusion: c.conclusion ?? '-',
                })}
          </li>
        ))}
      </ul>
    </>
  );
}

function answerText(t: TFunction, a: ApprovalAnswer): string {
  if (a.outcome === 'withdrawn') return t('mr.actions.withdrawn');
  if (a.outcome === 'unchanged') return t('mr.actions.unchanged');
  if (a.counted) return t('mr.actions.approved');
  return t('mr.actions.approvedUncounted', { why: t(uncountedKey(a.uncounted_reason)) });
}

function Actions({
  projectId,
  iid,
  status,
  sourceBranch,
  onMergeStarted,
}: {
  projectId: number;
  iid: number;
  status: MergeStatus;
  sourceBranch: string;
  onMergeStarted: () => void;
}) {
  const { t } = useTranslation();
  const approve = useApprove(projectId, iid);
  const unapprove = useUnapprove(projectId, iid);
  const [result, setResult] = useState('');
  const v = status.viewer;
  const head = status.head_sha;
  // `can_unapprove` pairs with `approved_by_me` (docs/api/merge-status.md).
  const mayApprove = v.can_approve && !v.approved_by_me && head !== null;
  const mayWithdraw = v.can_unapprove && v.approved_by_me;
  const busy = approve.isPending || unapprove.isPending;
  const failed = approve.isError ? approve.error : unapprove.isError ? unapprove.error : null;
  const mayMerge = v.can_merge && status.mergeable && head !== null;

  const onApproval = () => {
    if (busy) return;
    setResult('');
    approve.reset();
    unapprove.reset();
    if (mayWithdraw) {
      unapprove.mutate(undefined, { onSuccess: (a) => { setResult(answerText(t, a)); } });
    } else if (mayApprove) {
      approve.mutate(head, { onSuccess: (a) => { setResult(answerText(t, a)); } });
    }
  };

  return (
    <>
      {(mayApprove || mayWithdraw || mayMerge) && (
        <div className={styles.actions} role="group" aria-label={t('mr.actions.label')}>
          {(mayApprove || mayWithdraw) && (
            // One button in one place: after approving, the same element
            // becomes "withdraw", so the focus stays where it was.
            <button
              type="button"
              className={mayWithdraw ? controls.button : controls.primary}
              aria-disabled={busy ? 'true' : undefined}
              onClick={onApproval}
            >
              {mayWithdraw ? t('mr.actions.withdraw') : t('mr.actions.approve')}
            </button>
          )}
          <MergeButton
            projectId={projectId}
            iid={iid}
            status={status}
            sourceBranch={sourceBranch}
            onStarted={onMergeStarted}
          />
        </div>
      )}
      <p role="status" className={styles.nextHint} data-testid="mr-action-result">
        {result}
      </p>
      {failed !== null && (
        <ActionError
          projectId={projectId}
          iid={iid}
          error={failed}
          onReload={() => {
            approve.reset();
            unapprove.reset();
          }}
        />
      )}
    </>
  );
}
