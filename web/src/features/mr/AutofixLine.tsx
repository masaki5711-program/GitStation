// The deterministic autofix on the merge request screen (M4 C14, ADR-0017
// Amendment C14; plan 08): one line saying what was fixed automatically, or
// why it was not, and what to do next. The error code is never shown (the
// request id of the decision is, for a support question).
import type { TFunction } from 'i18next';
import { useTranslation } from 'react-i18next';

import { useMergeRequestAutofix, type AutofixAttempt } from '../../api/autofix';
import styles from './Mr.module.css';

/** The kind of line an attempt is shown as. */
export type AutofixKind = 'running' | 'applied' | 'proposed' | 'refused' | 'failed' | 'denied';

/** The line for an attempt (null: nothing worth a line). */
export function autofixKind(a: AutofixAttempt | null): AutofixKind | null {
  if (a === null) return null;
  switch (a.state) {
    case 'queued':
    case 'running':
      return 'running';
    case 'applied':
      return 'applied';
    case 'proposed':
      return 'proposed';
    case 'refused':
      return 'refused';
    case 'failed':
      return 'failed';
    case 'skipped':
      return a.reason === 'denied' ? 'denied' : null;
    default:
      // no_change, superseded: nothing changed on the merge request.
      return null;
  }
}

/** The line's text: what happened and what to do next. */
export function autofixText(t: TFunction, kind: AutofixKind, a: AutofixAttempt): string {
  const fixer = a.fixer ?? '';
  const paths = a.paths.map((p) => p.path).join(', ');
  switch (kind) {
    case 'running':
      return t('autofix.line.running', { fixer });
    case 'applied':
      return t('autofix.line.applied', { fixer, sha: (a.commit_sha ?? '').slice(0, 8) });
    case 'proposed':
      return t('autofix.line.proposed', { fixer, paths });
    case 'refused':
      return t('autofix.line.refused', { paths, requestId: a.request_id });
    case 'failed':
      return t('autofix.line.failed', { fixer });
    case 'denied':
      return t('autofix.line.denied');
  }
}

const WARNINGS: readonly AutofixKind[] = ['proposed', 'refused', 'failed', 'denied'];

export function AutofixLine({ projectId, iid, open }: { projectId: number; iid: number; open: boolean }) {
  const { t } = useTranslation();
  const read = useMergeRequestAutofix(projectId, iid, open);
  // The line is advice: a failed read shows nothing.
  if (read.data === undefined) return null;
  const a = read.data.attempt;
  const kind = autofixKind(a);
  if (kind === null || a === null) return null;
  return (
    <p
      role="status"
      className={WARNINGS.includes(kind) ? styles.autoMergeStopped : styles.nextHint}
      data-testid="mr-autofix"
      data-kind={kind}
    >
      <strong>{t('autofix.label')}</strong> {autofixText(t, kind, a)}
    </p>
  );
}
