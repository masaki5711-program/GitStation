// A project's AI assets switch (plan 17 P17-3, ADR-0035): ai_assets.enabled
// turns on the AI asset lint, the forge/risk rules for evaluation cases and
// AI asset changes, and asset releases — for this project only. Off by
// default. Everyone who reads the project sees the state; a Maintainer
// changes it in a web session (never an agent, a bot or a service account).
// The control comes from `viewer` only; the server decides again.
import { useState } from 'react';
import { useTranslation } from 'react-i18next';

import { useAiAssets, useSetAiAssets, type AiAssets } from '../../api/aiAssets';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { formatDateTime } from '../../features/labels';
import { Denied, Section } from './common';
import styles from './Settings.module.css';

export function AiAssetsSection({ projectId }: { projectId: number }) {
  const { t } = useTranslation();
  const read = useAiAssets(projectId);
  return (
    <Section id="ai-assets-title" title={t('settings.aiAssets.title')}>
      <p className={styles.hint}>{t('settings.aiAssets.intro')}</p>
      {read.isPending && <Loading />}
      {read.isError && (
        <QueryError
          error={read.error}
          onRetry={() => {
            void read.refetch();
          }}
        />
      )}
      {read.isSuccess && <AiAssetsBody projectId={projectId} a={read.data} />}
    </Section>
  );
}

function AiAssetsBody({ projectId, a }: { projectId: number; a: AiAssets }) {
  const { t, i18n } = useTranslation();
  const save = useSetAiAssets(projectId);
  const [message, setMessage] = useState('');
  const busy = save.isPending;
  return (
    <>
      <p data-testid="ai-assets-state">{a.enabled ? t('settings.aiAssets.on') : t('settings.aiAssets.off')}</p>
      {a.changed_at !== null && (
        <p className={styles.muted}>{t('settings.aiAssets.changedAt', { at: formatDateTime(a.changed_at, i18n.language) })}</p>
      )}
      {a.viewer.can_change ? (
        <div className={styles.row}>
          <button
            type="button"
            className={controls.button}
            aria-disabled={busy ? 'true' : undefined}
            onClick={() => {
              if (busy) return;
              setMessage('');
              const next = !a.enabled;
              save.mutate(next, {
                onSuccess: () => {
                  setMessage(next ? t('settings.aiAssets.turnedOn') : t('settings.aiAssets.turnedOff'));
                },
              });
            }}
          >
            {a.enabled ? t('settings.aiAssets.turnOff') : t('settings.aiAssets.turnOn')}
          </button>
        </div>
      ) : (
        <p className={styles.hint}>{t('settings.aiAssets.maintainerOnly')}</p>
      )}
      <p className={styles.hint}>{t('settings.aiAssets.offKeeps')}</p>
      <p role="status" className={styles.muted}>
        {message}
      </p>
      {save.isError && <Denied error={save.error} />}
    </>
  );
}
