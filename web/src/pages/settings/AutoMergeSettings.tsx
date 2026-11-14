// A project's auto-merge (M4 C9, plan 04 M-5, ADR-0017 Amendment 2026-11-13):
// its merge.auto (off / reserve / conditional / full) and the emergency stop.
// The Owner changes the mode (in a web session); a Maintainer turns the
// emergency stop on or off; everyone who reads the project sees both, and
// the instance's emergency stop when an administrator turned it on. Controls
// come from `viewer` only; the server decides again.
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { TFunction } from 'i18next';

import {
  AUTO_MERGE_MODES,
  isAutoMergeMode,
  useAutoMerge,
  useSetAutoMergeMode,
  useSetAutoMergeStop,
  type AutoMerge,
  type AutoMergeMode,
} from '../../api/autoMerge';
import { SelectField } from '../../components/Field';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { formatDateTime } from '../../features/labels';
import { Denied, Section } from './common';
import styles from './Settings.module.css';

export function modeLabel(t: TFunction, mode: string): string {
  switch (mode) {
    case 'off':
      return t('settings.autoMerge.mode.off');
    case 'reserve':
      return t('settings.autoMerge.mode.reserve');
    case 'conditional':
      return t('settings.autoMerge.mode.conditional');
    case 'full':
      return t('settings.autoMerge.mode.full');
    default:
      return mode;
  }
}

export function AutoMergeSection({ projectId }: { projectId: number }) {
  const { t } = useTranslation();
  const read = useAutoMerge(projectId);
  return (
    <Section id="auto-merge-title" title={t('settings.autoMerge.title')}>
      <p className={styles.hint}>{t('settings.autoMerge.intro')}</p>
      {read.isPending && <Loading />}
      {read.isError && (
        <QueryError
          error={read.error}
          onRetry={() => {
            void read.refetch();
          }}
        />
      )}
      {read.isSuccess && <AutoMergeBody projectId={projectId} a={read.data} />}
    </Section>
  );
}

function AutoMergeBody({ projectId, a }: { projectId: number; a: AutoMerge }) {
  const { t, i18n } = useTranslation();
  const setMode = useSetAutoMergeMode(projectId);
  const setStop = useSetAutoMergeStop(projectId);
  const [mode, setDraft] = useState<AutoMergeMode>(isAutoMergeMode(a.mode) ? a.mode : 'off');
  const [message, setMessage] = useState('');
  const busy = setMode.isPending || setStop.isPending;
  const when = (at: string) => formatDateTime(at, i18n.language);
  return (
    <>
      <p>{t('settings.autoMerge.current', { mode: modeLabel(t, a.mode) })}</p>
      {a.instance_stop !== null && (
        <p className={styles.warning} role="status">
          {t('settings.autoMerge.instanceStopped', { at: when(a.instance_stop.at) })}
        </p>
      )}
      {a.project_stop !== null && (
        <p className={styles.warning} role="status">
          {t('settings.autoMerge.projectStopped', { at: when(a.project_stop.at) })}
        </p>
      )}
      {a.viewer.can_change_mode ? (
        <form
          noValidate
          className={styles.narrow}
          onSubmit={(e) => {
            e.preventDefault();
            if (busy) return;
            setMessage('');
            setMode.mutate(mode, {
              onSuccess: (next) => {
                setMessage(t('settings.autoMerge.saved', { mode: modeLabel(t, next.mode) }));
              },
            });
          }}
        >
          <SelectField
            label={t('settings.autoMerge.modeLabel')}
            value={mode}
            onChange={(v) => {
              if (isAutoMergeMode(v)) setDraft(v);
            }}
          >
            {AUTO_MERGE_MODES.map((m) => (
              <option key={m} value={m}>
                {modeLabel(t, m)}
              </option>
            ))}
          </SelectField>
          <p className={styles.hint}>{t('settings.autoMerge.stopsOnly')}</p>
          <button type="submit" className={controls.button} aria-disabled={busy ? 'true' : undefined}>
            {t('settings.autoMerge.save')}
          </button>
        </form>
      ) : (
        <p className={styles.hint}>{t('settings.autoMerge.ownerOnly')}</p>
      )}
      {a.viewer.can_stop && (
        <div className={styles.row}>
          <button
            type="button"
            className={controls.button}
            aria-disabled={busy ? 'true' : undefined}
            onClick={() => {
              if (busy) return;
              setMessage('');
              const stopping = a.project_stop === null;
              setStop.mutate(stopping, {
                onSuccess: () => {
                  setMessage(stopping ? t('settings.autoMerge.stopped') : t('settings.autoMerge.resumed'));
                },
              });
            }}
          >
            {a.project_stop === null ? t('settings.autoMerge.stop') : t('settings.autoMerge.resume')}
          </button>
        </div>
      )}
      <p role="status" className={styles.muted}>
        {message}
      </p>
      {setMode.isError && <Denied error={setMode.error} />}
      {setStop.isError && <Denied error={setStop.error} />}
    </>
  );
}
