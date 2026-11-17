// A project's deterministic autofix (M4 C14, ADR-0017 Amendment C14): off
// by default; a Maintainer turns it on or off. Everyone who reads the
// project sees whether it is on. The control comes from `viewer` only; the
// server decides again.
import { useState } from 'react';
import { useTranslation } from 'react-i18next';

import { useAutofix, useSetAutofix, type Autofix } from '../../api/autofix';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { formatDateTime } from '../../features/labels';
import { Denied, Section } from './common';
import styles from './Settings.module.css';

export function AutofixSection({ projectId }: { projectId: number }) {
  const { t } = useTranslation();
  const read = useAutofix(projectId);
  return (
    <Section id="autofix-title" title={t('settings.autofix.title')}>
      <p className={styles.hint}>{t('settings.autofix.intro')}</p>
      {read.isPending && <Loading />}
      {read.isError && (
        <QueryError
          error={read.error}
          onRetry={() => {
            void read.refetch();
          }}
        />
      )}
      {read.isSuccess && <AutofixBody projectId={projectId} a={read.data} />}
    </Section>
  );
}

function AutofixBody({ projectId, a }: { projectId: number; a: Autofix }) {
  const { t, i18n } = useTranslation();
  const set = useSetAutofix(projectId);
  const [message, setMessage] = useState('');
  return (
    <>
      <p>
        {a.enabled ? t('settings.autofix.on') : t('settings.autofix.off')}
        {a.changed_at !== null && (
          <span className={styles.muted}> {t('settings.autofix.changedAt', { at: formatDateTime(a.changed_at, i18n.language) })}</span>
        )}
      </p>
      <p className={styles.hint}>{t('settings.autofix.scope')}</p>
      {a.viewer.can_change ? (
        <div className={styles.row}>
          <button
            type="button"
            className={controls.button}
            aria-disabled={set.isPending ? 'true' : undefined}
            onClick={() => {
              if (set.isPending) return;
              setMessage('');
              const enabling = !a.enabled;
              set.mutate(enabling, {
                onSuccess: () => {
                  setMessage(enabling ? t('settings.autofix.turnedOn') : t('settings.autofix.turnedOff'));
                },
              });
            }}
          >
            {a.enabled ? t('settings.autofix.turnOff') : t('settings.autofix.turnOn')}
          </button>
        </div>
      ) : (
        <p className={styles.hint}>{t('settings.autofix.maintainerOnly')}</p>
      )}
      <p role="status" className={styles.muted}>
        {message}
      </p>
      {set.isError && <Denied error={set.error} />}
    </>
  );
}
