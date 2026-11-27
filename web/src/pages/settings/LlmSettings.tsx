// A project's AI first-pass review (plan 19 §3.1 / §8.2, ADR-0039):
// llm.review is off (the default), advisory or required. Everyone who reads
// the project sees it; a Maintainer changes it in a web session (never an
// agent, a bot or a service account). The control comes from `viewer` only;
// the server decides again. The advice never approves or blocks a merge.
import type { TFunction } from 'i18next';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';

import {
  isLlmReviewMode,
  LLM_REVIEW_MODES,
  useLlmSetting,
  useSetLlmSetting,
  type LlmReviewMode,
  type LlmSetting,
} from '../../api/llm';
import { RadioField } from '../../components/Field';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { formatDateTime } from '../../features/labels';
import { Denied, Section } from './common';
import styles from './Settings.module.css';

export function llmModeLabel(t: TFunction, mode: string): string {
  switch (mode) {
    case 'advisory':
      return t('settings.llm.advisory');
    case 'required':
      return t('settings.llm.required');
    default:
      return t('settings.llm.off');
  }
}

function modeHelp(t: TFunction, mode: LlmReviewMode): string {
  switch (mode) {
    case 'advisory':
      return t('settings.llm.advisoryHelp');
    case 'required':
      return t('settings.llm.requiredHelp');
    case 'off':
      return t('settings.llm.offHelp');
  }
}

export function LlmSection({ projectId }: { projectId: number }) {
  const { t } = useTranslation();
  const read = useLlmSetting(projectId);
  return (
    <Section id="llm-title" title={t('settings.llm.title')}>
      <p className={styles.hint}>{t('settings.llm.intro')}</p>
      {read.isPending && <Loading />}
      {read.isError && (
        <QueryError
          error={read.error}
          onRetry={() => {
            void read.refetch();
          }}
        />
      )}
      {read.isSuccess && <LlmBody projectId={projectId} s={read.data} />}
    </Section>
  );
}

function LlmBody({ projectId, s }: { projectId: number; s: LlmSetting }) {
  const { t, i18n } = useTranslation();
  const save = useSetLlmSetting(projectId);
  const [mode, setDraft] = useState<LlmReviewMode>(isLlmReviewMode(s.review) ? s.review : 'off');
  const [message, setMessage] = useState('');
  const busy = save.isPending;
  return (
    <>
      <p data-testid="llm-state">{t('settings.llm.current', { mode: llmModeLabel(t, s.review) })}</p>
      {s.changed_at !== null && (
        <p className={styles.muted}>{t('settings.llm.changedAt', { at: formatDateTime(s.changed_at, i18n.language) })}</p>
      )}
      {s.viewer.can_change ? (
        <form
          noValidate
          className={styles.narrow}
          onSubmit={(e) => {
            e.preventDefault();
            if (busy) return;
            setMessage('');
            save.mutate(mode, {
              onSuccess: (next) => {
                setMessage(t('settings.llm.saved', { mode: llmModeLabel(t, next.review) }));
              },
            });
          }}
        >
          <RadioField
            legend={t('settings.llm.legend')}
            name={`llm-review-${String(projectId)}`}
            value={mode}
            onChange={setDraft}
            choices={LLM_REVIEW_MODES.map((m) => ({ value: m, label: llmModeLabel(t, m), help: modeHelp(t, m) }))}
          />
          <button type="submit" className={controls.button} aria-disabled={busy ? 'true' : undefined}>
            {t('settings.llm.save')}
          </button>
        </form>
      ) : (
        <p className={styles.hint}>{t('settings.llm.maintainerOnly')}</p>
      )}
      <p className={styles.hint}>{t('settings.llm.offKeeps')}</p>
      <p role="status" className={styles.muted}>
        {message}
      </p>
      {save.isError && <Denied error={save.error} />}
    </>
  );
}
