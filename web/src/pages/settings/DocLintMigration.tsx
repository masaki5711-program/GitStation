// A project's document lint migration (plan 13 Q13, ADR-0026): broken
// document references are errors in the forge/doc-lint check; while the
// repository's documents are being moved into GitStation a Maintainer may
// lower them to warnings until a date. The date is required, from tomorrow
// to `max_until` (180 days); the day after it they are errors again by
// themselves. Every date is the server's (`today` and `max_until` come with
// the read, UTC): the browser's clock decides nothing, and dates are shown as
// written (`YYYY-MM-DD`), never through a Date that could shift the day.
// Edit controls come from `viewer.can_manage` only; the server decides again.
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import type { TFunction } from 'i18next';

import {
  useClearDocLintMigration,
  useDocLintMigration,
  useSetDocLintMigration,
} from '../../api/docLintMigration';
import type { DocLintMigration } from '../../api/types';
import { TextField } from '../../components/Field';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { formatDateTime } from '../../features/labels';
import { Denied, Section } from './common';
import styles from './Settings.module.css';

const DATE = /^[0-9]{4}-[0-9]{2}-[0-9]{2}$/;

/** The day after `day` (`YYYY-MM-DD`, calendar arithmetic in UTC). */
export function nextDay(day: string): string {
  const [y, m, d] = day.split('-').map(Number);
  if (y === undefined || m === undefined || d === undefined) return day;
  const next = new Date(Date.UTC(y, m - 1, d + 1));
  return next.toISOString().slice(0, 10);
}

/** When a migration date stops applying: the end of `day` in UTC (the next
 * day's 00:00 UTC), in the viewer's own time (`timeZone` for tests). The
 * moment comes from the server's date string, not the browser's clock. */
export function cutoff(day: string, language: string, timeZone?: string): string {
  const [y, m, d] = day.split('-').map(Number);
  if (y === undefined || m === undefined || d === undefined) return day;
  const end = new Date(Date.UTC(y, m - 1, d + 1));
  if (Number.isNaN(end.getTime())) return day;
  return new Intl.DateTimeFormat(language, {
    year: 'numeric',
    month: 'short',
    day: 'numeric',
    hour: 'numeric',
    minute: '2-digit',
    timeZoneName: 'short',
    ...(timeZone === undefined ? {} : { timeZone }),
  }).format(end);
}

/** Why `raw` cannot be sent (the server has the last word), or null. */
function dateProblem(raw: string, min: string, max: string, t: TFunction): string | null {
  const s = raw.trim();
  if (s === '') return t('settings.docLint.required');
  if (!DATE.test(s) || s < min || s > max) return t('settings.docLint.range', { min, max });
  return null;
}

export function DocLintMigrationSection({ projectId }: { projectId: number }) {
  const { t } = useTranslation();
  const migration = useDocLintMigration(projectId);
  return (
    <Section id="doc-lint-title" title={t('settings.docLint.title')}>
      <p className={styles.hint}>{t('settings.docLint.intro')}</p>
      {migration.isPending && <Loading />}
      {migration.isError && (
        <QueryError
          error={migration.error}
          onRetry={() => {
            void migration.refetch();
          }}
        />
      )}
      {migration.isSuccess && <MigrationBody projectId={projectId} m={migration.data} />}
    </Section>
  );
}

function MigrationBody({ projectId, m }: { projectId: number; m: DocLintMigration }) {
  const { t, i18n } = useTranslation();
  const set = useSetDocLintMigration(projectId);
  const clear = useClearDocLintMigration(projectId);
  const min = nextDay(m.today);
  const [date, setDate] = useState(m.in_force && m.warn_until !== null ? m.warn_until : '');
  const [attempt, setAttempt] = useState(0);
  const [message, setMessage] = useState('');
  const problem = dateProblem(date, min, m.max_until, t);
  const busy = set.isPending || clear.isPending;
  const until = m.warn_until;
  /** The day the help's cut-off is shown for: the date typed, else the first allowed. */
  const shownDay = DATE.test(date.trim()) ? date.trim() : min;
  return (
    <>
      <p className={until !== null && m.in_force ? styles.warning : undefined}>
        {until === null
          ? t('settings.docLint.none')
          : m.in_force
            ? t('settings.docLint.inForce', { until, local: cutoff(until, i18n.language) })
            : t('settings.docLint.ended', { until })}
      </p>
      {until !== null && m.set_at !== null && (
        <p className={styles.muted}>{t('settings.docLint.setAt', { at: formatDateTime(m.set_at, i18n.language) })}</p>
      )}
      {m.viewer.can_manage ? (
        <form
          noValidate
          className={styles.narrow}
          onSubmit={(e) => {
            e.preventDefault();
            if (busy) return;
            setAttempt((x) => x + 1);
            setMessage('');
            if (problem !== null) return;
            set.mutate(date.trim(), {
              onSuccess: (next) => {
                setMessage(t('settings.docLint.saved', { until: next.warn_until ?? '' }));
              },
            });
          }}
        >
          <TextField
            label={t('settings.docLint.date')}
            type="date"
            min={min}
            max={m.max_until}
            value={date}
            onChange={setDate}
            help={t('settings.docLint.help', {
              min,
              max: m.max_until,
              today: m.today,
              local: cutoff(shownDay, i18n.language),
              day: shownDay,
            })}
            error={attempt > 0 ? problem : null}
            required
          />
          <div className={styles.row}>
            <button type="submit" className={controls.button} aria-disabled={busy ? 'true' : undefined}>
              {t('settings.docLint.set')}
            </button>
            {until !== null && (
              <button
                type="button"
                className={controls.button}
                aria-disabled={busy ? 'true' : undefined}
                onClick={() => {
                  if (busy) return;
                  setMessage('');
                  clear.mutate(undefined, {
                    onSuccess: () => {
                      setDate('');
                      setAttempt(0);
                      setMessage(t('settings.docLint.cleared'));
                    },
                  });
                }}
              >
                {t('settings.docLint.clear')}
              </button>
            )}
          </div>
        </form>
      ) : (
        <p className={styles.hint}>{t('settings.docLint.readOnly')}</p>
      )}
      <p role="status" className={styles.muted}>
        {message}
      </p>
      {set.isError && <Denied error={set.error} />}
      {clear.isError && <Denied error={clear.error} />}
    </>
  );
}
