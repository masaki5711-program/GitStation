// A project's pipeline schedules (`/-/pipelines/schedules`, M4 C12): each
// one's branch and rule, when it runs next, and one line on its last run —
// why nothing started and what to do about it (plan 08). A Maintainer adds,
// changes, pauses and deletes them; whoever saves one becomes its owner, and
// it runs with that person's rights at each run, never with secrets.
import { useQuery } from '@tanstack/react-query';
import type { TFunction } from 'i18next';
import { useEffect, useId, useRef, useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { call } from '../../../api/client';
import { useApi } from '../../../api/context';
import { branchOf } from '../../../api/pipelines';
import {
  mayManageSchedules,
  OFFSETS,
  useDeleteSchedule,
  useSaveSchedule,
  useSchedules,
  type PipelineSchedule,
} from '../../../api/pipelineSchedules';
import { SelectField, TextField } from '../../../components/Field';
import { Loading } from '../../../components/Loading';
import { QueryError } from '../../../components/QueryError';
import controls from '../../../components/controls.module.css';
import { formatDateTime } from '../../../features/labels';
import { pipelineUrl, pipelinesUrl } from '../../../features/repoUrls';
import { useDocumentTitle } from '../../../hooks/useDocumentTitle';
import { useSession } from '../../../session/session';
import { EmptyState } from '../../../ui/EmptyState';
import { PageHeader } from '../../../ui/PageHeader';
import { RepoError } from '../common';
import styles from './Pipelines.module.css';

/** The form's state: a new schedule, or the one being changed. */
type Editing = { id: number | null; draft: Draft } | null;

interface Draft {
  description: string;
  branch: string;
  cron: string;
  offset: string;
  active: boolean;
}

const NEW_DRAFT: Draft = {
  description: '',
  branch: 'main',
  cron: '0 2 * * *',
  offset: '540',
  active: true,
};

function draftOf(s: PipelineSchedule): Draft {
  return {
    description: s.description,
    branch: branchOf(s.ref),
    cron: s.cron,
    offset: String(s.utc_offset_minutes),
    active: s.active,
  };
}

/** `540` → `UTC+09:00`. */
export function offsetLabel(minutes: number): string {
  const known = OFFSETS.find((o) => o.minutes === minutes);
  if (known !== undefined) return known.label;
  const sign = minutes < 0 ? '-' : '+';
  const abs = Math.abs(minutes);
  const h = String(Math.floor(abs / 60)).padStart(2, '0');
  const m = String(abs % 60).padStart(2, '0');
  return `UTC${sign}${h}:${m}`;
}

/** One line on the last run: what happened and, when nothing started, the
 * next step (Literal keys only: the dictionary gate reads them). */
export function lastRunSentence(t: TFunction, s: PipelineSchedule, manage: boolean): string {
  switch (s.last_outcome) {
    case null:
      return t('pipelines.schedules.last.never');
    case 'created':
      return t('pipelines.schedules.last.created');
    case 'no_definition':
      return t('pipelines.schedules.last.noDefinition');
    case 'ref_missing':
      return t('pipelines.schedules.last.refMissing', { branch: branchOf(s.ref) });
    case 'refused':
      return manage
        ? t('pipelines.schedules.last.refusedManage')
        : t('pipelines.schedules.last.refusedAsk');
    case 'failed':
      return t('pipelines.schedules.last.failed', { id: s.id });
    default:
      return s.last_outcome;
  }
}

export function SchedulesScreen({ projectId, project }: { projectId: number; project: string }) {
  const { t } = useTranslation();
  const api = useApi();
  const session = useSession();
  const schedules = useSchedules(projectId);
  // The same query (and key) as the project's frame: no second request.
  const info = useQuery({
    queryKey: ['project', projectId],
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/projects/{id}', { signal, params: { path: { id: projectId } } })),
  });
  const manage = info.isSuccess && mayManageSchedules(info.data.role, session.user.is_admin);
  const [editing, setEditing] = useState<Editing>(null);
  const title = t('pipelines.schedules.title');
  useDocumentTitle(title);
  const add = manage && editing === null && (
    <button type="button" className={controls.primary} onClick={() => {
        setEditing({ id: null, draft: NEW_DRAFT });
      }}>
      {t('pipelines.schedules.add')}
    </button>
  );

  return (
    <>
      <p className={styles.hint}>
        <Link to={pipelinesUrl(project)}>{t('pipelines.backToList')}</Link>
      </p>
      <PageHeader title={title} subtitle={t('pipelines.schedules.subtitle')} actions={add} />
      {editing !== null && (
        <ScheduleForm
          projectId={projectId}
          editing={editing}
          onDone={() => {
            setEditing(null);
          }}
        />
      )}
      {schedules.isPending && <Loading />}
      {schedules.isError && (
        <RepoError
          error={schedules.error}
          onRetry={() => {
            void schedules.refetch();
          }}
        />
      )}
      {schedules.isSuccess &&
        (schedules.data.schedules.length === 0 ? (
          <EmptyState
            icon="activity"
            reason={t('pipelines.schedules.empty')}
            hints={[manage ? t('pipelines.schedules.emptyManage') : t('pipelines.schedules.emptyAsk')]}
          />
        ) : (
          <ol className={styles.list} data-testid="schedule-list" aria-label={t('pipelines.schedules.listLabel')}>
            {schedules.data.schedules.map((s) => (
              <ScheduleItem
                key={s.id}
                s={s}
                project={project}
                projectId={projectId}
                manage={manage}
                me={session.user.id}
                onEdit={() => {
                  setEditing({ id: s.id, draft: draftOf(s) });
                }}
              />
            ))}
          </ol>
        ))}
    </>
  );
}

function ScheduleItem({
  s,
  project,
  projectId,
  manage,
  me,
  onEdit,
}: {
  s: PipelineSchedule;
  project: string;
  projectId: number;
  manage: boolean;
  me: number;
  onEdit: () => void;
}) {
  const { t, i18n } = useTranslation();
  const remove = useDeleteSchedule(projectId);
  const [confirming, setConfirming] = useState(false);
  const questionId = useId();
  const stopped = s.last_outcome !== null && s.last_outcome !== 'created';
  return (
    <li className={styles.item} data-testid="schedule-item">
      <span className={styles.itemMain}>
        <strong>{s.description}</strong>
        <span className={`${styles.badge ?? ''} ${(s.active ? styles.badgeRunning : styles.badgeNeutral) ?? ''}`}>
          {s.active ? t('pipelines.schedules.active') : t('pipelines.schedules.paused')}
        </span>
        <code className={styles.ref}>{branchOf(s.ref)}</code>
        <code>{s.cron}</code>
        <span className={styles.meta}>{offsetLabel(s.utc_offset_minutes)}</span>
      </span>
      <span className={styles.meta}>
        {s.next_run_at !== null && (
          <span>
            {t('pipelines.schedules.next', { time: formatDateTime(s.next_run_at, i18n.language) })}
          </span>
        )}
        <span>
          {s.owner_id === me
            ? t('pipelines.schedules.ownerYou')
            : t('pipelines.schedules.owner', { id: s.owner_id })}
        </span>
      </span>
      <p className={`${styles.scheduleLine ?? ''} ${stopped ? (styles.statusFailed ?? '') : ''}`}>
        {lastRunSentence(t, s, manage)}{' '}
        {s.last_outcome === 'created' && s.last_pipeline_id !== null && (
          <Link to={pipelineUrl(project, s.last_pipeline_id)}>
            {t('pipelines.number', { id: s.last_pipeline_id })}
          </Link>
        )}
      </p>
      {manage && (
        <p className={styles.actions}>
          <button type="button" className={controls.button} onClick={onEdit}>
            {t('pipelines.schedules.edit')}
          </button>
          <button
            type="button"
            className={controls.danger}
            disabled={remove.isPending || confirming}
            onClick={() => {
              setConfirming(true);
            }}
          >
            {t('pipelines.schedules.delete')}
          </button>
        </p>
      )}
      {confirming && (
        <div role="group" aria-labelledby={questionId} className={styles.confirm}>
          <p id={questionId}>{t('pipelines.schedules.deleteConfirm', { name: s.description })}</p>
          <p className={styles.actions}>
            <button
              type="button"
              className={controls.danger}
              onClick={() => {
                setConfirming(false);
                remove.mutate(s.id);
              }}
            >
              {t('pipelines.schedules.deleteYes')}
            </button>
            <button
              type="button"
              className={controls.button}
              onClick={() => {
                setConfirming(false);
              }}
            >
              {t('pipelines.cancelNo')}
            </button>
          </p>
        </div>
      )}
      {remove.isError && <QueryError error={remove.error} />}
    </li>
  );
}

function ScheduleForm({
  projectId,
  editing,
  onDone,
}: {
  projectId: number;
  editing: NonNullable<Editing>;
  onDone: () => void;
}) {
  const { t } = useTranslation();
  const save = useSaveSchedule(projectId);
  const [draft, setDraft] = useState<Draft>(editing.draft);
  const headingId = useId();
  const heading = useRef<HTMLHeadingElement>(null);
  useEffect(() => {
    heading.current?.focus();
  }, []);
  const set = (patch: Partial<Draft>) => {
    setDraft((d) => ({ ...d, ...patch }));
  };
  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    if (save.isPending) return;
    save.mutate(
      {
        id: editing.id,
        body: {
          description: draft.description,
          ref: draft.branch,
          cron: draft.cron,
          utc_offset_minutes: Number(draft.offset),
          active: draft.active,
        },
      },
      { onSuccess: onDone },
    );
  };
  return (
    <section aria-labelledby={headingId} className={styles.scheduleForm} data-testid="schedule-form">
      <h2 id={headingId} ref={heading} tabIndex={-1} className={styles.sectionTitle}>
        {editing.id === null ? t('pipelines.schedules.newTitle') : t('pipelines.schedules.editTitle')}
      </h2>
      <form onSubmit={submit} noValidate>
        <TextField
          label={t('pipelines.schedules.description')}
          value={draft.description}
          onChange={(v) => {
            set({ description: v });
          }}
          required
        />
        <TextField
          label={t('pipelines.schedules.branch')}
          value={draft.branch}
          onChange={(v) => {
            set({ branch: v });
          }}
          required
        />
        <TextField
          label={t('pipelines.schedules.cron')}
          help={t('pipelines.schedules.cronHelp')}
          value={draft.cron}
          onChange={(v) => {
            set({ cron: v });
          }}
          required
        />
        <SelectField
          label={t('pipelines.schedules.offset')}
          value={draft.offset}
          onChange={(v) => {
            set({ offset: v });
          }}
        >
          {OFFSETS.map((o) => (
            <option key={o.minutes} value={String(o.minutes)}>
              {o.label}
            </option>
          ))}
        </SelectField>
        <div className={controls.field}>
          <label className={styles.check}>
            <input
              type="checkbox"
              checked={draft.active}
              onChange={(e) => {
                set({ active: e.target.checked });
              }}
            />
            <span>{t('pipelines.schedules.activeLabel')}</span>
          </label>
          <p className={controls.help}>{t('pipelines.schedules.ownerHelp')}</p>
        </div>
        {save.isError && <QueryError error={save.error} />}
        <p className={styles.actions}>
          <button
            type="submit"
            className={controls.primary}
            aria-disabled={save.isPending ? 'true' : undefined}
          >
            {save.isPending ? t('pipelines.schedules.saving') : t('pipelines.schedules.save')}
          </button>
          <button type="button" className={controls.button} onClick={onDone}>
            {t('pipelines.cancelNo')}
          </button>
        </p>
      </form>
    </section>
  );
}
