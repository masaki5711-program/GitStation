// Members of a project or a group: the direct members, add (a user found by
// search), change a role, remove. Inherited roles are not listed (the API
// lists direct members only). Refusals ("would leave no Owner", "the member
// list changed") are shown with their reason + request id.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';

import { call, callNoContent, type ApiClient } from '../../api/client';
import { useApi } from '../../api/context';
import type { components } from '../../api/schema.gen';
import { SelectField, TextField } from '../../components/Field';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { ErrorSummary } from '../../features/CreateDialogs';
import { formatDate, roleLabel } from '../../features/labels';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { ConfirmDialog, Denied, Section, focusHeading, useConfirm, useRefreshSessionOn403 } from './common';
import styles from './Settings.module.css';

type Member = components['schemas']['MemberBody'];
type RoleName = components['schemas']['RoleName'];
export type Scope = { kind: 'project' | 'group'; id: number };

export const ROLES: readonly RoleName[] = ['guest', 'reporter', 'developer', 'maintainer', 'owner'];

function asRole(value: string): RoleName {
  return ROLES.find((r) => r === value) ?? 'reporter';
}

/** At most this many pages of 100 members are read. */
const MAX_PAGES = 10;

async function listMembers(api: ApiClient, scope: Scope, signal: AbortSignal) {
  const members: Member[] = [];
  let after: number | undefined;
  for (let n = 0; n < MAX_PAGES; n += 1) {
    const query = { limit: 100, ...(after === undefined ? {} : { after }) };
    const page =
      scope.kind === 'project'
        ? await call(() =>
            api.GET('/api/v1/projects/{id}/members', {
              signal,
              params: { path: { id: scope.id }, query },
            }),
          )
        : await call(() =>
            api.GET('/api/v1/groups/{id}/members', {
              signal,
              params: { path: { id: scope.id }, query },
            }),
          );
    members.push(...page.members);
    if (page.next_after === null) return { members, truncated: false };
    after = page.next_after;
  }
  return { members, truncated: true };
}

function addMember(api: ApiClient, scope: Scope, user_id: number, role: RoleName) {
  return scope.kind === 'project'
    ? call(() =>
        api.POST('/api/v1/projects/{id}/members', {
          params: { path: { id: scope.id } },
          body: { user_id, role },
        }),
      )
    : call(() =>
        api.POST('/api/v1/groups/{id}/members', {
          params: { path: { id: scope.id } },
          body: { user_id, role },
        }),
      );
}

function changeMember(api: ApiClient, scope: Scope, user_id: number, role: RoleName) {
  return scope.kind === 'project'
    ? call(() =>
        api.PATCH('/api/v1/projects/{id}/members/{user_id}', {
          params: { path: { id: scope.id, user_id } },
          body: { role },
        }),
      )
    : call(() =>
        api.PATCH('/api/v1/groups/{id}/members/{user_id}', {
          params: { path: { id: scope.id, user_id } },
          body: { role },
        }),
      );
}

function removeMember(api: ApiClient, scope: Scope, user_id: number): Promise<void> {
  return scope.kind === 'project'
    ? callNoContent(() =>
        api.DELETE('/api/v1/projects/{id}/members/{user_id}', {
          params: { path: { id: scope.id, user_id } },
        }),
      )
    : callNoContent(() =>
        api.DELETE('/api/v1/groups/{id}/members/{user_id}', {
          params: { path: { id: scope.id, user_id } },
        }),
      );
}

export function MembersScreen({ scope }: { scope: Scope }) {
  const { t } = useTranslation();
  const api = useApi();
  useDocumentTitle(t('members.title'));
  const key = ['members', scope.kind, scope.id];
  const list = useQuery({ queryKey: key, queryFn: ({ signal }) => listMembers(api, scope, signal) });

  return (
    <>
      <h1 className={styles.title}>{t('members.title')}</h1>
      <Section id="members-list-title" title={t('members.current')}>
        {list.isPending && <Loading />}
        {list.isError && (
          <QueryError
            error={list.error}
            onRetry={() => {
              void list.refetch();
            }}
          />
        )}
        {list.isSuccess && <MemberTable scope={scope} members={list.data.members} />}
        {list.isSuccess && list.data.truncated && (
          <p role="status" className={styles.hint}>
            {t('common.truncated')}
          </p>
        )}
      </Section>
      <Section id="members-add-title" title={t('members.add')}>
        <AddMember scope={scope} members={list.data?.members ?? []} />
      </Section>
    </>
  );
}

function MemberTable({ scope, members }: { scope: Scope; members: Member[] }) {
  const { t, i18n } = useTranslation();
  const api = useApi();
  const queryClient = useQueryClient();
  const refresh = useRefreshSessionOn403();
  const key = ['members', scope.kind, scope.id];
  const [drafts, setDrafts] = useState<Record<number, string>>({});
  const confirm = useConfirm<Member>();
  const change = useMutation({
    mutationFn: (v: { id: number; role: string }) => changeMember(api, scope, v.id, asRole(v.role)),
    onSuccess: async (_m, v) => {
      setDrafts((d) => Object.fromEntries(Object.entries(d).filter(([k]) => Number(k) !== v.id)));
      await queryClient.invalidateQueries({ queryKey: key });
    },
    onError: (e) => {
      refresh(e);
    },
  });
  const remove = useMutation({
    mutationFn: (id: number) => removeMember(api, scope, id),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: key });
      confirm.close();
      focusHeading('members-list-title');
    },
    onError: refresh,
  });
  if (members.length === 0) return <p>{t('members.none')}</p>;
  return (
    <>
      <div className={styles.scroll} role="region" aria-label={t('members.current')} tabIndex={0}>
        <table className={styles.table}>
          <thead>
            <tr>
              <th scope="col">{t('members.user')}</th>
              <th scope="col">{t('members.role')}</th>
              <th scope="col">{t('members.since')}</th>
              <th scope="col">{t('common.actions')}</th>
            </tr>
          </thead>
          <tbody>
            {members.map((m) => {
              const draft = drafts[m.user_id] ?? m.role;
              return (
                <tr key={m.user_id}>
                  <th scope="row">
                    {m.display_name} <code>{m.username}</code>
                    {!m.active && <span className={controls.badge}>{t('members.disabled')}</span>}
                  </th>
                  <td>
                    <select
                      className={controls.input}
                      aria-label={t('members.roleOf', { name: m.display_name })}
                      value={draft}
                      onChange={(e) => {
                        setDrafts({ ...drafts, [m.user_id]: e.target.value });
                      }}
                    >
                      {ROLES.map((r) => (
                        <option key={r} value={r}>
                          {roleLabel(t, r) ?? r}
                        </option>
                      ))}
                    </select>
                  </td>
                  <td>{formatDate(m.created_at, i18n.language)}</td>
                  <td>
                    <span className={styles.inline}>
                      <button
                        type="button"
                        className={controls.button}
                        disabled={draft === m.role}
                        onClick={() => {
                          change.mutate({ id: m.user_id, role: draft });
                        }}
                      >
                        {t('members.change')}
                      </button>
                      <button
                        type="button"
                        className={styles.danger}
                        onClick={() => {
                          remove.reset();
                          confirm.ask(m);
                        }}
                      >
                        {t('members.remove')}
                      </button>
                    </span>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      {change.isError && <Denied error={change.error} />}
      <ConfirmDialog
        open={confirm.target !== null}
        title={t('members.removeTitle')}
        message={<p>{t('members.removeBody', { name: confirm.target?.display_name ?? '' })}</p>}
        confirmLabel={t('members.remove')}
        busy={remove.isPending}
        error={remove.isError ? remove.error : null}
        onConfirm={() => {
          if (confirm.target !== null) remove.mutate(confirm.target.user_id);
        }}
        onCancel={confirm.close}
      />
    </>
  );
}

function AddMember({ scope, members }: { scope: Scope; members: Member[] }) {
  const { t } = useTranslation();
  const api = useApi();
  const queryClient = useQueryClient();
  const refresh = useRefreshSessionOn403();
  const [text, setText] = useState('');
  const [term, setTerm] = useState('');
  const [role, setRole] = useState<string>('reporter');
  const [attempt, setAttempt] = useState(0);
  const [added, setAdded] = useState<string | null>(null);

  const trimmed = text.trim();
  const termError = trimmed.length < 2 || trimmed.length > 64 ? t('members.searchLength') : null;
  const results = useQuery({
    queryKey: ['user-search', term],
    enabled: term !== '',
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/users', { signal, params: { query: { search: term } } })),
  });
  const add = useMutation({
    mutationFn: (v: { id: number; name: string }) => addMember(api, scope, v.id, asRole(role)),
    onSuccess: async (_m, v) => {
      setAdded(v.name);
      await queryClient.invalidateQueries({ queryKey: ['members', scope.kind, scope.id] });
    },
    onError: refresh,
  });
  const search = (e: SyntheticEvent) => {
    e.preventDefault();
    setAttempt((n) => n + 1);
    setAdded(null);
    if (termError !== null) return;
    setTerm(trimmed);
  };
  const present = new Set(members.map((m) => m.user_id));
  return (
    <>
      <form onSubmit={search} noValidate className={styles.narrow}>
        <ErrorSummary errors={termError === null ? [] : [termError]} attempt={attempt} />
        <TextField
          label={t('members.search')}
          value={text}
          onChange={setText}
          error={attempt > 0 ? termError : null}
          help={t('members.searchHelp')}
          required
        />
        <SelectField label={t('members.role')} value={role} onChange={setRole}>
          {ROLES.map((r) => (
            <option key={r} value={r}>
              {roleLabel(t, r) ?? r}
            </option>
          ))}
        </SelectField>
        <button type="submit" className={controls.button}>
          {t('members.searchButton')}
        </button>
      </form>
      <div aria-live="polite">
        {results.isPending && term !== '' && <Loading />}
        {results.isError && <QueryError error={results.error} />}
        {results.isSuccess &&
          (results.data.items.length === 0 ? (
            <p>{t('members.noMatch')}</p>
          ) : (
            <ul className={styles.results} aria-label={t('members.results')}>
              {results.data.items.map((u) => (
                <li key={u.id}>
                  <span>
                    {u.display_name} <code>{u.username}</code>
                  </span>
                  {present.has(u.id) ? (
                    <span className={styles.muted}>{t('members.already')}</span>
                  ) : (
                    <button
                      type="button"
                      className={controls.button}
                      aria-label={t('members.addOne', { name: u.display_name })}
                      aria-disabled={add.isPending ? 'true' : undefined}
                      onClick={() => {
                        if (!add.isPending) add.mutate({ id: u.id, name: u.display_name });
                      }}
                    >
                      {t('members.addButton')}
                    </button>
                  )}
                </li>
              ))}
            </ul>
          ))}
      </div>
      {add.isError && <Denied error={add.error} />}
      <p role="status" className={styles.muted}>
        {added === null ? '' : t('members.added', { name: added })}
      </p>
    </>
  );
}
