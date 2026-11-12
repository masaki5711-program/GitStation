// Administration: service accounts (M3 S0, ADR-0019 Addendum S0). Administrators
// only. The list, creating an account, disabling it, and one account's tokens
// (issued from a browser session only; the secret is in the answer of the
// issue request, once, and lives in this component's state only: never in the
// query cache, storage or the URL). A disabled account cannot be enabled again
// by the API, so no enable button is offered; the page says so instead.
//
// Query keys: the list is ['service-accounts']; one account is
// ['service-account', id] with its tokens and check grants below it. A change
// refreshes both the list and the item.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useEffect, useRef, useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useLocation, useSearchParams } from 'react-router';

import { call, callNoContent } from '../../api/client';
import { useApi } from '../../api/context';
import type { components } from '../../api/schema.gen';
import { CopyButton } from '../../components/CopyButton';
import { TextField } from '../../components/Field';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { ErrorSummary } from '../../features/CreateDialogs';
import { formatDateTime } from '../../features/labels';
import { useSession } from '../../session/session';
import { ConfirmDialog, Denied, Section, useConfirm, useFocusHeading, useRefreshSessionOn403 } from './common';
import { SERVICE_ACCOUNT_SCOPES } from './scopes';
import styles from './Settings.module.css';
import { useLeaveGuard } from '../../hooks/useLeaveGuard';

type Account = components['schemas']['ServiceAccountBody'];
type TokenRow = components['schemas']['ServiceAccountTokenBody'];
type Issued = components['schemas']['IssuedServiceAccountToken'];


const LIST_KEY = ['service-accounts'] as const;
/** The admin users list shows service accounts too (disable changes it). */
const ADMIN_USERS_KEY = ['admin-users'] as const;
const itemKey = (id: number) => ['service-account', id] as const;

/** The list and the item are both stale after a change. */
function useRefreshAccount() {
  const queryClient = useQueryClient();
  return async (id: number) => {
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: LIST_KEY }),
      queryClient.invalidateQueries({ queryKey: itemKey(id) }),
      queryClient.invalidateQueries({ queryKey: ADMIN_USERS_KEY }),
    ]);
  };
}

export function ServiceAccounts({ base }: { base: string }) {
  const [params] = useSearchParams();
  const raw = params.get('account');
  const id = raw !== null && /^[1-9][0-9]{0,15}$/.test(raw) ? Number(raw) : null;
  return id === null ? <AccountList base={base} /> : <AccountDetail key={id} id={id} base={base} />;
}

const back = (base: string) => `${base}?tab=service-accounts`;

function AccountList({ base }: { base: string }) {
  const { t } = useTranslation();
  const api = useApi();
  const focusHeading = useFocusHeading();
  const location = useLocation();
  const refreshAccount = useRefreshAccount();
  const refresh = useRefreshSessionOn403();
  const [notice, setNotice] = useState('');
  const disable = useDisable(refreshAccount, refresh);
  const list = useQuery({
    queryKey: LIST_KEY,
    queryFn: ({ signal }) => call(() => api.GET('/api/v1/admin/service-accounts', { signal })),
  });
  // Coming back from one account: the heading takes the focus.
  const cameBack = (location.state as { focus?: string } | null)?.focus === 'list';
  useEffect(() => {
    if (cameBack) focusHeading('sa-title');
  }, [cameBack, focusHeading]);
  return (
    <>
      <Section id="sa-title" title={t('sa.title')}>
        <p className={styles.hint}>{t('sa.explain')}</p>
        <p role="status" className={styles.muted}>
          {notice}
        </p>
        {list.isPending && <Loading />}
        {list.isError && (
          <QueryError
            error={list.error}
            onRetry={() => {
              void list.refetch();
            }}
          />
        )}
        {list.isSuccess && (
          <>
            {list.data.truncated && (
              <p role="status" className={styles.hint}>
                {t('common.truncated')}
              </p>
            )}
            {list.data.items.length === 0 ? (
              <p>{t('sa.none')}</p>
            ) : (
              <AccountTable
                accounts={list.data.items}
                base={base}
                onDisable={(a) => {
                  disable.reset();
                  disable.confirm.ask(a);
                }}
              />
            )}
          </>
        )}
      </Section>
      <Section id="sa-new-title" title={t('sa.create')}>
        <CreateAccount
          onCreated={(a) => {
            setNotice(t('sa.created', { name: a.display_name }));
          }}
        />
      </Section>
      <DisableConfirm
        disable={disable}
        onDone={(name) => {
          setNotice(t('sa.disabledDone', { name }));
          focusHeading('sa-title');
        }}
      />
    </>
  );
}

function AccountTable({
  accounts,
  base,
  onDisable,
}: {
  accounts: Account[];
  base: string;
  onDisable: (a: Account) => void;
}) {
  const { t } = useTranslation();
  return (
    <div className={styles.scroll} role="region" aria-label={t('sa.title')} tabIndex={0}>
      <table className={styles.table}>
        <thead>
          <tr>
            <th scope="col">{t('sa.account')}</th>
            <th scope="col">{t('sa.purpose')}</th>
            <th scope="col">{t('sa.owner')}</th>
            <th scope="col">{t('tokens.state')}</th>
            <th scope="col">{t('sa.activeTokens')}</th>
            <th scope="col">{t('common.actions')}</th>
          </tr>
        </thead>
        <tbody>
          {accounts.map((a) => (
            <tr key={a.id} className={a.state === 'active' ? undefined : styles.rowMuted}>
              <th scope="row">
                {a.display_name} <code>{a.username}</code>
              </th>
              <td>{a.purpose ?? ''}</td>
              <td>
                <OwnerName owner={a.owner} />
              </td>
              <td>
                <StateChip state={a.state} />
              </td>
              <td>{a.active_tokens}</td>
              <td>
                <span className={styles.inline}>
                  <Link
                    className={`${controls.button} ${styles.touch}`}
                    to={`${back(base)}&account=${a.id}`}
                    aria-label={t('sa.manageOf', { name: a.display_name })}
                  >
                    {t('sa.manage')}
                  </Link>
                  {a.state === 'active' && (
                    <button
                      type="button"
                      className={`${styles.dangerQuiet} ${styles.touch}`}
                      aria-label={t('sa.disableOf', { name: a.display_name })}
                      onClick={() => {
                        onDisable(a);
                      }}
                    >
                      {t('sa.disable')}
                    </button>
                  )}
                </span>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

function OwnerName({ owner }: { owner: Account['owner'] }) {
  const { t } = useTranslation();
  if (owner === null) return <>{t('sa.noOwner')}</>;
  return (
    <>
      {owner.username}
      {!owner.active && <span className={controls.badge}>{t('sa.ownerInactive')}</span>}
    </>
  );
}

function StateChip({ state }: { state: string }) {
  const { t } = useTranslation();
  const active = state === 'active';
  return (
    <span className={`${styles.chip} ${active ? styles.chipActive : styles.chipRevoked}`}>
      {active ? t('admin.active') : t('admin.disabled')}
    </span>
  );
}

/** One request for disabling: shared by the list and the detail. */
function useDisable(refreshAccount: (id: number) => Promise<void>, refresh: (e: unknown) => void) {
  const api = useApi();
  const confirm = useConfirm<Account>();
  const mutation = useMutation({
    mutationFn: (a: Account) =>
      call(() => api.POST('/api/v1/admin/users/{id}/disable', { params: { path: { id: a.id } } })),
    onError: refresh,
  });
  return {
    confirm,
    mutation,
    reset: mutation.reset,
    run: async (a: Account, onDone: (name: string) => void) => {
      await mutation.mutateAsync(a);
      await refreshAccount(a.id);
      confirm.close();
      onDone(a.display_name);
    },
  };
}

function DisableConfirm({
  disable,
  onDone,
}: {
  disable: ReturnType<typeof useDisable>;
  onDone: (name: string) => void;
}) {
  const { t } = useTranslation();
  const target = disable.confirm.target;
  return (
    <ConfirmDialog
      open={target !== null}
      title={t('sa.disableTitle')}
      message={<p>{t('sa.disableBody', { name: target?.display_name ?? '' })}</p>}
      confirmLabel={t('sa.disable')}
      busy={disable.mutation.isPending}
      error={disable.mutation.isError ? disable.mutation.error : null}
      onConfirm={() => {
        if (target !== null) {
          disable.run(target, onDone).catch(() => {
            // Shown in the dialog (mutation.error); onError refreshed the session.
          });
        }
      }}
      onCancel={disable.confirm.close}
    />
  );
}

function CreateAccount({ onCreated }: { onCreated: (a: Account) => void }) {
  const { t } = useTranslation();
  const api = useApi();
  const session = useSession();
  const refreshAccount = useRefreshAccount();
  const refresh = useRefreshSessionOn403();
  const [username, setUsername] = useState('');
  const [displayName, setDisplayName] = useState('');
  const [purpose, setPurpose] = useState('');
  const [attempt, setAttempt] = useState(0);

  const userTrim = username.trim();
  const nameTrim = displayName.trim();
  const purposeTrim = purpose.trim();
  const userError = userTrim === '' ? t('sa.usernameRequired') : null;
  const nameError = Array.from(nameTrim).length > 255 ? t('sa.displayNameLong') : null;
  const purposeError =
    purposeTrim === ''
      ? t('sa.purposeRequired')
      : Array.from(purposeTrim).length > 500 || /[\r\n]/.test(purposeTrim)
        ? t('sa.purposeInvalid')
        : null;
  const errors = [userError, nameError, purposeError].filter((e): e is string => e !== null);

  const create = useMutation({
    mutationFn: () =>
      call(() =>
        api.POST('/api/v1/admin/service-accounts', {
          body: {
            username: userTrim,
            ...(nameTrim === '' ? {} : { display_name: nameTrim }),
            purpose: purposeTrim,
            owner_id: session.user.id,
          },
        }),
      ),
    onSuccess: async (made) => {
      setUsername('');
      setDisplayName('');
      setPurpose('');
      setAttempt(0);
      await refreshAccount(made.id);
      onCreated(made);
    },
    onError: refresh,
  });

  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    if (create.isPending) return;
    setAttempt((x) => x + 1);
    if (errors.length > 0) return;
    create.mutate();
  };
  return (
    <form onSubmit={submit} noValidate className={styles.narrow}>
      <ErrorSummary errors={errors} attempt={attempt} />
      <TextField
        label={t('sa.username')}
        value={username}
        onChange={setUsername}
        error={attempt > 0 ? userError : null}
        help={t('sa.usernameHelp')}
        required
      />
      <TextField
        label={t('sa.displayName')}
        value={displayName}
        onChange={setDisplayName}
        error={attempt > 0 ? nameError : null}
        help={t('sa.displayNameHelp')}
      />
      <TextField
        label={t('sa.description')}
        value={purpose}
        onChange={setPurpose}
        error={attempt > 0 ? purposeError : null}
        help={t('sa.descriptionHelp')}
        required
      />
      <p className={controls.help}>{t('sa.ownerHelp', { name: session.user.display_name })}</p>
      {create.isError && <Denied error={create.error} />}
      <button
        type="submit"
        className={`${controls.primary} ${styles.touch}`}
        aria-disabled={create.isPending ? 'true' : undefined}
      >
        {create.isPending ? t('settings.saving') : t('sa.createButton')}
      </button>
    </form>
  );
}

function AccountDetail({ id, base }: { id: number; base: string }) {
  const { t, i18n } = useTranslation();
  const api = useApi();
  const focusHeading = useFocusHeading();
  const refreshAccount = useRefreshAccount();
  const refresh = useRefreshSessionOn403();
  const [notice, setNotice] = useState('');
  const [secret, setSecret] = useState<Issued | null>(null);
  const leave = useLeaveGuard(secret !== null, () => {
    setSecret(null);
  });
  const disable = useDisable(refreshAccount, refresh);
  const account = useQuery({
    queryKey: itemKey(id),
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/admin/service-accounts/{id}', { signal, params: { path: { id } } })),
  });
  useEffect(() => {
    focusHeading('sa-detail-title');
  }, [id, focusHeading]);
  const a = account.data;
  const onRevoked = (name: string) => {
    setNotice(t('sa.revokedDone', { name }));
  };
  return (
    <>
      <p>
        <Link to={back(base)} state={{ focus: 'list' }} className={styles.touch}>
          {t('sa.backToList')}
        </Link>
      </p>
      <p role="status" className={styles.srOnly}>
        {secret === null ? notice : t('sa.tokenCreatedStatus')}
      </p>
      {secret !== null && (
        <SecretPanel
          issued={secret}
          onDone={() => {
            setSecret(null);
            focusHeading('sa-tokens-title');
          }}
        />
      )}
      <Section id="sa-detail-title" title={a?.display_name ?? t('sa.detailTitle')}>
        {account.isPending && <Loading />}
        {account.isError && (
          <QueryError
            error={account.error}
            onRetry={() => {
              void account.refetch();
            }}
          />
        )}
        {a !== undefined && (
          <>
            <dl className={styles.facts}>
              <dt>{t('sa.username')}</dt>
              <dd>
                <code>{a.username}</code>
              </dd>
              <dt>{t('sa.purpose')}</dt>
              <dd>{a.purpose ?? ''}</dd>
              <dt>{t('sa.owner')}</dt>
              <dd>
                <OwnerName owner={a.owner} />
              </dd>
              <dt>{t('tokens.state')}</dt>
              <dd>
                <StateChip state={a.state} />
              </dd>
              <dt>{t('sa.createdAt')}</dt>
              <dd>{formatDateTime(a.created_at, i18n.language)}</dd>
            </dl>
            {a.state === 'active' ? (
              <button
                type="button"
                className={`${styles.dangerQuiet} ${styles.touch}`}
                aria-label={t('sa.disableOf', { name: a.display_name })}
                onClick={() => {
                  disable.reset();
                  disable.confirm.ask(a);
                }}
              >
                {t('sa.disable')}
              </button>
            ) : (
              <p className={styles.warning} role="note">
                {t('sa.isDisabled', { name: a.display_name })}
              </p>
            )}
          </>
        )}
      </Section>
      <Section id="sa-tokens-title" title={t('sa.tokens')}>
        <TokenList id={id} onRevoked={onRevoked} />
      </Section>
      {a?.state === 'active' && (
        <Section id="sa-token-new-title" title={t('sa.issue')}>
          {secret === null ? (
            <IssueToken id={id} onIssued={setSecret} />
          ) : (
            <p className={styles.hint}>{t('tokens.finishFirst')}</p>
          )}
        </Section>
      )}
      <Section id="sa-grants-title" title={t('sa.grants')}>
        <Grants id={id} />
      </Section>
      <DisableConfirm
        disable={disable}
        onDone={(name) => {
          setNotice(t('sa.disabledDone', { name }));
          focusHeading('sa-detail-title');
        }}
      />
      <ConfirmDialog
        open={leave.target !== null}
        title={t('tokens.leaveTitle')}
        message={<p>{t('tokens.leaveBody')}</p>}
        confirmLabel={t('tokens.leaveConfirm')}
        busy={false}
        error={null}
        onConfirm={leave.proceed}
        onCancel={leave.stay}
      />
    </>
  );
}

function SecretPanel({ issued, onDone }: { issued: Issued; onDone: () => void }) {
  const { t, i18n } = useTranslation();
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    ref.current?.focus();
  }, []);
  // Warn on a reload or close while the secret is still on the screen.
  useEffect(() => {
    const warn = (e: BeforeUnloadEvent) => {
      e.preventDefault();
    };
    window.addEventListener('beforeunload', warn);
    return () => {
      window.removeEventListener('beforeunload', warn);
    };
  }, []);
  return (
    <section
      ref={ref}
      tabIndex={-1}
      className={styles.secret}
      aria-labelledby="sa-secret-title"
      data-testid="token-secret"
    >
      <h2 id="sa-secret-title" className={styles.secretTitle}>
        {t('tokens.secretTitle', { name: issued.name })}
      </h2>
      <p>
        <strong>{t('tokens.secretWarning')}</strong>
      </p>
      <code className={styles.secretValue} data-testid="token-value">
        {issued.token}
      </code>
      <p>{t('tokens.expiresOn', { date: formatDateTime(issued.expires_at, i18n.language) })}</p>
      <div className={styles.row}>
        <CopyButton text={issued.token} label={t('tokens.copy')} />
        <button type="button" className={`${controls.primary} ${styles.touch}`} onClick={onDone}>
          {t('tokens.stored')}
        </button>
      </div>
    </section>
  );
}

function stateOf(k: TokenRow): 'revoked' | 'expired' | 'active' {
  if (k.revoked_at !== null) return 'revoked';
  return new Date(k.expires_at).getTime() <= Date.now() ? 'expired' : 'active';
}

function TokenList({ id, onRevoked }: { id: number; onRevoked: (name: string) => void }) {
  const { t, i18n } = useTranslation();
  const api = useApi();
  const focusHeading = useFocusHeading();
  const refreshAccount = useRefreshAccount();
  const refresh = useRefreshSessionOn403();
  const confirm = useConfirm<TokenRow>();
  const tokens = useQuery({
    queryKey: [...itemKey(id), 'tokens'],
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/admin/service-accounts/{id}/tokens', { signal, params: { path: { id } } })),
  });
  const revoke = useMutation({
    mutationFn: (k: TokenRow) =>
      callNoContent(() =>
        api.DELETE('/api/v1/admin/service-accounts/{id}/tokens/{token_id}', {
          params: { path: { id, token_id: k.id } },
        }),
      ),
    onSuccess: async (_r, k) => {
      await refreshAccount(id);
      confirm.close();
      onRevoked(k.name);
      focusHeading('sa-tokens-title');
    },
    onError: refresh,
  });
  if (tokens.isPending) return <Loading />;
  if (tokens.isError) {
    return (
      <QueryError
        error={tokens.error}
        onRetry={() => {
          void tokens.refetch();
        }}
      />
    );
  }
  const rows = tokens.data.items;
  if (rows.length === 0) return <p>{t('sa.noTokens')}</p>;
  return (
    <>
      <div className={styles.scroll} role="region" aria-label={t('sa.tokens')} tabIndex={0}>
        <table className={styles.table}>
          <thead>
            <tr>
              <th scope="col">{t('tokens.name')}</th>
              <th scope="col">{t('tokens.scopes')}</th>
              <th scope="col">{t('tokens.expires')}</th>
              <th scope="col">{t('tokens.lastUsed')}</th>
              <th scope="col">{t('tokens.state')}</th>
              <th scope="col">{t('common.actions')}</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((k) => {
              const state = stateOf(k);
              return (
                <tr key={k.id} className={state === 'active' ? undefined : styles.rowMuted}>
                  <th scope="row">{k.name}</th>
                  <td>{k.scopes.join(', ')}</td>
                  <td>{formatDateTime(k.expires_at, i18n.language)}</td>
                  <td>
                    {k.last_used_at === null ? t('tokens.never') : formatDateTime(k.last_used_at, i18n.language)}
                  </td>
                  <td>
                    <span
                      className={`${styles.chip} ${
                        state === 'active'
                          ? styles.chipActive
                          : state === 'expired'
                            ? styles.chipExpired
                            : styles.chipRevoked
                      }`}
                    >
                      {state === 'revoked'
                        ? t('tokens.revoked')
                        : state === 'expired'
                          ? t('tokens.expired')
                          : t('tokens.active')}
                    </span>
                  </td>
                  <td>
                    {state === 'active' && (
                      <button
                        type="button"
                        className={`${styles.dangerQuiet} ${styles.touch}`}
                        aria-label={t('tokens.revokeOf', { name: k.name })}
                        onClick={() => {
                          revoke.reset();
                          confirm.ask(k);
                        }}
                      >
                        {t('tokens.revoke')}
                      </button>
                    )}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      <ConfirmDialog
        open={confirm.target !== null}
        title={t('tokens.revokeTitle')}
        message={<p>{t('tokens.revokeBody', { name: confirm.target?.name ?? '' })}</p>}
        confirmLabel={t('tokens.revoke')}
        busy={revoke.isPending}
        error={revoke.isError ? revoke.error : null}
        onConfirm={() => {
          if (confirm.target !== null) revoke.mutate(confirm.target);
        }}
        onCancel={confirm.close}
      />
    </>
  );
}

function IssueToken({ id, onIssued }: { id: number; onIssued: (c: Issued) => void }) {
  const { t } = useTranslation();
  const api = useApi();
  const refreshAccount = useRefreshAccount();
  const refresh = useRefreshSessionOn403();
  const [name, setName] = useState('');
  const [scopes, setScopes] = useState<string[]>(['read_repository']);
  const [days, setDays] = useState('90');
  const [agent, setAgent] = useState(false);
  const [attempt, setAttempt] = useState(0);

  const nameTrim = name.trim();
  const nameError =
    nameTrim === '' ? t('tokens.nameRequired') : Array.from(nameTrim).length > 100 ? t('tokens.nameLong') : null;
  const scopeError = scopes.length === 0 ? t('tokens.scopeRequired') : null;
  const n = Number(days);
  const daysError =
    days.trim() === '' || !Number.isInteger(n) || n < 1 || n > 365 ? t('tokens.daysInvalid') : null;
  const errors = [nameError, scopeError, daysError].filter((e): e is string => e !== null);

  const issue = useMutation({
    // The answer holds the secret: the mutation cache must not keep it.
    gcTime: 0,
    mutationFn: () =>
      call(() =>
        api.POST('/api/v1/admin/service-accounts/{id}/tokens', {
          params: { path: { id } },
          body: { name: nameTrim, scopes, expires_in_days: n, is_agent: agent },
        }),
      ),
    onSuccess: async (issued) => {
      setName('');
      setScopes(['read_repository']);
      setAgent(false);
      setAttempt(0);
      onIssued(issued);
      // The secret now lives in the parent's state only.
      issue.reset();
      await refreshAccount(id);
    },
    onError: refresh,
  });

  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    if (issue.isPending) return;
    setAttempt((x) => x + 1);
    if (errors.length > 0) return;
    issue.mutate();
  };
  return (
    <form onSubmit={submit} noValidate className={styles.narrow}>
      <ErrorSummary errors={errors} attempt={attempt} />
      <p className={styles.hint}>{t('sa.issueExplain')}</p>
      <TextField
        label={t('tokens.name')}
        value={name}
        onChange={setName}
        error={attempt > 0 ? nameError : null}
        help={t('tokens.nameHelp')}
        required
      />
      <fieldset className={controls.field} aria-describedby="sa-scopes-help">
        <legend className={controls.label}>{t('tokens.scopes')}</legend>
        <div className={styles.checks}>
          {SERVICE_ACCOUNT_SCOPES.map((s) => (
            <label key={s} className={`${styles.check} ${styles.touch}`}>
              <input
                type="checkbox"
                checked={scopes.includes(s)}
                onChange={(e) => {
                  setScopes(e.target.checked ? [...scopes, s] : scopes.filter((x) => x !== s));
                }}
              />
              <code>{s}</code>
            </label>
          ))}
        </div>
        <p id="sa-scopes-help" className={controls.help}>
          {t('sa.scopesHelp')}
        </p>
        {attempt > 0 && scopeError !== null && <p className={controls.fieldError}>{scopeError}</p>}
      </fieldset>
      <TextField
        label={t('tokens.days')}
        value={days}
        onChange={setDays}
        error={attempt > 0 ? daysError : null}
        help={t('tokens.daysHelp')}
        required
      />
      <div className={controls.field}>
        <label className={`${styles.check} ${styles.touch}`}>
          <input
            type="checkbox"
            checked={agent}
            aria-describedby="sa-agent-help"
            onChange={(e) => {
              setAgent(e.target.checked);
            }}
          />
          <span>{t('tokens.agent')}</span>
        </label>
        <p id="sa-agent-help" className={controls.help}>
          {t('tokens.agentHelp')}
        </p>
      </div>
      {issue.isError && <Denied error={issue.error} />}
      <button
        type="submit"
        className={`${controls.primary} ${styles.touch}`}
        aria-disabled={issue.isPending ? 'true' : undefined}
      >
        {issue.isPending ? t('settings.saving') : t('sa.issueButton')}
      </button>
    </form>
  );
}

/** The check namespaces the account may report, by project (read-only here).
 * The API does not list project memberships; this is the project range it
 * does expose. */
function Grants({ id }: { id: number }) {
  const { t } = useTranslation();
  const api = useApi();
  const grants = useQuery({
    queryKey: [...itemKey(id), 'check-grants'],
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/admin/service-accounts/{id}/check-grants', { signal, params: { path: { id } } }),
      ),
  });
  if (grants.isPending) return <Loading />;
  if (grants.isError) {
    return (
      <QueryError
        error={grants.error}
        onRetry={() => {
          void grants.refetch();
        }}
      />
    );
  }
  if (grants.data.grants.length === 0) return <p>{t('sa.noGrants')}</p>;
  return (
    <>
      <p className={styles.hint}>{t('sa.grantsExplain')}</p>
      <ul className={styles.results}>
        {grants.data.grants.map((g) => (
          <li key={`${g.project_id}/${g.namespace}`}>
            <span>{t('sa.projectN', { id: g.project_id })}</span>
            <code>{g.namespace}</code>
          </li>
        ))}
      </ul>
    </>
  );
}
