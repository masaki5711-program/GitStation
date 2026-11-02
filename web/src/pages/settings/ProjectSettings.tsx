// `/{project}/-/settings`: name and description, visibility, protected refs,
// move / rename and delete. Every control is there for everyone who can see
// the page; what the person may not do is refused by the server with its
// reason (user_message + request_id), shown at the control.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from 'react-router';

import { call, callNoContent } from '../../api/client';
import { useApi } from '../../api/context';
import type { Project, Visibility } from '../../api/types';
import { SelectField, RadioField, TextField } from '../../components/Field';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { ErrorSummary } from '../../features/CreateDialogs';
import { formatDateTime, roleLabel } from '../../features/labels';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { useSession } from '../../session/session';
import { ConfirmDialog, Denied, Section, useConfirm, useFocusHeading, useRefreshSessionOn403 } from './common';
import { GeneralForm, MoveForm } from './Forms';
import styles from './Settings.module.css';

export function ProjectSettings({ projectId }: { projectId: number }) {
  const { t } = useTranslation();
  const api = useApi();
  const queryClient = useQueryClient();
  const project = useQuery({
    queryKey: ['project', projectId],
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/projects/{id}', { signal, params: { path: { id: projectId } } })),
  });
  useDocumentTitle(t('settings.title'));
  if (project.isPending) return <Loading />;
  if (project.isError) {
    return (
      <QueryError
        error={project.error}
        onRetry={() => {
          void project.refetch();
        }}
      />
    );
  }
  const p = project.data;
  const store = (next: Project) => {
    queryClient.setQueryData(['project', projectId], next);
    void queryClient.invalidateQueries({ queryKey: ['projects'] });
  };
  return (
    <>
      <h1 className={styles.title}>{t('settings.title')}</h1>
      <Section id="general-title" title={t('settings.general')}>
        <GeneralForm
          displayName={p.display_name}
          description={p.description}
          save={async (body) => {
            const next = await call(() =>
              api.PATCH('/api/v1/projects/{id}', { params: { path: { id: projectId } }, body }),
            );
            store(next);
            return next;
          }}
        />
      </Section>
      <Section id="visibility-title" title={t('form.visibility')}>
        <VisibilityForm project={p} onSaved={store} />
      </Section>
      <Section id="refs-title" title={t('settings.refs.title')}>
        <ProtectedRefs projectId={projectId} />
      </Section>
      <Section id="move-title" title={t('settings.move.title')}>
        <MoveForm kind="project" id={projectId} path={p.path} />
      </Section>
      <Section id="delete-title" title={t('settings.delete.title')} danger>
        <DeleteProject project={p} />
      </Section>
    </>
  );
}

function VisibilityForm({ project, onSaved }: { project: Project; onSaved: (p: Project) => void }) {
  const { t } = useTranslation();
  const api = useApi();
  const refresh = useRefreshSessionOn403();
  const current: Visibility = project.visibility === 'internal' ? 'internal' : 'private';
  const [value, setValue] = useState<Visibility>(current);
  const save = useMutation({
    mutationFn: (visibility: Visibility) =>
      call(() =>
        api.PUT('/api/v1/projects/{id}/visibility', {
          params: { path: { id: project.id } },
          body: { visibility },
        }),
      ),
    onSuccess: onSaved,
    onError: refresh,
  });
  return (
    <form
      className={styles.narrow}
      onSubmit={(e) => {
        e.preventDefault();
        if (!save.isPending && value !== current) save.mutate(value);
      }}
    >
      <RadioField<Visibility>
        legend={t('form.visibility')}
        name="visibility"
        value={value}
        onChange={setValue}
        choices={[
          { value: 'private', label: t('visibility.private'), help: t('visibility.privateHelp') },
          { value: 'internal', label: t('visibility.internal'), help: t('visibility.internalHelp') },
        ]}
      />
      {save.isError && <Denied error={save.error} />}
      <button
        type="submit"
        className={controls.primary}
        aria-disabled={save.isPending ? 'true' : undefined}
      >
        {save.isPending ? t('settings.saving') : t('settings.save')}
      </button>
      <span role="status" className={styles.muted}>
        {save.isSuccess ? t('settings.saved') : ''}
      </span>
    </form>
  );
}

const PUSH_ROLES = ['developer', 'maintainer', 'owner'] as const;

/** `refs/heads/…` or `refs/tags/…`, at most 255 bytes, at most one `*`, only
 * in the last part (ADR-0008). The server has the last word. */
export function patternProblem(pattern: string): 'empty' | 'prefix' | 'length' | 'star' | null {
  if (pattern.trim() === '') return 'empty';
  if (!pattern.startsWith('refs/heads/') && !pattern.startsWith('refs/tags/')) return 'prefix';
  if (new TextEncoder().encode(pattern).length > 255) return 'length';
  const stars = pattern.split('*').length - 1;
  if (stars > 1 || (stars === 1 && pattern.slice(pattern.lastIndexOf('/') + 1).indexOf('*') < 0)) {
    return 'star';
  }
  return null;
}

function ProtectedRefs({ projectId }: { projectId: number }) {
  const { t, i18n } = useTranslation();
  const focusHeading = useFocusHeading();
  const api = useApi();
  const queryClient = useQueryClient();
  const refresh = useRefreshSessionOn403();
  const key = ['protected-refs', projectId];
  const rules = useQuery({
    queryKey: key,
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/protected-refs', { signal, params: { path: { id: projectId } } }),
      ),
  });
  const [pattern, setPattern] = useState('');
  const [role, setRole] = useState<RoleKey>('maintainer');
  const [attempt, setAttempt] = useState(0);
  const create = useMutation({
    mutationFn: () =>
      call(() =>
        api.POST('/api/v1/projects/{id}/protected-refs', {
          params: { path: { id: projectId } },
          body: { pattern: pattern.trim(), push_min_role: role },
        }),
      ),
    onSuccess: async () => {
      setPattern('');
      await queryClient.invalidateQueries({ queryKey: key });
    },
    onError: refresh,
  });
  const [changed, setChanged] = useState<string | null>(null);
  const change = useMutation({
    mutationFn: (v: { id: number; role: RoleKey; pattern: string }) =>
      call(() =>
        api.PATCH('/api/v1/projects/{id}/protected-refs/{rule_id}', {
          params: { path: { id: projectId, rule_id: v.id } },
          body: { push_min_role: v.role },
        }),
      ),
    onSuccess: async (_r, v) => {
      setChanged(v.pattern);
      await queryClient.invalidateQueries({ queryKey: key });
    },
    onError: refresh,
  });
  const confirm = useConfirm<{ id: number; pattern: string }>();
  const remove = useMutation({
    mutationFn: (id: number) =>
      callNoContent(() =>
        api.DELETE('/api/v1/projects/{id}/protected-refs/{rule_id}', {
          params: { path: { id: projectId, rule_id: id } },
        }),
      ),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: key });
      confirm.close();
      focusHeading('refs-title');
    },
    onError: refresh,
  });
  const [drafts, setDrafts] = useState<Record<number, RoleKey>>({});

  const problem = patternProblem(pattern);
  const problemText =
    problem === null
      ? null
      : problem === 'empty'
        ? t('settings.refs.patternEmpty')
        : problem === 'prefix'
          ? t('settings.refs.patternPrefix')
          : problem === 'length'
            ? t('settings.refs.patternLength')
            : t('settings.refs.patternStar');
  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    if (create.isPending) return;
    setAttempt((n) => n + 1);
    if (problem !== null) return;
    create.mutate();
  };

  return (
    <>
      <p className={styles.hint}>{t('settings.refs.explain')}</p>
      {rules.isPending && <Loading />}
      {rules.isError && (
        <QueryError
          error={rules.error}
          onRetry={() => {
            void rules.refetch();
          }}
        />
      )}
      {rules.isSuccess &&
        (rules.data.rules.length === 0 ? (
          <p>{t('settings.refs.none')}</p>
        ) : (
          <div className={styles.scroll} role="region" aria-label={t('settings.refs.title')} tabIndex={0}>
            <table className={styles.table}>
              <thead>
                <tr>
                  <th scope="col">{t('settings.refs.pattern')}</th>
                  <th scope="col">{t('settings.refs.minRole')}</th>
                  <th scope="col">{t('settings.refs.updated')}</th>
                  <th scope="col">{t('common.actions')}</th>
                </tr>
              </thead>
              <tbody>
                {rules.data.rules.map((r) => {
                  const current = parseRole(r.push_min_role);
                  const draft = drafts[r.id] ?? current;
                  return (
                    <tr key={r.id}>
                      <th scope="row">
                        <code>{r.pattern}</code>
                      </th>
                      <td>
                        <select
                          className={controls.input}
                          aria-label={t('settings.refs.minRoleOf', { pattern: r.pattern })}
                          value={draft ?? ''}
                          onChange={(e) => {
                            const next = parseRole(e.target.value);
                            if (next !== undefined) setDrafts({ ...drafts, [r.id]: next });
                          }}
                        >
                          {current === undefined && (
                            <option value="" disabled>
                              {r.push_min_role}
                            </option>
                          )}
                          {PUSH_ROLES.map((x) => (
                            <option key={x} value={x}>
                              {roleLabel(t, x) ?? x}
                            </option>
                          ))}
                        </select>
                      </td>
                      <td>{formatDateTime(r.updated_at, i18n.language)}</td>
                      <td>
                        <span className={styles.inline}>
                          <button
                            type="button"
                            className={controls.button}
                            aria-label={t('settings.refs.changeOf', { pattern: r.pattern })}
                            aria-disabled={draft === undefined || draft === r.push_min_role || change.isPending ? 'true' : undefined}
                            onClick={() => {
                              if (draft === undefined || draft === r.push_min_role || change.isPending) return;
                              change.mutate({ id: r.id, role: draft, pattern: r.pattern });
                            }}
                          >
                            {t('settings.refs.change')}
                          </button>
                          <button
                            type="button"
                            className={styles.dangerQuiet}
                            aria-label={t('settings.refs.removeOf', { pattern: r.pattern })}
                            onClick={() => {
                              remove.reset();
                              confirm.ask({ id: r.id, pattern: r.pattern });
                            }}
                          >
                            {t('settings.refs.remove')}
                          </button>
                        </span>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        ))}
      {change.isError && <Denied error={change.error} />}
      <p role="status" className={styles.muted}>
        {changed === null ? '' : t('settings.refs.changed', { pattern: changed })}
      </p>
      <form onSubmit={submit} noValidate className={styles.narrow}>
        <h3>{t('settings.refs.add')}</h3>
        <ErrorSummary errors={problemText === null ? [] : [problemText]} attempt={attempt} />
        <TextField
          label={t('settings.refs.pattern')}
          value={pattern}
          onChange={setPattern}
          error={attempt > 0 ? problemText : null}
          help={t('settings.refs.patternHelp')}
          required
        />
        <SelectField
          label={t('settings.refs.minRole')}
          value={role}
          onChange={(v) => {
            const next = parseRole(v);
            if (next !== undefined) setRole(next);
          }}
        >
          {PUSH_ROLES.map((x) => (
            <option key={x} value={x}>
              {roleLabel(t, x) ?? x}
            </option>
          ))}
        </SelectField>
        {create.isError && <Denied error={create.error} />}
        <button
          type="submit"
          className={controls.primary}
          aria-disabled={create.isPending ? 'true' : undefined}
        >
          {t('settings.refs.add')}
        </button>
      </form>
      <ConfirmDialog
        open={confirm.target !== null}
        title={t('settings.refs.removeTitle')}
        message={<p>{t('settings.refs.removeBody', { pattern: confirm.target?.pattern ?? '' })}</p>}
        confirmLabel={t('settings.refs.remove')}
        busy={remove.isPending}
        error={remove.isError ? remove.error : null}
        onConfirm={() => {
          if (confirm.target !== null) remove.mutate(confirm.target.id);
        }}
        onCancel={confirm.close}
      />
    </>
  );
}

type RoleKey = (typeof PUSH_ROLES)[number];

/** The role a `<select>` value names, or `undefined` (the caller changes nothing). */
function parseRole(value: string): RoleKey | undefined {
  return PUSH_ROLES.find((r) => r === value);
}

function DeleteProject({ project }: { project: Project }) {
  const { t } = useTranslation();
  const api = useApi();
  const session = useSession();
  const navigate = useNavigate();
  const queryClient = useQueryClient();
  const refresh = useRefreshSessionOn403();
  const confirm = useConfirm<true>();
  const remove = useMutation({
    mutationFn: () =>
      call(() => api.DELETE('/api/v1/projects/{id}', { params: { path: { id: project.id } } })),
    onSuccess: async (deletion) => {
      // The purge date is only in this answer; keep it for the list of
      // projects waiting for deletion.
      queryClient.setQueryData(['deletion', project.id], deletion);
      await queryClient.invalidateQueries({ queryKey: ['projects'] });
      confirm.close();
      void navigate(`/${session.user.username}/-/settings?tab=trash`);
    },
    onError: refresh,
  });
  return (
    <div className={`${styles.zone} ${styles.narrow}`}>
      <p>{t('settings.delete.explain')}</p>
      <button
        type="button"
        className={styles.danger}
        onClick={() => {
          remove.reset();
          confirm.ask(true);
        }}
      >
        {t('settings.delete.button')}
      </button>
      <ConfirmDialog
        open={confirm.target !== null}
        title={t('settings.delete.confirmTitle')}
        message={<p>{t('settings.delete.confirmBody', { name: project.display_name })}</p>}
        confirmLabel={t('settings.delete.button')}
        busy={remove.isPending}
        error={remove.isError ? remove.error : null}
        onConfirm={() => {
          remove.mutate();
        }}
        onCancel={confirm.close}
      />
    </div>
  );
}
