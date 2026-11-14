// SSH public keys (M4 C13, ADR-0033 §2): list, add and delete one's own keys
// for git over SSH, and the server's host key fingerprint to compare on the
// first connection. Adding works in a browser session only (the API refuses
// a token). A key is public data: nothing here is secret.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useState, type SyntheticEvent } from 'react';
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
import { ConfirmDialog, Denied, Section, useConfirm, useFocusHeading, useRefreshSessionOn403 } from './common';
import styles from './Settings.module.css';

type SshKey = components['schemas']['SshKeyBody'];
type SshServer = components['schemas']['SshServerBody'];

const KEY = ['ssh-keys'];
/** The server refuses longer lines (FG-AUTH-0023 too_long). */
const MAX_KEY_CHARS = 8192;
const ACCEPTED = /^(ssh-ed25519|ecdsa-sha2-nistp(256|384|521)) [A-Za-z0-9+/=]+( .*)?$/;

export function SshKeys() {
  const { t } = useTranslation();
  const api = useApi();
  const keys = useQuery({
    queryKey: KEY,
    queryFn: ({ signal }) => call(() => api.GET('/api/v1/user/ssh-keys', { signal })),
  });
  return (
    <>
      <Section id="ssh-keys-title" title={t('sshKeys.title')}>
        <p className={styles.hint}>{t('sshKeys.explain')}</p>
        {keys.isPending && <Loading />}
        {keys.isError && (
          <QueryError
            error={keys.error}
            onRetry={() => {
              void keys.refetch();
            }}
          />
        )}
        {keys.isSuccess && (
          <>
            <ServerFacts server={keys.data.ssh} />
            <KeyTable keys={keys.data.items} />
          </>
        )}
      </Section>
      <Section id="ssh-keys-new-title" title={t('sshKeys.add')}>
        <AddKey />
      </Section>
    </>
  );
}

function ServerFacts({ server }: { server: SshServer | null }) {
  const { t } = useTranslation();
  if (server === null) return <p className={styles.hint}>{t('sshKeys.disabled')}</p>;
  const clone = `git clone ssh://git@${server.clone_host}/<group>/<project>.git`;
  return (
    <div data-testid="ssh-server">
      <p>{t('sshKeys.cloneHow')}</p>
      <pre className={styles.pre}>
        <code>{clone}</code>
      </pre>
      <p>{t('sshKeys.hostKey')}</p>
      <ul>
        {server.host_key_fingerprints.map((fp) => (
          <li key={fp}>
            <code>{fp}</code>
          </li>
        ))}
      </ul>
    </div>
  );
}

function KeyTable({ keys }: { keys: SshKey[] }) {
  const { t, i18n } = useTranslation();
  const focusHeading = useFocusHeading();
  const api = useApi();
  const queryClient = useQueryClient();
  const refresh = useRefreshSessionOn403();
  const confirm = useConfirm<SshKey>();
  const remove = useMutation({
    mutationFn: (id: number) =>
      callNoContent(() => api.DELETE('/api/v1/user/ssh-keys/{id}', { params: { path: { id } } })),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: KEY });
      confirm.close();
      focusHeading('ssh-keys-title');
    },
    onError: refresh,
  });
  if (keys.length === 0) return <p>{t('sshKeys.none')}</p>;
  return (
    <>
      <div className={styles.scroll} role="region" aria-label={t('sshKeys.title')} tabIndex={0}>
        <table className={styles.table}>
          <thead>
            <tr>
              <th scope="col">{t('sshKeys.keyTitle')}</th>
              <th scope="col">{t('sshKeys.fingerprint')}</th>
              <th scope="col">{t('sshKeys.added')}</th>
              <th scope="col">{t('sshKeys.lastUsed')}</th>
              <th scope="col">{t('common.actions')}</th>
            </tr>
          </thead>
          <tbody>
            {keys.map((k) => (
              <tr key={k.id}>
                <th scope="row">{k.title}</th>
                <td>
                  <code>{k.fingerprint}</code>
                  <span className={styles.hint}> ({k.algorithm})</span>
                </td>
                <td>{formatDateTime(k.created_at, i18n.language)}</td>
                <td>
                  {k.last_used_at === null
                    ? t('sshKeys.never')
                    : formatDateTime(k.last_used_at, i18n.language)}
                </td>
                <td>
                  <button
                    type="button"
                    className={styles.dangerQuiet}
                    aria-label={t('sshKeys.deleteOf', { title: k.title })}
                    onClick={() => {
                      remove.reset();
                      confirm.ask(k);
                    }}
                  >
                    {t('sshKeys.delete')}
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <ConfirmDialog
        open={confirm.target !== null}
        title={t('sshKeys.deleteTitle')}
        message={<p>{t('sshKeys.deleteBody', { title: confirm.target?.title ?? '' })}</p>}
        confirmLabel={t('sshKeys.delete')}
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

function AddKey() {
  const { t } = useTranslation();
  const api = useApi();
  const queryClient = useQueryClient();
  const refresh = useRefreshSessionOn403();
  const [key, setKey] = useState('');
  const [title, setTitle] = useState('');
  const [attempt, setAttempt] = useState(0);
  const [added, setAdded] = useState<SshKey | null>(null);

  const keyTrim = key.trim();
  const keyError =
    keyTrim === ''
      ? t('sshKeys.keyRequired')
      : keyTrim.length > MAX_KEY_CHARS || keyTrim.includes('\n') || !ACCEPTED.test(keyTrim)
        ? t('sshKeys.keyInvalid')
        : null;
  const titleError = Array.from(title.trim()).length > 100 ? t('sshKeys.titleLong') : null;
  const errors = [keyError, titleError].filter((e): e is string => e !== null);

  const create = useMutation({
    mutationFn: () =>
      call(() =>
        api.POST('/api/v1/user/ssh-keys', {
          body: { key: keyTrim, title: title.trim() === '' ? null : title.trim() },
        }),
      ),
    onSuccess: async (k) => {
      setKey('');
      setTitle('');
      setAttempt(0);
      setAdded(k);
      await queryClient.invalidateQueries({ queryKey: KEY });
    },
    onError: refresh,
  });

  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    if (create.isPending) return;
    setAttempt((x) => x + 1);
    if (errors.length > 0) return;
    setAdded(null);
    create.mutate();
  };
  return (
    <form onSubmit={submit} noValidate className={styles.narrow}>
      <p role="status" className={styles.srOnly}>
        {added === null ? '' : t('sshKeys.addedStatus', { title: added.title })}
      </p>
      <ErrorSummary errors={errors} attempt={attempt} />
      <div className={controls.field}>
        <label htmlFor="ssh-key-line" className={controls.label}>
          {t('sshKeys.key')}
        </label>
        <textarea
          id="ssh-key-line"
          className={controls.input}
          value={key}
          rows={4}
          spellCheck={false}
          aria-invalid={attempt > 0 && keyError !== null ? 'true' : undefined}
          aria-describedby="ssh-key-help"
          required
          onChange={(e) => {
            setKey(e.target.value);
          }}
        />
        <p id="ssh-key-help" className={controls.help}>
          {t('sshKeys.keyHelp')}
        </p>
        {attempt > 0 && keyError !== null && <p className={controls.fieldError}>{keyError}</p>}
      </div>
      <TextField
        label={t('sshKeys.keyTitle')}
        value={title}
        onChange={setTitle}
        error={attempt > 0 ? titleError : null}
        help={t('sshKeys.titleHelp')}
      />
      {create.isError && <Denied error={create.error} />}
      {added !== null && (
        <p data-testid="ssh-key-added">
          {t('sshKeys.addedStatus', { title: added.title })} <code>{added.fingerprint}</code>{' '}
          <CopyButton text={added.fingerprint} label={t('sshKeys.copyFingerprint')} />
        </p>
      )}
      <button
        type="submit"
        className={controls.primary}
        aria-disabled={create.isPending ? 'true' : undefined}
      >
        {create.isPending ? t('settings.saving') : t('sshKeys.addButton')}
      </button>
    </form>
  );
}
