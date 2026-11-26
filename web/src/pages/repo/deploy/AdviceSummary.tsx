// The local LLM's approval summary, apart from the facts (plan 19 §3.4 /
// §4, L5-6): labelled as AI advice in words and a glyph, with the model,
// the prompt version and when it was made; a stale one says so and why; a
// less trustworthy one says so; none says the approval does not need it.
// The text is plain text (never HTML, never Markdown). Nothing here can
// approve: the advice has no controls at all.
import { useTranslation } from 'react-i18next';

import type { SummaryAdvice } from '../../../api/deployments';
import { adviceState, staleReasonText } from '../../../features/deployApproval';
import { formatDateTime } from '../../../features/labels';
import styles from './Deploy.module.css';

export function AdviceSummary({ advice }: { advice: SummaryAdvice }) {
  const { t, i18n } = useTranslation();
  const state = adviceState(advice);
  return (
    <section aria-labelledby="deploy-advice-title" data-testid="deploy-advice" data-state={state}>
      <h2 id="deploy-advice-title" className={styles.sectionTitle}>
        {t('deploy.advice.title')}
      </h2>
      <div className={styles.advice}>
        <p className={styles.adviceLabel}>{t('deploy.advice.label')}</p>
        {state === 'shown' && (
          <>
            {advice.stale && (
              <p className={styles.warning} data-testid="advice-stale">
                {t('deploy.advice.staleLead')} {staleReasonText(t, advice.stale_reason)}
              </p>
            )}
            {advice.degraded === true && (
              <p className={styles.warning} data-testid="advice-degraded">
                {t('deploy.advice.degraded')}
              </p>
            )}
            {/* Made in Japanese only for now (ADR-0039 amendment L5-8, decision 3). */}
            <p className={styles.adviceText} lang="ja" data-testid="advice-text">
              {advice.text}
            </p>
          </>
        )}
        {state === 'withheld' && <p data-testid="advice-withheld">{t('deploy.advice.withheld')}</p>}
        {state === 'none' && <p data-testid="advice-none">{t('deploy.advice.none')}</p>}
        {state !== 'none' && (
          <p className={styles.adviceMeta}>
            {advice.model != null && <span>{t('deploy.advice.model', { model: advice.model })}</span>}
            {advice.prompt_version != null && (
              <span>{t('deploy.advice.prompt', { version: advice.prompt_version })}</span>
            )}
            {advice.created_at != null && (
              <time dateTime={advice.created_at}>
                {t('deploy.advice.made', { at: formatDateTime(advice.created_at, i18n.language) })}
              </time>
            )}
          </p>
        )}
        <p className={styles.adviceMeta}>{t('deploy.advice.notRequired')}</p>
      </div>
    </section>
  );
}
