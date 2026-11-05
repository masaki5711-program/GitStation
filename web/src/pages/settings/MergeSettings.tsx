// The merge part of a project's settings (U2): merging (methods, approvals on
// push, committers' approvals, CODEOWNERS suspension), approval rules, and the
// merge conditions of protected branches. Edit controls are shown from the
// viewer flags of `GET …/merge-settings` only (can_manage, can_loosen,
// can_suspend_codeowners); without them everything is read-only, so a
// Developer still sees why a merge is stopped. The server decides again.
//
// `updated_at` (of the settings, of an approval rule, of a merge rule) is sent
// back as `expected_updated_at` exactly as it was read (an opaque string; null
// for a rule that does not exist yet: create only); a 409 means someone saved
// in between: the screen offers to read them again instead of overwriting
// theirs. Approval rules name their approvers themselves, so every rule can be
// changed here whatever the number of members.
import { useEffect, useRef, useState, type KeyboardEvent, type ReactNode, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { useQuery } from '@tanstack/react-query';

import { call } from '../../api/client';
import { useApi } from '../../api/context';
import {
  MERGE_METHODS,
  isMergeMethod,
  useApprovalRules,
  useCodeownersEnforcement,
  useDeleteApprovalRule,
  useDeleteMergeRule,
  useGroupChoices,
  useMergeSettings,
  useProtectedRefs,
  usePutApprovalRule,
  usePutMergeRule,
  usePutMergeSettings,
  type MergeMethod,
} from '../../api/mergeSettings';
import type { ApprovalRule, MergeSettings, ProtectedRef } from '../../api/types';
import { SelectField, TextField } from '../../components/Field';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { isStale } from '../../features/mr/ActionError';
import { formatDateTime, mergeMethodLabel } from '../../features/labels';
import { Modal } from '../../ui/Modal';
import { focusWhenNoDialog } from '../../ui/focus';
import { ConfirmDialog, Denied, Section, useConfirm, useFocusHeading } from './common';
import { patternProblem } from './patterns';
import styles from './Settings.module.css';

/** What the viewer may change, from `GET …/merge-settings` (all false while
 * it is not read, or when it could not be read). */
export function useMergeViewer(projectId: number) {
  const settings = useMergeSettings(projectId);
  return settings.data?.viewer ?? { can_manage: false, can_loosen: false, can_suspend_codeowners: false };
}

function listOf(items: string[], language: string): string {
  return new Intl.ListFormat(language, { style: 'short', type: 'unit' }).format(items);
}

// ---------------------------------------------------------------- merging

export function MergingSection({ projectId }: { projectId: number }) {
  const { t } = useTranslation();
  const settings = useMergeSettings(projectId);
  /** A new form after "read again" (the drafts start from what was read). */
  const [generation, setGeneration] = useState(0);
  const [message, setMessage] = useState('');
  const focusHeading = useFocusHeading();
  return (
    <Section id="merging-title" title={t('settings.merging.title')}>
      {settings.isPending && <Loading />}
      {settings.isError && (
        <QueryError
          error={settings.error}
          onRetry={() => {
            void settings.refetch();
          }}
        />
      )}
      {settings.isSuccess && (
        <>
          {settings.data.viewer.can_manage ? (
            <MergingForm
              key={generation}
              projectId={projectId}
              settings={settings.data}
              onSaved={(saved) => {
                setMessage(saved ? t('settings.merging.saved') : '');
              }}
              onChangedElsewhere={() => {
                setMessage(t('settings.merging.changedElsewhere'));
              }}
              onReload={async () => {
                await settings.refetch();
                setGeneration((g) => g + 1);
                // The reload button goes away with the old form.
                focusHeading('merging-title');
              }}
            />
          ) : (
            <MergingView settings={settings.data} />
          )}
          <p role="status" className={styles.muted}>
            {message}
          </p>
          <Codeowners projectId={projectId} settings={settings.data} />
        </>
      )}
    </Section>
  );
}

function MergingView({ settings }: { settings: MergeSettings }) {
  const { t, i18n } = useTranslation();
  const yes = (v: boolean) => (v ? t('settings.merging.yes') : t('settings.merging.no'));
  return (
    <>
      <p className={styles.hint}>{t('settings.merging.readOnly')}</p>
      <dl className={styles.facts}>
        <dt>{t('settings.merging.methods')}</dt>
        <dd>{listOf(settings.allowed_methods.map((m) => mergeMethodLabel(t, m)), i18n.language)}</dd>
        <dt>{t('settings.merging.default')}</dt>
        <dd>{mergeMethodLabel(t, settings.default_method)}</dd>
        <dt>{t('settings.merging.deleteSource')}</dt>
        <dd>{yes(settings.delete_source_default)}</dd>
        <dt>{t('settings.merging.reset')}</dt>
        <dd>{yes(settings.reset_approvals_on_push)}</dd>
        <dt>{t('settings.merging.committers')}</dt>
        <dd>{yes(settings.committer_approvals_excluded)}</dd>
      </dl>
    </>
  );
}

function Check({
  label,
  checked,
  onChange,
  disabled = false,
  help,
}: {
  label: string;
  checked: boolean;
  onChange: (v: boolean) => void;
  disabled?: boolean;
  help?: string | null;
}) {
  return (
    <div className={controls.field}>
      <label className={controls.choiceLabel}>
        <input
          type="checkbox"
          checked={checked}
          disabled={disabled}
          onChange={(e) => {
            onChange(e.target.checked);
          }}
        />
        <span>{label}</span>
      </label>
      {help !== undefined && help !== null && <p className={controls.choiceHelp}>{help}</p>}
    </div>
  );
}

function sameMethods(a: readonly string[], b: readonly string[]): boolean {
  return a.length === b.length && a.every((m) => b.includes(m));
}

/** The fields this form edits are the same in both. */
function sameFields(a: MergeSettings, b: MergeSettings): boolean {
  return (
    sameMethods(a.allowed_methods, b.allowed_methods) &&
    a.default_method === b.default_method &&
    a.delete_source_default === b.delete_source_default &&
    a.reset_approvals_on_push === b.reset_approvals_on_push &&
    a.committer_approvals_excluded === b.committer_approvals_excluded
  );
}

function MergingForm({
  projectId,
  settings,
  onSaved,
  onChangedElsewhere,
  onReload,
}: {
  projectId: number;
  settings: MergeSettings;
  onSaved: (saved: boolean) => void;
  /** The untouched form took values someone else saved. */
  onChangedElsewhere: () => void;
  onReload: () => Promise<void>;
}) {
  const { t } = useTranslation();
  const save = usePutMergeSettings(projectId);
  /** The settings the drafts started from: its updated_at is what a save
   * sends back, so a change made elsewhere meanwhile is a 409, never
   * overwritten. */
  const [basis, setBasis] = useState(settings);
  const [allowed, setAllowed] = useState<MergeMethod[]>(settings.allowed_methods.filter(isMergeMethod));
  const [preferred, setPreferred] = useState(settings.default_method);
  const [deleteSource, setDeleteSource] = useState(settings.delete_source_default);
  const [reset, setReset] = useState(settings.reset_approvals_on_push);
  const [excluded, setExcluded] = useState(settings.committer_approvals_excluded);
  const [attempt, setAttempt] = useState(0);
  const dirty =
    !sameMethods(allowed, basis.allowed_methods) ||
    preferred !== basis.default_method ||
    deleteSource !== basis.delete_source_default ||
    reset !== basis.reset_approvals_on_push ||
    excluded !== basis.committer_approvals_excluded;

  // New settings arrived (this form's own save, a CODEOWNERS change, a
  // refetch). Without a change to these fields the new stamp is adopted; with
  // one, untouched drafts start again from it, and edited drafts keep the old
  // stamp, so their save is refused (409) instead of overwriting. The form is
  // not remounted: the focus stays where it is.
  useEffect(() => {
    if (settings.updated_at === basis.updated_at) return;
    if (sameFields(settings, basis)) {
      setBasis(settings);
    } else if (!dirty) {
      setBasis(settings);
      setAllowed(settings.allowed_methods.filter(isMergeMethod));
      setPreferred(settings.default_method);
      setDeleteSource(settings.delete_source_default);
      setReset(settings.reset_approvals_on_push);
      setExcluded(settings.committer_approvals_excluded);
      onChangedElsewhere();
    }
  }, [settings, basis, dirty, onChangedElsewhere]);
  // The default is always one of the allowed methods.
  const defaultMethod = allowed.find((m) => m === preferred) ?? allowed[0] ?? null;
  const problem = allowed.length === 0 ? t('settings.merging.noMethod') : null;
  // Turning off a switch that is on loosens the approvals: that needs a role
  // at or above every protection (can_loosen). Turning one on tightens.
  const lockReset = settings.reset_approvals_on_push && !settings.viewer.can_loosen;
  const lockExcluded = settings.committer_approvals_excluded && !settings.viewer.can_loosen;

  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    if (save.isPending) return;
    setAttempt((n) => n + 1);
    onSaved(false);
    if (problem !== null || defaultMethod === null) return;
    save.mutate(
      {
        allowed_methods: MERGE_METHODS.filter((m) => allowed.includes(m)),
        default_method: defaultMethod,
        delete_source_default: deleteSource,
        reset_approvals_on_push: reset,
        committer_approvals_excluded: excluded,
        // Verbatim (null for the defaults that were never stored).
        expected_updated_at: basis.updated_at,
      },
      {
        onSuccess: (next) => {
          setBasis(next);
          onSaved(true);
        },
      },
    );
  };

  return (
    <form onSubmit={submit} noValidate className={styles.narrow}>
      <fieldset className={controls.field} aria-describedby={problem !== null && attempt > 0 ? 'merging-methods-error' : undefined}>
        <legend className={controls.label}>{t('settings.merging.methods')}</legend>
        {MERGE_METHODS.map((m) => (
          <Check
            key={m}
            label={mergeMethodLabel(t, m)}
            checked={allowed.includes(m)}
            onChange={(on) => {
              setAllowed(on ? [...allowed, m] : allowed.filter((x) => x !== m));
            }}
          />
        ))}
        {problem !== null && attempt > 0 && (
          <p id="merging-methods-error" className={controls.fieldError}>
            {problem}
          </p>
        )}
      </fieldset>
      {defaultMethod !== null && (
        <SelectField label={t('settings.merging.default')} value={defaultMethod} onChange={setPreferred}>
          {MERGE_METHODS.filter((m) => allowed.includes(m)).map((m) => (
            <option key={m} value={m}>
              {mergeMethodLabel(t, m)}
            </option>
          ))}
        </SelectField>
      )}
      <Check label={t('settings.merging.deleteSource')} checked={deleteSource} onChange={setDeleteSource} />
      <Check
        label={t('settings.merging.reset')}
        checked={reset}
        onChange={setReset}
        disabled={lockReset}
        help={lockReset ? t('settings.merging.needsLoosen') : t('settings.merging.resetHelp')}
      />
      <Check
        label={t('settings.merging.committers')}
        checked={excluded}
        onChange={setExcluded}
        disabled={lockExcluded}
        help={lockExcluded ? t('settings.merging.needsLoosen') : t('settings.merging.committersHelp')}
      />
      {save.isError && (
        <div>
          <Denied error={save.error} />
          {isStale(save.error) && (
            <button
              type="button"
              className={controls.button}
              onClick={() => {
                void onReload();
              }}
            >
              {t('settings.merging.reload')}
            </button>
          )}
        </div>
      )}
      <button type="submit" className={controls.primary} aria-disabled={save.isPending ? 'true' : undefined}>
        {save.isPending ? t('settings.saving') : t('settings.merging.save')}
      </button>
    </form>
  );
}

const MINUTES = /^[0-9]{1,4}$/;

function Codeowners({ projectId, settings }: { projectId: number; settings: MergeSettings }) {
  const { t, i18n } = useTranslation();
  const change = useCodeownersEnforcement(projectId);
  const [minutes, setMinutes] = useState('60');
  const [attempt, setAttempt] = useState(0);
  const n = MINUTES.test(minutes.trim()) ? Number(minutes.trim()) : NaN;
  const problem = Number.isInteger(n) && n >= 1 && n <= 1440 ? null : t('settings.codeowners.minutesRange');
  const c = settings.codeowners;
  const suspended = !c.enforced && c.suspended_until !== null;
  const v = settings.viewer;
  return (
    <>
      <h3 className={styles.subTitle}>{t('settings.codeowners.title')}</h3>
      <p role="status" className={suspended ? styles.warning : undefined}>
        {suspended
          ? t('settings.codeowners.suspended', { until: formatDateTime(c.suspended_until ?? '', i18n.language) })
          : t('settings.codeowners.enforced')}
      </p>
      {!suspended && v.can_suspend_codeowners && (
        <form
          noValidate
          className={styles.narrow}
          onSubmit={(e) => {
            e.preventDefault();
            if (change.isPending) return;
            setAttempt((x) => x + 1);
            if (problem !== null) return;
            change.mutate({ enforced: false, for_minutes: n });
          }}
        >
          <TextField
            label={t('settings.codeowners.minutes')}
            value={minutes}
            onChange={setMinutes}
            help={t('settings.codeowners.minutesHelp')}
            error={attempt > 0 ? problem : null}
            required
          />
          <button type="submit" className={controls.button} aria-disabled={change.isPending ? 'true' : undefined}>
            {t('settings.codeowners.suspend')}
          </button>
        </form>
      )}
      {suspended && v.can_manage && (
        <button
          type="button"
          className={controls.button}
          aria-disabled={change.isPending ? 'true' : undefined}
          onClick={() => {
            if (!change.isPending) change.mutate({ enforced: true });
          }}
        >
          {t('settings.codeowners.enforce')}
        </button>
      )}
      {change.isError && <Denied error={change.error} />}
    </>
  );
}

// ---------------------------------------------------------------- approval rules

const RULE_NAME = /^[a-z0-9][a-z0-9_.-]{0,63}$/;

function parseCount(raw: string, min: number, max: number): number | null {
  const s = raw.trim();
  if (!/^[0-9]{1,3}$/.test(s)) return null;
  const n = Number(s);
  return n >= min && n <= max ? n : null;
}

type GroupName = { id: number; path: string; display_name: string };

interface GroupChoices {
  groups: GroupName[];
  /** The list was read (a rule dialog's form starts then). */
  ready: boolean;
  error: unknown;
  retry: () => void;
}

function useChoices(enabled: boolean): GroupChoices {
  const groups = useGroupChoices(enabled);
  return {
    groups: groups.data ?? [],
    ready: groups.isSuccess,
    error: groups.error,
    retry: () => {
      void groups.refetch();
    },
  };
}

/** Who may approve for a rule, by the names the rule answers with. */
function approverNames(r: ApprovalRule): { users: string[]; groups: string[] } {
  return { users: r.users.map((u) => u.username), groups: r.groups.map((g) => g.full_path) };
}

export function ApprovalRulesSection({ projectId, canManage }: { projectId: number; canManage: boolean }) {
  const { t } = useTranslation();
  const focusHeading = useFocusHeading();
  const rules = useApprovalRules(projectId);
  const list = rules.data?.rules ?? [];
  const [editing, setEditing] = useState<ApprovalRule | 'new' | null>(null);
  /** The button that opened the dialog gets the focus back when it closes. */
  const opener = useRef<HTMLElement | null>(null);
  const openDialog = (target: ApprovalRule | 'new') => {
    opener.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    setEditing(target);
  };
  const choices = useChoices(editing !== null);
  const confirm = useConfirm<string>();
  const remove = useDeleteApprovalRule(projectId);
  const [done, setDone] = useState('');

  const who = (r: ApprovalRule) => {
    const { users, groups } = approverNames(r);
    const people = [...users, ...groups];
    return people.length === 0 ? t('settings.rules.anyone') : people.join(', ');
  };

  return (
    <Section id="rules-title" title={t('settings.rules.title')}>
      <p className={styles.hint}>{t('settings.rules.explain')}</p>
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
        (list.length === 0 ? (
          <p>{t('settings.rules.none')}</p>
        ) : (
          <div className={styles.scroll} role="region" aria-label={t('settings.rules.tableLabel')} tabIndex={0}>
            <table className={styles.table}>
              <thead>
                <tr>
                  <th scope="col">{t('settings.rules.name')}</th>
                  <th scope="col">{t('settings.rules.target')}</th>
                  <th scope="col">{t('settings.rules.required')}</th>
                  <th scope="col">{t('settings.rules.who')}</th>
                  {canManage && <th scope="col">{t('common.actions')}</th>}
                </tr>
              </thead>
              <tbody>
                {list.map((r) => (
                  <tr key={r.approval_rule_id}>
                    <th scope="row">{r.name}</th>
                    <td>{r.target_pattern === null ? t('settings.rules.everyTarget') : <code>{r.target_pattern}</code>}</td>
                    <td>{r.required_approvals}</td>
                    <td>{who(r)}</td>
                    {canManage && (
                      <td>
                        <span className={styles.inline}>
                          <button
                            type="button"
                            className={controls.button}
                            aria-label={t('settings.rules.changeOf', { name: r.name })}
                            onClick={() => {
                              openDialog(r);
                            }}
                          >
                            {t('settings.rules.change')}
                          </button>
                          <button
                            type="button"
                            className={styles.dangerQuiet}
                            aria-label={t('settings.rules.deleteOf', { name: r.name })}
                            onClick={() => {
                              remove.reset();
                              confirm.ask(r.name);
                            }}
                          >
                            {t('settings.rules.delete')}
                          </button>
                        </span>
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
      {/* Only with the list read: the name check needs every existing name
          (the server refuses an existing name too: a new rule only creates). */}
      {canManage && rules.isSuccess && (
        <button
          type="button"
          className={controls.button}
          onClick={() => {
            openDialog('new');
          }}
        >
          {t('settings.rules.add')}
        </button>
      )}
      {editing !== null && (
        <RuleDialog
          projectId={projectId}
          // The rule as last read (a reload after a 409 gives the dialog the
          // new updated_at; the typed values stay).
          rule={editing === 'new' ? null : (list.find((r) => r.approval_rule_id === editing.approval_rule_id) ?? editing)}
          taken={list.map((r) => r.name)}
          choices={choices}
          onClose={(savedName) => {
            setEditing(null);
            if (savedName !== null) setDone(t('settings.rules.saved', { name: savedName }));
            focusWhenNoDialog(opener);
          }}
          onReload={async () => {
            await rules.refetch();
          }}
        />
      )}
      <ConfirmDialog
        open={confirm.target !== null}
        title={t('settings.rules.deleteTitle')}
        message={<p>{t('settings.rules.deleteBody', { name: confirm.target ?? '' })}</p>}
        confirmLabel={t('settings.rules.deleteConfirm')}
        busy={remove.isPending}
        error={remove.isError ? remove.error : null}
        onConfirm={() => {
          const name = confirm.target;
          if (name === null) return;
          remove.mutate(name, {
            onSuccess: () => {
              confirm.close();
              setDone(t('settings.rules.deleted', { name }));
              focusHeading('rules-title');
            },
          });
        }}
        onCancel={confirm.close}
      />
    </Section>
  );
}

function RuleDialog({
  projectId,
  rule,
  taken,
  choices,
  onClose,
  onReload,
}: {
  projectId: number;
  rule: ApprovalRule | null;
  /** The names of the rules that exist (the server refuses a new rule under one). */
  taken: string[];
  choices: GroupChoices;
  /** The saved rule's name, or null when nothing was saved. */
  onClose: (saved: string | null) => void;
  /** After a 409: read the rules again (the dialog stays open). */
  onReload: () => Promise<void>;
}) {
  const { t } = useTranslation();
  const save = usePutApprovalRule(projectId);
  const title = rule === null ? t('settings.rules.add') : t('settings.rules.changeTitle', { name: rule.name });
  // One dialog; its form is mounted once the group choices were read. The
  // approvers are seeded from the rule's own names, never from a list.
  return (
    <Modal
      open
      onClose={() => {
        onClose(null);
      }}
      title={title}
      busy={save.isPending}
    >
      {choices.ready ? (
        <RuleForm rule={rule} taken={taken} choices={choices} save={save} onClose={onClose} onReload={onReload} />
      ) : (
        <>
          {choices.error !== null ? <QueryError error={choices.error} onRetry={choices.retry} /> : <Loading />}
          <div className={styles.row}>
            <button
              type="button"
              className={controls.button}
              data-initial-focus=""
              onClick={() => {
                onClose(null);
              }}
            >
              {t('common.cancel')}
            </button>
          </div>
        </>
      )}
    </Modal>
  );
}

function RuleForm({
  rule,
  taken,
  choices,
  save,
  onClose,
  onReload,
}: {
  rule: ApprovalRule | null;
  taken: string[];
  choices: GroupChoices;
  save: ReturnType<typeof usePutApprovalRule>;
  onClose: (saved: string | null) => void;
  onReload: () => Promise<void>;
}) {
  const { t } = useTranslation();
  const form = useRef<HTMLFormElement>(null);
  // The form replaces the loading state inside the open dialog: its first
  // field takes the focus (the dialog only does that when it opens).
  useEffect(() => {
    form.current?.querySelector<HTMLElement>('[data-initial-focus], input, select, textarea')?.focus();
  }, []);
  const [name, setName] = useState(rule?.name ?? '');
  const [target, setTarget] = useState(rule?.target_pattern ?? '');
  const [count, setCount] = useState(String(rule?.required_approvals ?? 1));
  // Saving replaces the approvers: they start as the rule names them. An
  // approver the answer could not name (its ids are longer than its names)
  // would be dropped by a save, so none is offered then.
  const known = rule === null ? { users: [], groups: [] } : approverNames(rule);
  const unnamed =
    rule !== null && (rule.users.length !== rule.user_ids.length || rule.groups.length !== rule.group_ids.length);
  const [users, setUsers] = useState<string[]>(known.users);
  const [groups, setGroups] = useState<string[]>(known.groups);
  const [attempt, setAttempt] = useState(0);

  // Names are lower case only (the server's rule), so they compare as they are.
  const nameProblem =
    rule !== null
      ? null
      : !RULE_NAME.test(name)
        ? t('settings.rules.nameInvalid')
        : taken.includes(name)
          ? t('settings.rules.nameTaken')
          : null;
  const required = parseCount(count, 1, 20);
  const countProblem = required === null ? t('settings.rules.countInvalid') : null;
  const targetProblem =
    target.trim() === '' || patternProblem(target.trim()) === null ? null : t('settings.rules.targetInvalid');

  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    if (save.isPending || unnamed) return;
    setAttempt((n) => n + 1);
    if (nameProblem !== null || countProblem !== null || targetProblem !== null || required === null) return;
    const ruleName = rule?.name ?? name;
    save.mutate(
      {
        name: ruleName,
        body: {
          required_approvals: required,
          target_pattern: target.trim() === '' ? null : target.trim(),
          users,
          groups,
          // Verbatim; null: a new rule (refused when the name exists by then).
          expected_updated_at: rule?.updated_at ?? null,
        },
      },
      {
        onSuccess: () => {
          onClose(ruleName);
        },
      },
    );
  };

  return (
      <form ref={form} onSubmit={submit} noValidate>
        {rule === null && (
          <TextField
            label={t('settings.rules.name')}
            value={name}
            onChange={setName}
            error={attempt > 0 ? nameProblem : null}
            help={t('settings.rules.nameHelp')}
            required
            initialFocus
          />
        )}
        <TextField
          label={t('settings.rules.targetLabel')}
          value={target}
          onChange={setTarget}
          error={attempt > 0 ? targetProblem : null}
          help={t('settings.rules.targetHelp')}
        />
        <TextField
          label={t('settings.rules.required')}
          value={count}
          onChange={setCount}
          error={attempt > 0 ? countProblem : null}
          required
        />
        <UserPicker
          chosen={users}
          onAdd={(u) => {
            if (!users.includes(u)) setUsers([...users, u]);
          }}
          onRemove={(u) => {
            setUsers(users.filter((x) => x !== u));
          }}
        />
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
        <p className={styles.hint}>{t('settings.rules.anyoneHelp')}</p>
        {unnamed && (
          <p role="note" className={styles.warning}>
            {t('settings.rules.namesMissing')}
          </p>
        )}
        {save.isError && (
          <SaveError
            error={save.error}
            onReload={async () => {
              await onReload();
              save.reset();
              form.current?.querySelector<HTMLElement>('input, select, textarea')?.focus();
            }}
          />
        )}
        <div className={styles.row}>
          <button
            type="button"
            className={controls.button}
            disabled={save.isPending}
            onClick={() => {
              onClose(null);
            }}
          >
            {t('common.cancel')}
          </button>
          {!unnamed && (
            <button type="submit" className={controls.primary} aria-disabled={save.isPending ? 'true' : undefined}>
              {t('settings.rules.save')}
            </button>
          )}
        </div>
      </form>
  );
}

function Chosen({ items, onRemove }: { items: string[]; onRemove: (item: string) => void }) {
  const { t } = useTranslation();
  if (items.length === 0) return null;
  return (
    <ul className={styles.results}>
      {items.map((x) => (
        <li key={x}>
          <code>{x}</code>
          <button
            type="button"
            className={controls.button}
            aria-label={t('settings.rules.removeOne', { name: x })}
            onClick={() => {
              onRemove(x);
            }}
          >
            {t('settings.rules.removeButton')}
          </button>
        </li>
      ))}
    </ul>
  );
}

function UserPicker({
  chosen,
  onAdd,
  onRemove,
}: {
  chosen: string[];
  onAdd: (username: string) => void;
  onRemove: (username: string) => void;
}) {
  const { t } = useTranslation();
  const api = useApi();
  const box = useRef<HTMLFieldSetElement>(null);
  const [text, setText] = useState('');
  const [term, setTerm] = useState('');
  const [tried, setTried] = useState(false);
  const searchField = () => box.current?.querySelector<HTMLInputElement>('input[type="text"]') ?? null;
  const search = () => {
    setTried(true);
    if (termError === null) setTerm(trimmed);
  };
  // Enter in the search field searches; it never submits the rule form.
  const onKeyDown = (e: KeyboardEvent<HTMLFieldSetElement>) => {
    if (e.key === 'Enter' && e.target instanceof HTMLInputElement) {
      e.preventDefault();
      search();
    }
  };
  const trimmed = text.trim();
  const termError = trimmed.length < 2 || trimmed.length > 64 ? t('members.searchLength') : null;
  const results = useQuery({
    queryKey: ['user-search', term],
    enabled: term !== '',
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/users', { signal, params: { query: { search: term } } })),
  });
  return (
    <fieldset ref={box} className={controls.field} onKeyDown={onKeyDown}>
      <legend className={controls.label}>{t('settings.rules.users')}</legend>
      <Chosen
        items={chosen}
        onRemove={(u) => {
          onRemove(u);
          searchField()?.focus();
        }}
      />
      <TextField
        label={t('members.search')}
        value={text}
        onChange={setText}
        error={tried ? termError : null}
        help={t('members.searchHelp')}
      />
      <button type="button" className={controls.button} onClick={search}>
        {t('members.searchButton')}
      </button>
      {results.isPending && term !== '' && <Loading />}
      {results.isError && <QueryError error={results.error} />}
      {/* Stays mounted: each new count is read out. */}
      <p role="status" className={styles.muted}>
        {!results.isSuccess
          ? ''
          : results.data.items.length === 0
            ? t('members.noMatch')
            : t('members.found', { count: results.data.items.length })}
      </p>
      {results.isSuccess && (
        <>
          {results.data.items.length > 0 && (
            <ul className={styles.results} aria-label={t('members.results')}>
              {results.data.items.map((u) => (
                <li key={u.id}>
                  <span>
                    {u.display_name} <code>{u.username}</code>
                  </span>
                  {chosen.includes(u.username) ? (
                    <span className={styles.muted}>{t('settings.rules.alreadyChosen')}</span>
                  ) : (
                    <button
                      type="button"
                      className={controls.button}
                      aria-label={t('members.addOne', { name: u.display_name })}
                      onClick={() => {
                        onAdd(u.username);
                        searchField()?.focus();
                      }}
                    >
                      {t('members.addButton')}
                    </button>
                  )}
                </li>
              ))}
            </ul>
          )}
        </>
      )}
    </fieldset>
  );
}

function GroupPicker({
  groups,
  chosen,
  onAdd,
  onRemove,
}: {
  groups: GroupName[];
  chosen: string[];
  onAdd: (path: string) => void;
  onRemove: (path: string) => void;
}) {
  const { t } = useTranslation();
  const [pick, setPick] = useState('');
  const box = useRef<HTMLFieldSetElement>(null);
  const select = () => box.current?.querySelector('select')?.focus();
  return (
    <fieldset ref={box} className={controls.field}>
      <legend className={controls.label}>{t('settings.rules.groups')}</legend>
      <Chosen
        items={chosen}
        onRemove={(g) => {
          onRemove(g);
          select();
        }}
      />
      <SelectField label={t('settings.rules.group')} value={pick} onChange={setPick}>
        <option value="">{t('settings.rules.chooseGroup')}</option>
        {groups.map((g) => (
          <option key={g.id} value={g.path}>
            {t('settings.rules.groupOption', { name: g.display_name, path: g.path })}
          </option>
        ))}
      </SelectField>
      <button
        type="button"
        className={controls.button}
        aria-disabled={pick === '' ? 'true' : undefined}
        onClick={() => {
          if (pick === '') return;
          onAdd(pick);
          setPick('');
          select();
        }}
      >
        {t('settings.rules.addGroup')}
      </button>
    </fieldset>
  );
}

/** A refused save; on a 409 (someone saved in between) the reload line. */
function SaveError({ error, onReload }: { error: unknown; onReload: () => Promise<void> }) {
  const { t } = useTranslation();
  return (
    <div>
      <Denied error={error} />
      {isStale(error) && (
        <button
          type="button"
          className={controls.button}
          onClick={() => {
            void onReload();
          }}
        >
          {t('settings.merging.reload')}
        </button>
      )}
    </div>
  );
}

// ---------------------------------------------------------------- merge conditions

const CHECK_NAME = /^(ci|kanflow|forge)\/[^\s/]+(\/[^\s/]+)*$/;

/** The check names of a textarea, one per line (blank lines ignored), or
 * null when one is not acceptable (the server has the last word). */
function parseChecks(raw: string): string[] | null {
  const names = raw
    .split('\n')
    .map((s) => s.trim())
    .filter((s) => s !== '');
  if (names.length > 20) return null;
  const ok = names.every((n) => CHECK_NAME.test(n) && new TextEncoder().encode(n).length <= 100);
  return ok ? names : null;
}

/** What a protected branch's merge conditions do that a person must know. */
function BranchWarnings({ rule }: { rule: ProtectedRef }) {
  const { t } = useTranslation();
  const unreportable = rule.warnings.includes('required_checks_unreportable');
  // M3 I1d: a link condition on a project without a Kanflow binding.
  const unbound = rule.warnings.includes('work_item_link_unbound');
  if (!unreportable && !unbound) return null;
  return (
    <div role="note" className={styles.warning}>
      {unreportable && <p>{t('settings.conditions.unreportable')}</p>}
      {unbound && <p data-testid="merge-warning-work-item-unbound">{t('settings.conditions.workItemUnbound')}</p>}
      {rule.merge_rule?.direct_push_forbidden === true && <p>{t('settings.conditions.frozen')}</p>}
    </div>
  );
}

export function MergeConditionsSection({ projectId, canManage }: { projectId: number; canManage: boolean }) {
  const { t } = useTranslation();
  const focusHeading = useFocusHeading();
  const refs = useProtectedRefs(projectId);
  const branches = (refs.data?.rules ?? []).filter((r) => r.pattern.startsWith('refs/heads/'));
  const [editing, setEditing] = useState<ProtectedRef | null>(null);
  const opener = useRef<HTMLElement | null>(null);
  const confirm = useConfirm<ProtectedRef>();
  const remove = useDeleteMergeRule(projectId);
  const [done, setDone] = useState('');
  return (
    <Section id="conditions-title" title={t('settings.conditions.title')}>
      <p className={styles.hint}>{t('settings.conditions.explain')}</p>
      {refs.isPending && <Loading />}
      {refs.isError && (
        <QueryError
          error={refs.error}
          onRetry={() => {
            void refs.refetch();
          }}
        />
      )}
      {refs.isSuccess && branches.length === 0 && <p>{t('settings.conditions.none')}</p>}
      {branches.length > 0 && (
        <ul className={styles.branchList}>
          {branches.map((r) => (
            <li key={r.id} className={styles.branch}>
              <h3 className={styles.subTitle}>
                <code>{r.pattern}</code>
              </h3>
              <Conditions rule={r} />
              <BranchWarnings rule={r} />
              {canManage && (
                <span className={styles.inline}>
                  <button
                    type="button"
                    className={controls.button}
                    aria-label={t('settings.conditions.changeOf', { pattern: r.pattern })}
                    onClick={() => {
                      opener.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
                      setEditing(r);
                    }}
                  >
                    {t('settings.conditions.change')}
                  </button>
                  {r.merge_rule !== null && (
                    <button
                      type="button"
                      className={styles.dangerQuiet}
                      aria-label={t('settings.conditions.removeOf', { pattern: r.pattern })}
                      onClick={() => {
                        remove.reset();
                        confirm.ask(r);
                      }}
                    >
                      {t('settings.conditions.remove')}
                    </button>
                  )}
                </span>
              )}
            </li>
          ))}
        </ul>
      )}
      <p role="status" className={styles.muted}>
        {done}
      </p>
      {editing !== null && (
        <ConditionsDialog
          projectId={projectId}
          // The branch as last read (a reload after a 409 gives the dialog the
          // new updated_at; the typed values stay).
          rule={branches.find((r) => r.id === editing.id) ?? editing}
          onClose={(saved) => {
            setEditing(null);
            if (saved) setDone(t('settings.conditions.saved', { pattern: editing.pattern }));
            focusWhenNoDialog(opener);
          }}
          onReload={async () => {
            await refs.refetch();
          }}
        />
      )}
      <ConfirmDialog
        open={confirm.target !== null}
        title={t('settings.conditions.removeTitle')}
        message={<p>{t('settings.conditions.removeBody', { pattern: confirm.target?.pattern ?? '' })}</p>}
        confirmLabel={t('settings.conditions.removeConfirm')}
        busy={remove.isPending}
        error={remove.isError ? remove.error : null}
        onConfirm={() => {
          const target = confirm.target;
          if (target === null) return;
          remove.mutate(target.id, {
            onSuccess: () => {
              confirm.close();
              setDone(t('settings.conditions.removed', { pattern: target.pattern }));
              focusHeading('conditions-title');
            },
          });
        }}
        onCancel={confirm.close}
      />
    </Section>
  );
}

function Conditions({ rule }: { rule: ProtectedRef }) {
  const { t, i18n } = useTranslation();
  const m = rule.merge_rule;
  if (m === null) return <p className={styles.muted}>{t('settings.conditions.noConditions')}</p>;
  const lines: ReactNode[] = [t('settings.conditions.approvals', { count: m.required_approvals })];
  if (m.required_checks.length > 0) {
    lines.push(t('settings.conditions.checks', { names: listOf(m.required_checks, i18n.language) }));
  }
  if (m.require_resolved_discussions) lines.push(t('settings.conditions.resolved'));
  if (m.direct_push_forbidden) lines.push(t('settings.conditions.noPush'));
  if (m.require_work_item_link) {
    lines.push(<span data-testid="merge-condition-work-item">{t('settings.conditions.workItem')}</span>);
  }
  return (
    <ul className={styles.plainList}>
      {lines.map((line, i) => (
        <li key={i}>{line}</li>
      ))}
    </ul>
  );
}

function ConditionsDialog({
  projectId,
  rule,
  onClose,
  onReload,
}: {
  projectId: number;
  rule: ProtectedRef;
  onClose: (saved: boolean) => void;
  /** After a 409: read the branches again (the dialog stays open). */
  onReload: () => Promise<void>;
}) {
  const { t } = useTranslation();
  const save = usePutMergeRule(projectId);
  const form = useRef<HTMLFormElement>(null);
  const m = rule.merge_rule;
  const [count, setCount] = useState(String(m?.required_approvals ?? 0));
  const [checks, setChecks] = useState((m?.required_checks ?? []).join('\n'));
  const [resolved, setResolved] = useState(m?.require_resolved_discussions ?? false);
  const [noPush, setNoPush] = useState(m?.direct_push_forbidden ?? false);
  const [link, setLink] = useState(m?.require_work_item_link ?? false);
  const [attempt, setAttempt] = useState(0);
  const required = parseCount(count, 0, 20);
  const names = parseChecks(checks);
  const countProblem = required === null ? t('settings.conditions.countInvalid') : null;
  const checksProblem = names === null ? t('settings.conditions.checksInvalid') : null;
  const blocking = (names ?? []).length > 0 || checks.trim() !== '';

  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    if (save.isPending) return;
    setAttempt((n) => n + 1);
    if (required === null || names === null) return;
    save.mutate(
      {
        ruleId: rule.id,
        // Every field (a PUT replaces the merge rule: the work item link of
        // M3 I1d is sent too, never dropped) and the merge rule as read,
        // verbatim (null: none yet, create only).
        body: {
          required_approvals: required,
          required_checks: names,
          require_resolved_discussions: resolved,
          direct_push_forbidden: noPush,
          require_work_item_link: link,
          expected_updated_at: m?.updated_at ?? null,
        },
      },
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
      title={t('settings.conditions.dialogTitle', { pattern: rule.pattern })}
      busy={save.isPending}
    >
      <form ref={form} onSubmit={submit} noValidate>
        <TextField
          label={t('settings.rules.required')}
          value={count}
          onChange={setCount}
          error={attempt > 0 ? countProblem : null}
          help={t('settings.conditions.countHelp')}
          initialFocus
        />
        <TextField
          label={t('settings.conditions.checksLabel')}
          value={checks}
          onChange={setChecks}
          error={attempt > 0 ? checksProblem : null}
          help={t('settings.conditions.checksHelp')}
          multiline
        />
        <Check label={t('settings.conditions.resolved')} checked={resolved} onChange={setResolved} />
        <Check label={t('settings.conditions.noPushLabel')} checked={noPush} onChange={setNoPush} />
        <Check label={t('settings.conditions.workItemLabel')} checked={link} onChange={setLink} />
        {/* Stays mounted: a warning that appears while typing is read out. */}
        <div role="status" className={blocking || link ? styles.warning : undefined}>
          {blocking && <p>{t('settings.conditions.unreportable')}</p>}
          {blocking && noPush && <p>{t('settings.conditions.frozen')}</p>}
          {/* Review I1d: what the link condition costs without a binding. */}
          {link && <p>{t('settings.conditions.workItemDialog')}</p>}
        </div>
        {save.isError && (
          <SaveError
            error={save.error}
            onReload={async () => {
              await onReload();
              save.reset();
              form.current?.querySelector<HTMLElement>('input, select, textarea')?.focus();
            }}
          />
        )}
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
          <button type="submit" className={controls.primary} aria-disabled={save.isPending ? 'true' : undefined}>
            {t('settings.conditions.save')}
          </button>
        </div>
      </form>
    </Modal>
  );
}
