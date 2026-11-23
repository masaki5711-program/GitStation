// Org bindings (ADR-0038 §2): "the people of this unit (and the units below
// it) are Reporter / Developer here". On the members screen: the people a
// binding brings in now (source 「組織から（<unit>）」, not removable there —
// change the binding instead) and, for the Owner, the bindings themselves
// with a small form (a unit picked from the org master, descendants,
// primary / any, minimum rank, role). A binding never gives more than
// Developer; Maintainer and Owner are added by hand.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import type { TFunction } from 'i18next';
import { useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';

import { call, callNoContent, type ApiClient } from '../../api/client';
import { useApi } from '../../api/context';
import type { components } from '../../api/schema.gen';
import { SelectField, TextField } from '../../components/Field';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { roleLabel } from '../../features/labels';
import { ConfirmDialog, Denied, Section, useConfirm, useFocusHeading, useRefreshSessionOn403 } from './common';
import styles from './Settings.module.css';

type Bindings = components['schemas']['OrgBindingsBody'];
type Binding = components['schemas']['OrgBindingBody'];
type BindingUnit = components['schemas']['OrgBindingUnitBody'];
type PickUnit = components['schemas']['OrgPickUnitBody'];
type NewBinding = components['schemas']['NewOrgBinding'];
export type BindingScope = { kind: 'project' | 'group'; id: number };

const BOUND_ROLES = ['reporter', 'developer'] as const;
type BoundRole = (typeof BOUND_ROLES)[number];

export const orgBindingsKey = (scope: BindingScope) => ['org-bindings', scope.kind, scope.id];

function listBindings(api: ApiClient, scope: BindingScope, signal: AbortSignal): Promise<Bindings> {
  return scope.kind === 'project'
    ? call(() => api.GET('/api/v1/projects/{id}/org-bindings', { signal, params: { path: { id: scope.id } } }))
    : call(() => api.GET('/api/v1/groups/{id}/org-bindings', { signal, params: { path: { id: scope.id } } }));
}

function addBinding(api: ApiClient, scope: BindingScope, body: NewBinding) {
  return scope.kind === 'project'
    ? call(() => api.POST('/api/v1/projects/{id}/org-bindings', { params: { path: { id: scope.id } }, body }))
    : call(() => api.POST('/api/v1/groups/{id}/org-bindings', { params: { path: { id: scope.id } }, body }));
}

function removeBinding(api: ApiClient, scope: BindingScope, binding_id: number): Promise<void> {
  return scope.kind === 'project'
    ? callNoContent(() =>
        api.DELETE('/api/v1/projects/{id}/org-bindings/{binding_id}', {
          params: { path: { id: scope.id, binding_id } },
        }),
      )
    : callNoContent(() =>
        api.DELETE('/api/v1/groups/{id}/org-bindings/{binding_id}', {
          params: { path: { id: scope.id, binding_id } },
        }),
      );
}

/** A unit level's label (the code for one the master may add later). */
function levelLabel(t: TFunction, level: string): string {
  switch (level) {
    case 'division':
      return t('orgBindings.level.division');
    case 'department':
      return t('orgBindings.level.department');
    case 'section':
      return t('orgBindings.level.section');
    case 'team':
      return t('orgBindings.level.team');
    default:
      return level;
  }
}

/** A unit's name in the UI language (as `orgName`), or its code. */
function unitName(unit: BindingUnit | null, code: string, lang: string): string {
  if (unit === null) return code;
  if (lang.startsWith('en')) return unit.name_en ?? unit.name_ja;
  if (lang.startsWith('id')) return unit.name_id ?? unit.name_en ?? unit.name_ja;
  return unit.name_ja;
}

export function useOrgBindings(scope: BindingScope) {
  const api = useApi();
  return useQuery({
    queryKey: orgBindingsKey(scope),
    queryFn: ({ signal }) => listBindings(api, scope, signal),
  });
}

/** The people the bindings bring in: name, role, 「組織から（…）」. */
export function OrgBoundMembers({ scope }: { scope: BindingScope }) {
  const { t, i18n } = useTranslation();
  const list = useOrgBindings(scope);
  if (list.isPending) return <Loading />;
  if (list.isError) return <QueryError error={list.error} onRetry={() => void list.refetch()} />;
  const { members, members_truncated: truncated, members_limited: limited, bindings } = list.data;
  if (bindings.length === 0) return <p className={styles.muted}>{t('orgBindings.noBindings')}</p>;
  // ADR-0038 §6: rank and concurrent units are the person's and the administrators'.
  const limitedNote = limited ? <p className={styles.hint}>{t('orgBindings.membersLimited')}</p> : null;
  if (members.length === 0)
    return (
      <>
        <p>{t('orgBindings.noMembers')}</p>
        {limitedNote}
      </>
    );
  return (
    <>
      <p className={styles.hint}>{t('orgBindings.membersHint')}</p>
      {limitedNote}
      <div className={styles.scroll} role="region" aria-label={t('orgBindings.membersTitle')} tabIndex={0}>
        <table className={styles.table}>
          <thead>
            <tr>
              <th scope="col">{t('members.user')}</th>
              <th scope="col">{t('members.role')}</th>
              <th scope="col">{t('orgBindings.source')}</th>
            </tr>
          </thead>
          <tbody>
            {members.map((m) => (
              <tr key={m.user_id}>
                <th scope="row">
                  {m.display_name} <code>{m.username}</code>
                </th>
                <td>{roleLabel(t, m.role) ?? m.role}</td>
                <td>{t('orgBindings.fromOrg', { name: unitName(m.unit, m.org_code, i18n.language) })}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      {truncated && (
        <p role="status" className={styles.hint}>
          {t('common.truncated')}
        </p>
      )}
    </>
  );
}

/** The bindings (and, for the Owner, adding and removing one). */
export function OrgBindingsSection({ scope }: { scope: BindingScope }) {
  const { t } = useTranslation();
  const list = useOrgBindings(scope);
  return (
    <Section id="org-bindings-title" title={t('orgBindings.title')}>
      <p className={styles.hint}>{t('orgBindings.explain')}</p>
      {list.isPending && <Loading />}
      {list.isError && <QueryError error={list.error} onRetry={() => void list.refetch()} />}
      {list.isSuccess && (
        <>
          <BindingTable scope={scope} bindings={list.data.bindings} canManage={list.data.viewer.can_manage} />
          {list.data.viewer.can_manage ? (
            <AddBinding scope={scope} />
          ) : (
            <p className={styles.muted}>{t('orgBindings.ownerOnly')}</p>
          )}
        </>
      )}
    </Section>
  );
}

function scopeText(b: Binding, t: TFunction): string {
  const parts = [
    b.include_descendants ? t('orgBindings.withDescendants') : t('orgBindings.unitOnly'),
    b.membership === 'primary' ? t('orgBindings.primaryOnly') : t('orgBindings.anyMembership'),
  ];
  if (b.min_rank !== null) parts.push(t('orgBindings.minRankShort', { rank: b.min_rank }));
  return parts.join('・');
}

function BindingTable({ scope, bindings, canManage }: { scope: BindingScope; bindings: Binding[]; canManage: boolean }) {
  const { t, i18n } = useTranslation();
  const api = useApi();
  const queryClient = useQueryClient();
  const refresh = useRefreshSessionOn403();
  const focusHeading = useFocusHeading();
  const confirm = useConfirm<Binding>();
  const remove = useMutation({
    mutationFn: (id: number) => removeBinding(api, scope, id),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: orgBindingsKey(scope) });
      confirm.close();
      focusHeading('org-bindings-title');
    },
    onError: refresh,
  });
  if (bindings.length === 0) return <p>{t('orgBindings.noBindings')}</p>;
  return (
    <>
      <div className={styles.scroll} role="region" aria-label={t('orgBindings.title')} tabIndex={0}>
        <table className={styles.table}>
          <thead>
            <tr>
              <th scope="col">{t('orgBindings.unit')}</th>
              <th scope="col">{t('orgBindings.scope')}</th>
              <th scope="col">{t('members.role')}</th>
              <th scope="col">{t('orgBindings.reason')}</th>
              {canManage && <th scope="col">{t('common.actions')}</th>}
            </tr>
          </thead>
          <tbody>
            {bindings.map((b) => {
              const name = unitName(b.unit, b.org_code, i18n.language);
              return (
                <tr key={b.id}>
                  <th scope="row">
                    {name} <code>{b.org_code}</code>
                    {b.unit === null && <span className={controls.badge}>{t('orgBindings.notInMaster')}</span>}
                  </th>
                  <td>{scopeText(b, t)}</td>
                  <td>{roleLabel(t, b.role) ?? b.role}</td>
                  <td>{b.reason ?? '—'}</td>
                  {canManage && (
                    <td>
                      <button
                        type="button"
                        className={styles.dangerQuiet}
                        aria-label={t('orgBindings.removeOf', { name })}
                        onClick={() => {
                          remove.reset();
                          confirm.ask(b);
                        }}
                      >
                        {t('members.remove')}
                      </button>
                    </td>
                  )}
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      <ConfirmDialog
        open={confirm.target !== null}
        title={t('orgBindings.removeTitle')}
        message={
          <p>
            {t('orgBindings.removeBody', {
              name: confirm.target === null ? '' : unitName(confirm.target.unit, confirm.target.org_code, i18n.language),
            })}
          </p>
        }
        confirmLabel={t('members.remove')}
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

function parseRank(raw: string): number | null | 'invalid' {
  const s = raw.trim();
  if (s === '') return null;
  if (!/^\d{1,3}$/.test(s)) return 'invalid';
  return Number(s);
}

function AddBinding({ scope }: { scope: BindingScope }) {
  const { t, i18n } = useTranslation();
  const api = useApi();
  const queryClient = useQueryClient();
  const refresh = useRefreshSessionOn403();
  const [text, setText] = useState('');
  const [term, setTerm] = useState('');
  const [picked, setPicked] = useState<PickUnit | null>(null);
  const [descendants, setDescendants] = useState(true);
  const [membership, setMembership] = useState<'any' | 'primary'>('any');
  const [rank, setRank] = useState('');
  const [role, setRole] = useState<BoundRole>('reporter');
  const [reason, setReason] = useState('');
  const [attempt, setAttempt] = useState(0);
  const [added, setAdded] = useState<string | null>(null);

  const units = useQuery({
    queryKey: ['org-units', term],
    enabled: term !== '',
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/org-units', { signal, params: { query: { search: term, limit: 20 } } })),
  });
  const add = useMutation({
    mutationFn: (body: NewBinding) => addBinding(api, scope, body),
    onSuccess: async (_b, body) => {
      setAdded(picked === null ? body.org_code : unitName(picked.unit, picked.unit.code, i18n.language));
      setPicked(null);
      setReason('');
      await queryClient.invalidateQueries({ queryKey: orgBindingsKey(scope) });
    },
    onError: refresh,
  });

  const trimmed = text.trim();
  const termError = trimmed.length < 1 || trimmed.length > 64 ? t('orgBindings.searchLength') : null;
  const parsedRank = parseRank(rank);
  const rankError = parsedRank === 'invalid' ? t('orgBindings.rankInvalid') : null;
  const reasonError = reason.length > 500 ? t('orgBindings.reasonTooLong') : null;

  const search = (e: SyntheticEvent) => {
    e.preventDefault();
    setAdded(null);
    if (termError !== null) return;
    setPicked(null);
    setTerm(trimmed);
  };
  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    setAttempt((n) => n + 1);
    setAdded(null);
    if (picked === null || rankError !== null || reasonError !== null || parsedRank === 'invalid' || add.isPending) {
      return;
    }
    const body: NewBinding = {
      plant_code: picked.plant_code,
      org_code: picked.unit.code,
      include_descendants: descendants,
      membership,
      min_rank: parsedRank,
      role,
      reason: reason.trim() === '' ? null : reason.trim(),
    };
    add.mutate(body);
  };
  return (
    <div className={styles.narrow}>
      <h3 className={styles.subTitle}>{t('orgBindings.add')}</h3>
      <form onSubmit={search} noValidate>
        <TextField
          label={t('orgBindings.search')}
          value={text}
          onChange={setText}
          help={t('orgBindings.searchHelp')}
          error={attempt > 0 && term === '' ? termError : null}
        />
        <button type="submit" className={controls.button}>
          {t('members.searchButton')}
        </button>
      </form>
      {units.isPending && term !== '' && <Loading />}
      {units.isError && <QueryError error={units.error} />}
      {units.isSuccess && units.data.items.length === 0 && (
        <p role="status" className={styles.muted}>
          {t('orgBindings.noUnits')}
        </p>
      )}
      {units.isSuccess && units.data.items.length > 0 && (
        <fieldset className={controls.field}>
          <legend className={controls.label}>{t('orgBindings.pick')}</legend>
          {units.data.items.map((u) => (
            <label key={`${u.plant_code}/${u.unit.code}`} className={controls.choiceLabel}>
              <input
                type="radio"
                name="org-unit"
                checked={picked?.plant_code === u.plant_code && picked.unit.code === u.unit.code}
                onChange={() => {
                  setPicked(u);
                }}
              />
              <span>
                {unitName(u.unit, u.unit.code, i18n.language)} <code>{u.unit.code}</code>{' '}
                <span className={styles.muted}>
                  {t('orgBindings.unitMeta', {
                    level: levelLabel(t, u.unit.level),
                    plant: u.plant_code,
                  })}
                </span>
              </span>
            </label>
          ))}
        </fieldset>
      )}
      <form onSubmit={submit} noValidate>
        {attempt > 0 && picked === null && (
          <p role="alert" className={controls.fieldError}>
            {t('orgBindings.pickFirst')}
          </p>
        )}
        <div className={controls.field}>
          <label className={controls.choiceLabel}>
            <input
              type="checkbox"
              checked={descendants}
              onChange={(e) => {
                setDescendants(e.target.checked);
              }}
            />
            <span>{t('orgBindings.includeDescendants')}</span>
          </label>
          <p className={controls.choiceHelp}>{t('orgBindings.includeDescendantsHelp')}</p>
        </div>
        <SelectField
          label={t('orgBindings.membership')}
          value={membership}
          onChange={(v) => {
            setMembership(v === 'primary' ? 'primary' : 'any');
          }}
        >
          <option value="any">{t('orgBindings.anyMembership')}</option>
          <option value="primary">{t('orgBindings.primaryOnly')}</option>
        </SelectField>
        <TextField
          label={t('orgBindings.minRank')}
          value={rank}
          onChange={setRank}
          help={t('orgBindings.minRankHelp')}
          error={rankError}
        />
        <SelectField
          label={t('members.role')}
          value={role}
          onChange={(v) => {
            setRole(v === 'developer' ? 'developer' : 'reporter');
          }}
        >
          {BOUND_ROLES.map((r) => (
            <option key={r} value={r}>
              {roleLabel(t, r) ?? r}
            </option>
          ))}
        </SelectField>
        <p className={styles.hint}>{t('orgBindings.roleCap')}</p>
        <TextField
          label={t('orgBindings.reason')}
          value={reason}
          onChange={setReason}
          help={t('orgBindings.reasonHelp')}
          error={reasonError}
        />
        <button type="submit" className={controls.button} aria-disabled={add.isPending ? 'true' : undefined}>
          {t('orgBindings.addButton')}
        </button>
      </form>
      <p role="status" className={styles.muted}>
        {added === null ? '' : t('orgBindings.added', { name: added })}
      </p>
      {add.isError && <Denied error={add.error} />}
    </div>
  );
}
