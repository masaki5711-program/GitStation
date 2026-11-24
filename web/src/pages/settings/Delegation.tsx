// 「代理の承認」 (ADR-0038 §3.5): while the person is away, one delegate may
// approve for them for at most 30 days (merge requests always, deployments
// where the environment allows it). One delegation per person; a new one
// replaces it. Set and removed in the browser only; the server checks again.
import { useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';

import {
  MAX_DELEGATION_DAYS,
  localToRfc3339,
  periodProblem,
  useDelegation,
  useDeleteDelegation,
  usePutDelegation,
} from '../../api/approvers';
import type { Delegation } from '../../api/types';
import { TextField } from '../../components/Field';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { formatDateTime } from '../../features/labels';
import { Denied, Section } from './common';
import { UserPicker } from './MergeSettings';
import styles from './Settings.module.css';

function Period({ d }: { d: Delegation }) {
  const { t, i18n } = useTranslation();
  const now = Date.now();
  const state = d.active
    ? t('approvers.delegation.active')
    : Date.parse(d.starts_at) > now
      ? t('approvers.delegation.upcoming')
      : t('approvers.delegation.ended');
  return (
    <>
      {t('approvers.delegation.period', {
        from: formatDateTime(d.starts_at, i18n.language),
        until: formatDateTime(d.ends_at, i18n.language),
      })}{' '}
      <span className={styles.chip}>{state}</span>
    </>
  );
}

export function DelegationSection() {
  const { t } = useTranslation();
  const state = useDelegation();
  const put = usePutDelegation();
  const remove = useDeleteDelegation();
  const [delegate, setDelegate] = useState<string[]>([]);
  const [start, setStart] = useState('');
  const [end, setEnd] = useState('');
  const [attempt, setAttempt] = useState(0);
  const [done, setDone] = useState('');

  const problem = periodProblem(start, end);
  const periodText =
    problem === 'invalid'
      ? t('approvers.delegation.periodInvalid')
      : problem === 'order'
        ? t('approvers.delegation.periodOrder')
        : problem === 'ended'
          ? t('approvers.delegation.periodEnded')
          : problem === 'tooLong'
            ? t('approvers.delegation.periodTooLong', { days: MAX_DELEGATION_DAYS })
            : null;
  const delegateText = delegate.length === 0 ? t('approvers.delegation.delegateMissing') : null;

  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    if (put.isPending) return;
    setAttempt((n) => n + 1);
    const s = localToRfc3339(start);
    const en = localToRfc3339(end);
    const who = delegate[0];
    if (periodText !== null || who === undefined || s === null || en === null) return;
    put.mutate(
      { delegate: who, starts_at: s, ends_at: en },
      {
        onSuccess: () => {
          setDone(t('approvers.delegation.saved', { name: who }));
        },
      },
    );
  };

  const mine = state.data?.mine ?? null;
  const toMe = state.data?.to_me ?? [];
  return (
    <Section id="delegation-title" title={t('approvers.delegation.title')}>
      <p className={styles.hint}>{t('approvers.delegation.explain', { days: MAX_DELEGATION_DAYS })}</p>
      {state.isPending && <Loading />}
      {state.isError && (
        <QueryError
          error={state.error}
          onRetry={() => {
            void state.refetch();
          }}
        />
      )}
      {state.isSuccess && (
        <>
          {mine === null ? (
            <p>{t('approvers.delegation.noneMine')}</p>
          ) : (
            <p data-testid="delegation-mine">
              {t('approvers.delegation.mine', { name: mine.delegate_display_name })} <Period d={mine} />{' '}
              <button
                type="button"
                className={styles.dangerQuiet}
                aria-disabled={remove.isPending ? 'true' : undefined}
                onClick={() => {
                  if (remove.isPending) return;
                  remove.mutate(undefined, {
                    onSuccess: () => {
                      setDone(t('approvers.delegation.removed'));
                    },
                  });
                }}
              >
                {t('approvers.delegation.remove')}
              </button>
            </p>
          )}
          {remove.isError && <Denied error={remove.error} />}
          {toMe.length > 0 && (
            <>
              <h3 className={styles.subTitle}>{t('approvers.delegation.toMe')}</h3>
              <ul className={styles.plainList}>
                {toMe.map((d) => (
                  <li key={d.delegator_id}>
                    {d.delegator_display_name} <code>{d.delegator_username}</code> <Period d={d} />
                  </li>
                ))}
              </ul>
            </>
          )}
          <form onSubmit={submit} noValidate>
            <UserPicker
              chosen={delegate}
              onAdd={(u) => {
                setDelegate([u]);
              }}
              onRemove={() => {
                setDelegate([]);
              }}
            />
            {attempt > 0 && delegateText !== null && <p className={controls.fieldError}>{delegateText}</p>}
            <TextField
              type="datetime-local"
              label={t('approvers.delegation.from')}
              value={start}
              onChange={setStart}
              required
            />
            <TextField
              type="datetime-local"
              label={t('approvers.delegation.until')}
              value={end}
              onChange={setEnd}
              error={attempt > 0 ? periodText : null}
              help={t('approvers.delegation.untilHelp', { days: MAX_DELEGATION_DAYS })}
              required
            />
            {put.isError && <Denied error={put.error} />}
            <button type="submit" className={controls.primary} aria-disabled={put.isPending ? 'true' : undefined}>
              {t('approvers.delegation.save')}
            </button>
          </form>
        </>
      )}
      <p role="status" className={styles.muted}>
        {done}
      </p>
    </Section>
  );
}
