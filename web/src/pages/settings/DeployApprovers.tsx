// Who may approve a deploy environment's deployments (ADR-0038 §3.4–§3.6):
// named people, groups and organisation selectors, how many must approve,
// and whether a delegate may approve. Without any, any Maintainer approves
// (as before). Changing them is an Owner's (or an administrator's): others
// see the list only; the server decides again (FG-AUTH-0003 otherwise).
import type { TFunction } from 'i18next';
import { useRef, useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';

import {
  asSelector,
  useDeployApprovers,
  usePutDeployApprovers,
  type ApproverSelector,
} from '../../api/approvers';
import type { DeployApprovers, PutApprovers } from '../../api/types';
import { TextField } from '../../components/Field';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { selectorText } from '../../features/approvers';
import { Modal } from '../../ui/Modal';
import { focusWhenNoDialog } from '../../ui/focus';
import { SelectorEditor } from './ApproverSelectors';
import { Denied, Section } from './common';
import { GroupPicker, UserPicker, useChoices } from './MergeSettings';
import styles from './Settings.module.css';

function selectorsOf(e: DeployApprovers): ApproverSelector[] {
  return (e.selectors as unknown[]).map(asSelector).filter((x): x is ApproverSelector => x !== null);
}

function tierLabel(t: TFunction, tier: string): string {
  if (tier === 'prod') return t('approvers.deploy.tierProd');
  if (tier === 'stg') return t('approvers.deploy.tierStg');
  return t('approvers.deploy.tierDev');
}

export function DeployApproversSection({ projectId, canManage }: { projectId: number; canManage: boolean }) {
  const { t } = useTranslation();
  const list = useDeployApprovers(projectId);
  const [editing, setEditing] = useState<DeployApprovers | null>(null);
  const [done, setDone] = useState('');
  const opener = useRef<HTMLElement | null>(null);
  const envs = list.data?.environments ?? [];

  const who = (e: DeployApprovers) => {
    if (e.unrestricted) return t('approvers.deploy.anyMaintainer');
    return [
      ...e.users.map((u) => u.display_name),
      ...e.groups.map((g) => g.full_path),
      ...selectorsOf(e).map((s) => selectorText(t, s)),
    ].join(', ');
  };

  return (
    <Section id="deploy-approvers-title" title={t('approvers.deploy.title')}>
      <p className={styles.hint}>{t('approvers.deploy.explain')}</p>
      {list.isPending && <Loading />}
      {list.isError && (
        <QueryError
          error={list.error}
          onRetry={() => {
            void list.refetch();
          }}
        />
      )}
      {list.isSuccess &&
        (envs.length === 0 ? (
          <p>{t('approvers.deploy.none')}</p>
        ) : (
          <div className={styles.scroll} role="region" aria-label={t('approvers.deploy.tableLabel')} tabIndex={0}>
            <table className={styles.table}>
              <thead>
                <tr>
                  <th scope="col">{t('approvers.deploy.tier')}</th>
                  <th scope="col">{t('approvers.deploy.who')}</th>
                  <th scope="col">{t('approvers.deploy.required')}</th>
                  <th scope="col">{t('approvers.deploy.delegation')}</th>
                  {canManage && <th scope="col">{t('common.actions')}</th>}
                </tr>
              </thead>
              <tbody>
                {envs.map((e) => (
                  <tr key={e.tier}>
                    <th scope="row">{tierLabel(t, e.tier)}</th>
                    <td>{who(e)}</td>
                    <td>{e.approvals_required}</td>
                    <td>{e.allow_delegation ? t('approvers.deploy.delegationOn') : t('approvers.deploy.delegationOff')}</td>
                    {canManage && (
                      <td>
                        <button
                          type="button"
                          className={controls.button}
                          aria-label={t('approvers.deploy.changeOf', { tier: tierLabel(t, e.tier) })}
                          onClick={() => {
                            opener.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
                            setEditing(e);
                          }}
                        >
                          {t('approvers.deploy.change')}
                        </button>
                      </td>
                    )}
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ))}
      <p role="status" className={styles.muted}>
        {done}
      </p>
      {editing !== null && (
        <ApproversDialog
          projectId={projectId}
          env={editing}
          onClose={(saved) => {
            setEditing(null);
            if (saved) setDone(t('approvers.deploy.saved', { tier: tierLabel(t, editing.tier) }));
            focusWhenNoDialog(opener);
          }}
        />
      )}
    </Section>
  );
}

function ApproversDialog({
  projectId,
  env,
  onClose,
}: {
  projectId: number;
  env: DeployApprovers;
  onClose: (saved: boolean) => void;
}) {
  const { t } = useTranslation();
  const save = usePutDeployApprovers(projectId);
  const choices = useChoices(true);
  const [users, setUsers] = useState<string[]>(env.users.map((u) => u.username));
  const [groups, setGroups] = useState<string[]>(env.groups.map((g) => g.full_path));
  const [selectors, setSelectors] = useState<ApproverSelector[]>(selectorsOf(env));
  const [count, setCount] = useState(String(env.approvals_required));
  const [delegation, setDelegation] = useState(env.allow_delegation);
  const [attempt, setAttempt] = useState(0);
  const unreadable = (env.selectors as unknown[]).length !== selectorsOf(env).length;
  const trimmed = count.trim();
  const required = /^[0-9]{1,2}$/.test(trimmed) && Number(trimmed) >= 1 && Number(trimmed) <= 20 ? Number(trimmed) : null;
  const onlyUsers = groups.length === 0 && selectors.length === 0 && users.length > 0;
  const countProblem =
    required === null
      ? t('approvers.deploy.countInvalid')
      : onlyUsers && required > users.length
        ? t('approvers.deploy.countMoreThanPeople')
        : null;

  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    if (save.isPending || unreadable) return;
    setAttempt((n) => n + 1);
    if (countProblem !== null || required === null) return;
    const body: PutApprovers = {
      users,
      groups,
      selectors: selectors as unknown as NonNullable<PutApprovers['selectors']>,
      approvals_required: required,
      allow_delegation: delegation,
    };
    save.mutate(
      { tier: env.tier, body },
      {
        onSuccess: () => {
          onClose(true);
        },
      },
    );
  };

  return (
    <Modal
      open
      onClose={() => {
        onClose(false);
      }}
      title={t('approvers.deploy.dialogTitle', { tier: tierLabel(t, env.tier) })}
      busy={save.isPending}
    >
      <form onSubmit={submit} noValidate>
        <UserPicker
          chosen={users}
          onAdd={(u) => {
            if (!users.includes(u)) setUsers([...users, u]);
          }}
          onRemove={(u) => {
            setUsers(users.filter((x) => x !== u));
          }}
        />
        {choices.ready ? (
          <GroupPicker
            groups={choices.groups}
            chosen={groups}
            onAdd={(g) => {
              if (!groups.includes(g)) setGroups([...groups, g]);
            }}
            onRemove={(g) => {
              setGroups(groups.filter((x) => x !== g));
            }}
          />
        ) : choices.error !== null ? (
          <QueryError error={choices.error} onRetry={choices.retry} />
        ) : (
          <Loading />
        )}
        <SelectorEditor selectors={selectors} onChange={setSelectors} />
        <p className={styles.hint}>{t('approvers.deploy.whoHelp')}</p>
        <TextField
          label={t('approvers.deploy.required')}
          value={count}
          onChange={setCount}
          error={attempt > 0 ? countProblem : null}
          required
        />
        <div className={controls.field}>
          <label className={controls.choiceLabel}>
            <input
              type="checkbox"
              checked={delegation}
              onChange={(e) => {
                setDelegation(e.target.checked);
              }}
            />
            <span>{t('approvers.deploy.allowDelegation')}</span>
          </label>
        </div>
        {delegation && env.tier === 'prod' && (
          <p role="note" className={styles.warning}>
            {t('approvers.deploy.prodDelegationWarning')}
          </p>
        )}
        {unreadable && (
          <p role="note" className={styles.warning}>
            {t('approvers.deploy.unreadable')}
          </p>
        )}
        {save.isError && <Denied error={save.error} />}
        <div className={styles.row}>
          <button
            type="button"
            className={controls.button}
            disabled={save.isPending}
            onClick={() => {
              onClose(false);
            }}
          >
            {t('common.cancel')}
          </button>
          {!unreadable && (
            <button type="submit" className={controls.primary} aria-disabled={save.isPending ? 'true' : undefined}>
              {t('approvers.deploy.save')}
            </button>
          )}
        </div>
      </form>
    </Modal>
  );
}
