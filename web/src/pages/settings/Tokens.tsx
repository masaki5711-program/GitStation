// Personal access tokens (ADR-0019 Amendment M2): created only from a browser
// session. The secret is in the answer of the create request, once. It lives
// in this component's state only (never in the query cache, storage or the
// URL), so leaving the page or reloading loses it for good; the person is told
// so before they leave.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useEffect, useRef, useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { useNavigate } from 'react-router';

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
import { ConfirmDialog, Denied, Section, useConfirm, useFocusHeading, useRefreshSessionOn403 } from './common';
import styles from './Settings.module.css';

type Token = components['schemas']['TokenBody'];
type Created = components['schemas']['CreatedToken'];

const SCOPES = ['read_repository', 'write_repository', 'read_api', 'api'] as const;
const KEY = ['tokens'];

export function Tokens() {
  const { t } = useTranslation();
  const focusHeading = useFocusHeading();
  const api = useApi();
  const tokens = useQuery({
    queryKey: KEY,
    queryFn: ({ signal }) => call(() => api.GET('/api/v1/user/tokens', { signal })),
  });
  // The one place the secret exists.
  const [secret, setSecret] = useState<Created | null>(null);
  const leave = useLeaveGuard(secret !== null, () => {
    setSecret(null);
  });
  return (
    <>
      {/* Always in the page, so its text is announced when it changes. */}
      <p role="status" className={styles.srOnly}>
        {secret === null ? '' : t('tokens.createdStatus')}
      </p>
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
        {secret === null ? (
          <CreateToken onCreated={setSecret} />
        ) : (
          <p className={styles.hint}>{t('tokens.finishFirst')}</p>
        )}
      </Section>
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

/** While the secret is on the screen, leaving asks first (the secret would
 * be lost for good). The app uses a plain BrowserRouter, so a router blocker
 * is not available:
 * - a click on an in-app link is intercepted (same-origin, not new-tab, not a
 *   download, not a jump inside this page);
 * - browser Back / Forward: a sentinel history entry sits on top of the page;
 *   Back lands on the page's own entry (same address, nothing re-rendered),
 *   the question is asked, and Cancel puts the sentinel back;
 * - reload and close are covered by `beforeunload` in the panel.
 * A navigation the app starts by code (`navigate()`) cannot be caught without
 * a data router; none of the screens does that while the secret is shown. */
function useLeaveGuard(active: boolean, onLeave: () => void) {
  const navigate = useNavigate();
  const [target, setTarget] = useState<{ kind: 'link'; to: string } | { kind: 'back' } | null>(null);
  const armed = useRef(false);
  useEffect(() => {
    if (!active) return;
    const arm = () => {
      window.history.pushState({ ...(window.history.state as object | null), gsSecretGuard: true }, '');
      armed.current = true;
    };
    arm();
    const onClick = (e: MouseEvent) => {
      if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
      const anchor = e.target instanceof Element ? e.target.closest('a[href]') : null;
      if (!(anchor instanceof HTMLAnchorElement)) return;
      if (anchor.hasAttribute('download')) return;
      const kind = anchor.getAttribute('target');
      if (kind !== null && kind !== '' && kind !== '_self') return;
      if (anchor.closest('dialog') !== null) return;
      const url = new URL(anchor.href, window.location.href);
      if (url.origin !== window.location.origin) return;
      // A jump inside this page loses nothing.
      if (url.pathname === window.location.pathname && url.search === window.location.search) return;
      e.preventDefault();
      e.stopPropagation();
      setTarget({ kind: 'link', to: url.pathname + url.search + url.hash });
    };
    const onPop = () => {
      // Back consumed the sentinel (or a Forward moved off it).
      if (!armed.current) return;
      armed.current = false;
      setTarget({ kind: 'back' });
    };
    document.addEventListener('click', onClick, true);
    window.addEventListener('popstate', onPop);
    return () => {
      document.removeEventListener('click', onClick, true);
      window.removeEventListener('popstate', onPop);
      // Leave no sentinel behind when the secret goes away on this page.
      if (armed.current && (window.history.state as { gsSecretGuard?: boolean } | null)?.gsSecretGuard === true) {
        armed.current = false;
        window.history.back();
      }
    };
  }, [active]);
  return {
    target,
    stay: () => {
      if (target?.kind === 'back') {
        window.history.pushState({ ...(window.history.state as object | null), gsSecretGuard: true }, '');
        armed.current = true;
      }
      setTarget(null);
    },
    proceed: () => {
      const to = target;
      setTarget(null);
      onLeave();
      if (to?.kind === 'link') void navigate(to.to);
      // Back: the sentinel is already gone; go on back one more entry.
      else window.history.back();
    },
  };
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
      className={styles.secret}
      aria-labelledby="secret-title"
      data-testid="token-secret"
    >
      <h2 id="secret-title" className={styles.secretTitle}>
        {t('tokens.secretTitle', { name: created.name })}
      </h2>
      <p>
        <strong>{t('tokens.secretWarning')}</strong>
      </p>
      <code className={styles.secretValue} data-testid="token-value">
        {created.token}
      </code>
      <p>
        {t('tokens.expiresOn', { date: formatDateTime(created.expires_at, i18n.language) })}
      </p>
      <p>{t('tokens.howTo')}</p>
      <pre className={styles.pre}>
        <code>{login}</code>
      </pre>
      <div className={styles.row}>
        <CopyButton text={created.token} label={t('tokens.copy')} />
        <button type="button" className={controls.primary} onClick={onDone}>
          {t('tokens.stored')}
        </button>
      </div>
    </section>
  );
}

function stateOf(k: Token): 'revoked' | 'expired' | 'active' {
  if (k.revoked_at !== null) return 'revoked';
  return new Date(k.expires_at).getTime() <= Date.now() ? 'expired' : 'active';
}

function TokenTable({ tokens }: { tokens: Token[] }) {
  const { t, i18n } = useTranslation();
  const focusHeading = useFocusHeading();
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
                <tr key={k.id} className={state === 'active' ? undefined : styles.rowMuted}>
                  <th scope="row">{k.name}</th>
                  <td>{k.scopes.join(', ')}</td>
                  <td>{formatDateTime(k.expires_at, i18n.language)}</td>
                  <td>
                    {k.last_used_at === null
                      ? t('tokens.never')
                      : formatDateTime(k.last_used_at, i18n.language)}
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
                        className={styles.dangerQuiet}
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
    // The answer holds the secret: once it is handed on, let go of the
    // mutation (gcTime 0 then drops it from the cache).
    create.mutate(undefined, {
      onSuccess: () => {
        create.reset();
      },
    });
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
