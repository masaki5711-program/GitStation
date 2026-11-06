// `/{project}/-/webhooks` (U3a, Maintainer+): the project's webhooks - list,
// create, edit, enable / disable, rotate the secret, delete. The server decides
// who may (a Developer gets its reason, shown as it came).
//
// - The URL is write-only: reads show the `origin`; editing leaves the URL
//   empty to keep it, and only the fields that changed are sent.
// - The secret of a create or a rotation is shown once, in this component's
//   state only (never the query cache, storage or the URL), with a warning;
//   a live region announces it, and the panel takes the focus.
// - The event types are checkboxes grouped by class, from the API's list
//   (`EventTypePicker`); `*` ("all events") is the default. The server still
//   judges the choice (a refusal is shown).
import { useEffect, useRef, useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import {
  useCreateWebhook,
  useDeleteWebhook,
  useRotateWebhookSecret,
  useUpdateWebhook,
  useWebhooks,
  type Webhook,
} from '../../api/webhooks';
import { TextField } from '../../components/Field';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { CopyButton } from '../../components/CopyButton';
import { ErrorSummary } from '../../features/CreateDialogs';
import { formatDateTime } from '../../features/labels';
import { encodePath } from '../../features/repoUrls';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { Modal } from '../../ui/Modal';
import { ConfirmDialog, Denied, Section, useConfirm, useFocusHeading, useRefreshSessionOn403 } from '../settings/common';
import styles from '../settings/Settings.module.css';
import { EnabledChip, StopReason } from './common';
import { ALL_EVENTS, EventTypePicker } from './EventTypePicker';

const sameSet = (a: string[], b: string[]) => a.length === b.length && a.every((x) => b.includes(x));

/** The one place a secret exists. */
interface Shown {
  origin: string;
  secret: string;
  /** Set after a rotation: the previous secret still signs until then. */
  previousUntil: string | null;
  announce: string;
}

export function WebhooksScreen({ projectId, project }: { projectId: number; project: string }) {
  const { t } = useTranslation();
  const hooks = useWebhooks(projectId);
  const focusHeading = useFocusHeading();
  const [shown, setShown] = useState<Shown | null>(null);
  const [message, setMessage] = useState('');
  useDocumentTitle(t('webhooks.title'));
  return (
    <>
      <h1 className={styles.title}>{t('webhooks.title')}</h1>
      <p className={styles.hint}>{t('webhooks.explain')}</p>
      {/* Always in the page, so a change of its text is announced. */}
      <p role="status" className={styles.srOnly}>
        {shown === null ? '' : shown.announce}
      </p>
      <p role="status" className={styles.muted}>
        {message}
      </p>
      {shown !== null && (
        <SecretPanel
          shown={shown}
          onDone={() => {
            setShown(null);
            focusHeading('webhooks-title');
          }}
        />
      )}
      <Section id="webhooks-title" title={t('webhooks.listTitle')}>
        {hooks.isPending && <Loading />}
        {hooks.isError && (
          <QueryError
            error={hooks.error}
            onRetry={() => {
              void hooks.refetch();
            }}
          />
        )}
        {hooks.isSuccess && (
          <HookTable
            projectId={projectId}
            project={project}
            items={hooks.data.items}
            onMessage={setMessage}
            onSecret={setShown}
          />
        )}
      </Section>
      <Section id="webhooks-new-title" title={t('webhooks.createTitle')}>
        {shown === null ? (
          <CreateForm
            projectId={projectId}
            onCreated={(hook, secret) => {
              setMessage('');
              setShown({
                origin: hook.origin,
                secret,
                previousUntil: null,
                announce: t('webhooks.createdStatus'),
              });
            }}
          />
        ) : (
          <p className={styles.hint}>{t('webhooks.finishFirst')}</p>
        )}
      </Section>
    </>
  );
}

function SecretPanel({ shown, onDone }: { shown: Shown; onDone: () => void }) {
  const { t, i18n } = useTranslation();
  const ref = useRef<HTMLElement>(null);
  // Focus the panel; while a dialog is still closing the page behind it is
  // inert, so try again until no dialog is open.
  useEffect(() => {
    let timer: number | null = null;
    let tries = 0;
    const attempt = () => {
      tries += 1;
      if (document.querySelector('dialog[open]') !== null && tries < 50) {
        timer = window.setTimeout(attempt, 20);
        return;
      }
      timer = null;
      ref.current?.focus();
    };
    attempt();
    return () => {
      if (timer !== null) window.clearTimeout(timer);
    };
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
      aria-labelledby="webhook-secret-title"
      data-testid="webhook-secret"
    >
      <h2 id="webhook-secret-title" className={styles.secretTitle}>
        {t('webhooks.secretTitle', { origin: shown.origin })}
      </h2>
      <p>
        <strong>{t('webhooks.secretWarning')}</strong>
      </p>
      <code className={styles.secretValue} data-testid="webhook-secret-value">
        {shown.secret}
      </code>
      {shown.previousUntil !== null && (
        <p>{t('webhooks.previousUntil', { date: formatDateTime(shown.previousUntil, i18n.language) })}</p>
      )}
      <p>{t('webhooks.secretHowTo')}</p>
      <div className={styles.row}>
        <CopyButton text={shown.secret} label={t('webhooks.copySecret')} />
        <button type="button" className={controls.primary} onClick={onDone}>
          {t('webhooks.stored')}
        </button>
      </div>
    </section>
  );
}

function CreateForm({
  projectId,
  onCreated,
}: {
  projectId: number;
  onCreated: (hook: Webhook, secret: string) => void;
}) {
  const { t } = useTranslation();
  const refresh = useRefreshSessionOn403();
  const create = useCreateWebhook(projectId);
  const [url, setUrl] = useState('');
  const [types, setTypes] = useState<string[]>([ALL_EVENTS]);
  const [attempt, setAttempt] = useState(0);
  const urlError = url.trim() === '' ? t('webhooks.urlRequired') : null;
  const typesError = types.length === 0 ? t('webhooks.eventTypesRequired') : null;
  const errors = [urlError, typesError].filter((e): e is string => e !== null);
  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    if (create.isPending) return;
    setAttempt((x) => x + 1);
    if (errors.length > 0) return;
    create.mutate(
      { url: url.trim(), event_types: types },
      {
        onSuccess: (created) => {
          setUrl('');
          setTypes([ALL_EVENTS]);
          setAttempt(0);
          onCreated(created.webhook, created.secret);
          // Let go of the answer that holds the secret.
          create.reset();
        },
        onError: refresh,
      },
    );
  };
  return (
    <form onSubmit={submit} noValidate className={styles.narrow}>
      <ErrorSummary errors={errors} attempt={attempt} />
      <TextField
        label={t('webhooks.url')}
        value={url}
        onChange={setUrl}
        error={attempt > 0 ? urlError : null}
        help={t('webhooks.urlHelp')}
        required
      />
      <EventTypePicker
        projectId={projectId}
        value={types}
        onChange={setTypes}
        error={attempt > 0 ? typesError : null}
      />
      {create.isError && <Denied error={create.error} />}
      <button type="submit" className={controls.primary} aria-disabled={create.isPending ? 'true' : undefined}>
        {create.isPending ? t('settings.saving') : t('webhooks.createButton')}
      </button>
    </form>
  );
}

function HookTable({
  projectId,
  project,
  items,
  onMessage,
  onSecret,
}: {
  projectId: number;
  project: string;
  items: Webhook[];
  onMessage: (text: string) => void;
  onSecret: (shown: Shown) => void;
}) {
  const { t, i18n } = useTranslation();
  const focusHeading = useFocusHeading();
  const refresh = useRefreshSessionOn403();
  const update = useUpdateWebhook(projectId);
  const rotate = useRotateWebhookSecret(projectId);
  const remove = useDeleteWebhook(projectId);
  const rotating = useConfirm<Webhook>();
  const deleting = useConfirm<Webhook>();
  const [editing, setEditing] = useState<Webhook | null>(null);
  const base = `/${encodePath(project)}/-/webhooks`;

  const toggle = (hook: Webhook) => {
    onMessage('');
    update.mutate(
      { hook: hook.id, body: { enabled: !hook.enabled } },
      {
        onSuccess: () => {
          onMessage(hook.enabled ? t('webhooks.turnedOff') : t('webhooks.turnedOn'));
        },
        onError: refresh,
      },
    );
  };

  return (
    <>
      {update.isError && <Denied error={update.error} />}
      {items.length === 0 ? (
        <p>{t('webhooks.none')}</p>
      ) : (
        <div className={styles.scroll} role="region" aria-label={t('webhooks.listTitle')} tabIndex={0}>
          <table className={styles.table}>
            <thead>
              <tr>
                <th scope="col">{t('webhooks.origin')}</th>
                <th scope="col">{t('webhooks.eventTypes')}</th>
                <th scope="col">{t('webhooks.state')}</th>
                <th scope="col">{t('webhooks.lastDelivery')}</th>
                <th scope="col">{t('common.actions')}</th>
              </tr>
            </thead>
            <tbody>
              {items.map((hook) => (
                <tr key={hook.id}>
                  <th scope="row">
                    <code>{hook.origin}</code>
                  </th>
                  <td>{hook.event_types.join(', ')}</td>
                  <td>
                    <EnabledChip enabled={hook.enabled} />
                    <StopReason hook={hook} />
                  </td>
                  <td>
                    {hook.last_delivery_at === null
                      ? t('webhooks.never')
                      : formatDateTime(hook.last_delivery_at, i18n.language)}
                  </td>
                  <td>
                    <div className={styles.inline}>
                      <Link
                        className={styles.touch}
                        to={`${base}/${hook.id}/deliveries`}
                        aria-label={t('webhooks.deliveriesOf', { origin: hook.origin })}
                      >
                        {t('webhooks.deliveries')}
                      </Link>
                      <button
                        type="button"
                        className={`${controls.button} ${styles.touch}`}
                        aria-label={t('webhooks.editOf', { origin: hook.origin })}
                        onClick={() => {
                          setEditing(hook);
                        }}
                      >
                        {t('webhooks.edit')}
                      </button>
                      <button
                        type="button"
                        className={`${controls.button} ${styles.touch}`}
                        aria-label={
                          hook.enabled
                            ? t('webhooks.disableOf', { origin: hook.origin })
                            : t('webhooks.enableOf', { origin: hook.origin })
                        }
                        aria-disabled={update.isPending ? 'true' : undefined}
                        onClick={() => {
                          if (!update.isPending) toggle(hook);
                        }}
                      >
                        {hook.enabled ? t('webhooks.disable') : t('webhooks.enable')}
                      </button>
                      <button
                        type="button"
                        className={`${controls.button} ${styles.touch}`}
                        aria-label={t('webhooks.rotateOf', { origin: hook.origin })}
                        onClick={() => {
                          rotate.reset();
                          rotating.ask(hook);
                        }}
                      >
                        {t('webhooks.rotate')}
                      </button>
                      <button
                        type="button"
                        className={`${styles.dangerQuiet} ${styles.touch}`}
                        aria-label={t('webhooks.deleteOf', { origin: hook.origin })}
                        onClick={() => {
                          remove.reset();
                          deleting.ask(hook);
                        }}
                      >
                        {t('webhooks.delete')}
                      </button>
                    </div>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
      <EditDialog
        projectId={projectId}
        hook={editing}
        onClose={() => {
          setEditing(null);
        }}
        onStart={() => {
          onMessage('');
        }}
        onSaved={(text) => {
          setEditing(null);
          onMessage(text);
        }}
      />
      <ConfirmDialog
        open={rotating.target !== null}
        title={t('webhooks.rotateTitle')}
        message={<p>{t('webhooks.rotateBody', { origin: rotating.target?.origin ?? '' })}</p>}
        confirmLabel={t('webhooks.rotate')}
        busy={rotate.isPending}
        error={rotate.isError ? rotate.error : null}
        onConfirm={() => {
          const hook = rotating.target;
          if (hook === null) return;
          rotate.mutate(hook.id, {
            onSuccess: (done) => {
              rotating.close();
              onMessage('');
              onSecret({
                origin: hook.origin,
                secret: done.secret,
                previousUntil: done.previous_secret_valid_until,
                announce: t('webhooks.rotatedStatus'),
              });
              rotate.reset();
            },
            onError: refresh,
          });
        }}
        onCancel={rotating.close}
      />
      <ConfirmDialog
        open={deleting.target !== null}
        title={t('webhooks.deleteTitle')}
        message={<p>{t('webhooks.deleteBody', { origin: deleting.target?.origin ?? '' })}</p>}
        confirmLabel={t('webhooks.deleteConfirm')}
        busy={remove.isPending}
        error={remove.isError ? remove.error : null}
        onConfirm={() => {
          const hook = deleting.target;
          if (hook === null) return;
          onMessage('');
          remove.mutate(hook.id, {
            onSuccess: () => {
              deleting.close();
              onMessage(t('webhooks.deleted'));
              focusHeading('webhooks-title');
            },
            onError: refresh,
          });
        }}
        onCancel={deleting.close}
      />
    </>
  );
}

/** Edit the URL (write-only) and the event types. Only what changed is sent. */
function EditDialog({
  projectId,
  hook,
  onClose,
  onStart,
  onSaved,
}: {
  projectId: number;
  hook: Webhook | null;
  onClose: () => void;
  onStart: () => void;
  onSaved: (text: string) => void;
}) {
  const { t } = useTranslation();
  const update = useUpdateWebhook(projectId);
  return (
    <Modal open={hook !== null} onClose={onClose} title={t('webhooks.editTitle')} busy={update.isPending}>
      {hook !== null && <EditForm projectId={projectId} hook={hook} update={update} onStart={onStart} onSaved={onSaved} onCancel={onClose} />}
    </Modal>
  );
}

function EditForm({
  projectId,
  hook,
  update,
  onStart,
  onSaved,
  onCancel,
}: {
  projectId: number;
  hook: Webhook;
  update: ReturnType<typeof useUpdateWebhook>;
  /** Called when a save begins: the last result is cleared, so the same
   * result twice in a row is announced again. */
  onStart: () => void;
  onSaved: (text: string) => void;
  onCancel: () => void;
}) {
  const { t } = useTranslation();
  const refresh = useRefreshSessionOn403();
  const [url, setUrl] = useState('');
  const [types, setTypes] = useState<string[]>(hook.event_types);
  const [attempt, setAttempt] = useState(0);
  const typesError = types.length === 0 ? t('webhooks.eventTypesRequired') : null;
  const [nothing, setNothing] = useState(false);
  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    if (update.isPending) return;
    onStart();
    setAttempt((x) => x + 1);
    if (typesError !== null) return;
    const next = types;
    const body: { url?: string; event_types?: string[] } = {};
    if (url.trim() !== '') body.url = url.trim();
    if (!sameSet(next, hook.event_types)) body.event_types = next;
    if (Object.keys(body).length === 0) {
      setNothing(true);
      return;
    }
    setNothing(false);
    update.mutate(
      { hook: hook.id, body },
      {
        onSuccess: (done) => {
          onSaved(done.outcome === 'unchanged' ? t('webhooks.unchanged') : t('webhooks.saved'));
        },
        onError: refresh,
      },
    );
  };
  return (
    <form onSubmit={submit} noValidate>
      <p className={styles.hint}>{t('webhooks.currentOrigin', { origin: hook.origin })}</p>
      <TextField
        label={t('webhooks.newUrl')}
        value={url}
        onChange={setUrl}
        help={t('webhooks.urlHelp')}
        initialFocus
      />
      <EventTypePicker
        projectId={projectId}
        value={types}
        onChange={setTypes}
        error={attempt > 0 ? typesError : null}
      />
      <p role="status" className={styles.muted}>
        {nothing ? t('webhooks.nothingChanged') : ''}
      </p>
      {update.isError && <Denied error={update.error} />}
      <div className={styles.row}>
        <button
          type="button"
          className={controls.button}
          aria-disabled={update.isPending ? 'true' : undefined}
          onClick={() => {
            if (!update.isPending) onCancel();
          }}
        >
          {t('common.cancel')}
        </button>
        <button type="submit" className={controls.primary} aria-disabled={update.isPending ? 'true' : undefined}>
          {update.isPending ? t('settings.saving') : t('webhooks.save')}
        </button>
      </div>
    </form>
  );
}
