// Personal access tokens (ADR-0019 Amendment M2): created only from a browser
// session. The secret is in the answer of the create request, once. It lives
// in this component's state only (never in the query cache, storage or the
// URL), so leaving the page or reloading loses it for good; the person is told
// so before they leave.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useEffect, useRef, useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';

import { call, callNoContent } from '../../api/client';
import { useApi } from '../../api/context';
import type { components } from '../../api/schema.gen';
import { TextField } from '../../components/Field';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { CopyButton } from '../../components/CopyButton';
import { ErrorSummary } from '../../features/CreateDialogs';
import { formatDateTime } from '../../features/labels';
import { useSession } from '../../session/session';
import { ConfirmDialog, Denied, Section, focusHeading, useConfirm, useRefreshSessionOn403 } from './common';
import styles from './Settings.module.css';

type Token = components['schemas']['TokenBody'];
type Created = components['schemas']['CreatedToken'];

const SCOPES = ['read_repository', 'write_repository', 'read_api', 'api'] as const;
const KEY = ['tokens'];

export function Tokens() {
  const { t } = useTranslation();
  const api = useApi();
  const tokens = useQuery({
    queryKey: KEY,
    queryFn: ({ signal }) => call(() => api.GET('/api/v1/user/tokens', { signal })),
  });
  // The one place the secret exists.
  const [secret, setSecret] = useState<Created | null>(null);
  return (
    <>
      {secret !== null && (
        <SecretPanel
          created={secret}
          onDone={() => {
            setSecret(null);
            focusHeading('tokens-title');
          }}
        />
      )}
      <Section id="tokens-title" title={t('tokens.title')}>
        <p className={styles.hint}>{t('tokens.explain')}</p>
        {tokens.isPending && <Loading />}
        {tokens.isError && (
          <QueryError
            error={tokens.error}
            onRetry={() => {
              void tokens.refetch();
            }}
          />
        )}
        {tokens.isSuccess && <TokenTable tokens={tokens.data.items} />}
      </Section>
      <Section id="tokens-new-title" title={t('tokens.create')}>
        <CreateToken onCreated={setSecret} />
      </Section>
    </>
  );
}

function SecretPanel({ created, onDone }: { created: Created; onDone: () => void }) {
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
  const login = `echo "<${t('tokens.secretPlaceholder')}>" | gstn auth login --with-token`;
  return (
    <section
      ref={ref}
      tabIndex={-1}
      role="alert"
      className={styles.secret}
      aria-labelledby="secret-title"
      data-testid="token-secret"
    >
      <h2 id="secret-title" className={styles.sectionTitle}>
        {t('tokens.secretTitle', { name: created.name })}
      </h2>
      <p>
        <strong>{t('tokens.secretWarning')}</strong>
      </p>
      <code className={styles.secretValue} data-testid="token-value">
        {created.token}
      </code>
      <CopyButton text={created.token} label={t('tokens.copy')} />
      <p>
        {t('tokens.expiresOn', { date: formatDateTime(created.expires_at, i18n.language) })}
      </p>
      <p>{t('tokens.howTo')}</p>
      <pre className={styles.pre}>
        <code>{login}</code>
      </pre>
      <button type="button" className={controls.primary} onClick={onDone}>
        {t('tokens.stored')}
      </button>
    </section>
  );
}

function stateOf(k: Token): 'revoked' | 'expired' | 'active' {
  if (k.revoked_at !== null) return 'revoked';
  return new Date(k.expires_at).getTime() <= Date.now() ? 'expired' : 'active';
}

function TokenTable({ tokens }: { tokens: Token[] }) {
  const { t, i18n } = useTranslation();
  const api = useApi();
  const queryClient = useQueryClient();
  const refresh = useRefreshSessionOn403();
  const confirm = useConfirm<Token>();
  const revoke = useMutation({
    mutationFn: (id: number) =>
      callNoContent(() => api.DELETE('/api/v1/user/tokens/{id}', { params: { path: { id } } })),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: KEY });
      confirm.close();
      focusHeading('tokens-title');
    },
    onError: refresh,
  });
  if (tokens.length === 0) return <p>{t('tokens.none')}</p>;
  return (
    <>
      <div className={styles.scroll} role="region" aria-label={t('tokens.title')} tabIndex={0}>
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
            {tokens.map((k) => {
              const state = stateOf(k);
              return (
                <tr key={k.id}>
                  <th scope="row">{k.name}</th>
                  <td>{k.scopes.join(', ')}</td>
                  <td>{formatDateTime(k.expires_at, i18n.language)}</td>
                  <td>
                    {k.last_used_at === null
                      ? t('tokens.never')
                      : formatDateTime(k.last_used_at, i18n.language)}
                  </td>
                  <td>
                    {state === 'revoked'
                      ? t('tokens.revoked')
                      : state === 'expired'
                        ? t('tokens.expired')
                        : t('tokens.active')}
                  </td>
                  <td>
                    {state === 'active' && (
                      <button
                        type="button"
                        className={styles.danger}
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
          if (confirm.target !== null) revoke.mutate(confirm.target.id);
        }}
        onCancel={confirm.close}
      />
    </>
  );
}

function CreateToken({ onCreated }: { onCreated: (c: Created) => void }) {
  const { t } = useTranslation();
  const api = useApi();
  const session = useSession();
  const queryClient = useQueryClient();
  const refresh = useRefreshSessionOn403();
  const [name, setName] = useState('');
  const [scopes, setScopes] = useState<string[]>(['read_repository']);
  const [days, setDays] = useState('90');
  const [agent, setAgent] = useState(false);
  const [attempt, setAttempt] = useState(0);

  const nameTrim = name.trim();
  const nameError =
    nameTrim === ''
      ? t('tokens.nameRequired')
      : Array.from(nameTrim).length > 100
        ? t('tokens.nameLong')
        : null;
  const scopeError = scopes.length === 0 ? t('tokens.scopeRequired') : null;
  const n = Number(days);
  const daysError =
    days.trim() === '' || !Number.isInteger(n) || n < 1 || n > 365 ? t('tokens.daysInvalid') : null;
  const errors = [nameError, scopeError, daysError].filter((e): e is string => e !== null);

  const create = useMutation({
    // The answer holds the secret: the mutation cache must not keep it.
    gcTime: 0,
    mutationFn: () =>
      call(() =>
        api.POST('/api/v1/user/tokens', {
          body: { name: nameTrim, scopes, expires_in_days: n, is_agent: agent },
        }),
      ),
    onSuccess: async (created) => {
      setName('');
      setScopes(['read_repository']);
      setAgent(false);
      setAttempt(0);
      onCreated(created);
      await queryClient.invalidateQueries({ queryKey: KEY });
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
  const all = session.user.is_admin ? [...SCOPES, 'admin'] : [...SCOPES];
  return (
    <form onSubmit={submit} noValidate className={styles.narrow}>
      <ErrorSummary errors={errors} attempt={attempt} />
      <TextField
        label={t('tokens.name')}
        value={name}
        onChange={setName}
        error={attempt > 0 ? nameError : null}
        help={t('tokens.nameHelp')}
        required
      />
      <fieldset className={controls.field}>
        <legend className={controls.label}>{t('tokens.scopes')}</legend>
        <div className={styles.checks}>
          {all.map((s) => (
            <label key={s} className={styles.check}>
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
        <label className={styles.check}>
          <input
            type="checkbox"
            checked={agent}
            onChange={(e) => {
              setAgent(e.target.checked);
            }}
          />
          <span>{t('tokens.agent')}</span>
        </label>
        <p className={controls.help}>{t('tokens.agentHelp')}</p>
      </div>
      {create.isError && <Denied error={create.error} />}
      <button
        type="submit"
        className={controls.primary}
        aria-disabled={create.isPending ? 'true' : undefined}
      >
        {create.isPending ? t('settings.saving') : t('tokens.createButton')}
      </button>
    </form>
  );
}
