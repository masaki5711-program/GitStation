// Administration: integrations (M3 I1a, plan 02 §2). Administrators only;
// the server decides again (a change needs the administrator's web session).
// The list, creating one for a service account, one integration with the
// projects bound to it, and disabling (asked first: kanflow stops receiving
// events and can no longer act for anyone) or enabling it again (taken back
// by disabling, so not asked). Projects are bound from kanflow, not here.
//
// The detail is `?tab=integrations&integration={id}`.
import { useQuery } from '@tanstack/react-query';
import { useEffect, useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useLocation, useSearchParams } from 'react-router';

import { call } from '../../api/client';
import { useApi } from '../../api/context';
import {
  useCreateIntegration,
  useIntegration,
  useIntegrationBindings,
  useIntegrations,
  useSetIntegrationEnabled,
  type Integration,
} from '../../api/integrations';
import { SelectField, TextField } from '../../components/Field';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { ErrorSummary } from '../../features/CreateDialogs';
import { formatDateTime } from '../../features/labels';
import { ConfirmDialog, Denied, Section, useConfirm, useFocusHeading, useRefreshSessionOn403 } from './common';
import styles from './Settings.module.css';

/** At most this many event type entries (the API's rule). */
const MAX_EVENT_TYPES = 64;

export function Integrations({ base }: { base: string }) {
  const [params] = useSearchParams();
  const raw = params.get('integration');
  const id = raw !== null && /^[1-9][0-9]{0,15}$/.test(raw) ? Number(raw) : null;
  return id === null ? <IntegrationList base={base} /> : <IntegrationDetail key={id} id={id} base={base} />;
}

const back = (base: string) => `${base}?tab=integrations`;

/** `kanflow #3`: the one name an integration has. */
function nameOf(t: ReturnType<typeof useTranslation>['t'], i: Pick<Integration, 'id' | 'kind'>): string {
  return t('integrations.name', { kind: i.kind, id: i.id });
}

/** Disable (asked first) and enable (not asked), shared by the list and the detail. */
function useToggle(onDone: (text: string) => void) {
  const { t } = useTranslation();
  const refresh = useRefreshSessionOn403();
  const confirm = useConfirm<Integration>();
  const mutation = useSetIntegrationEnabled();
  const run = (i: Integration, enabled: boolean) => {
    mutation.reset();
    mutation.mutate(
      { id: i.id, enabled },
      {
        onSuccess: () => {
          confirm.close();
          onDone(
            enabled
              ? t('integrations.enabledDone', { name: nameOf(t, i) })
              : t('integrations.disabledDone', { name: nameOf(t, i) }),
          );
        },
        onError: refresh,
      },
    );
  };
  return { confirm, mutation, run };
}

function ToggleButton({ i, toggle }: { i: Integration; toggle: ReturnType<typeof useToggle> }) {
  const { t } = useTranslation();
  if (i.enabled) {
    return (
      <button
        type="button"
        className={`${styles.dangerQuiet} ${styles.touch}`}
        aria-label={t('integrations.disableOf', { name: nameOf(t, i) })}
        onClick={() => {
          toggle.mutation.reset();
          toggle.confirm.ask(i);
        }}
      >
        {t('integrations.disable')}
      </button>
    );
  }
  return (
    <button
      type="button"
      className={`${controls.button} ${styles.touch}`}
      aria-label={t('integrations.enableOf', { name: nameOf(t, i) })}
      aria-disabled={toggle.mutation.isPending ? 'true' : undefined}
      onClick={() => {
        if (!toggle.mutation.isPending) toggle.run(i, true);
      }}
    >
      {t('integrations.enable')}
    </button>
  );
}

function DisableConfirm({ toggle }: { toggle: ReturnType<typeof useToggle> }) {
  const { t } = useTranslation();
  const target = toggle.confirm.target;
  return (
    <ConfirmDialog
      open={target !== null}
      title={t('integrations.disableTitle')}
      message={<p>{t('integrations.disableBody', { name: target === null ? '' : nameOf(t, target) })}</p>}
      confirmLabel={t('integrations.disable')}
      busy={toggle.mutation.isPending}
      error={toggle.mutation.isError && target !== null ? toggle.mutation.error : null}
      onConfirm={() => {
        if (target !== null) toggle.run(target, false);
      }}
      onCancel={toggle.confirm.close}
    />
  );
}

function StateChip({ enabled }: { enabled: boolean }) {
  const { t } = useTranslation();
  return (
    <span className={`${styles.chip} ${enabled ? styles.chipActive : styles.chipRevoked}`}>
      {enabled ? t('integrations.enabled') : t('integrations.disabled')}
    </span>
  );
}

function IntegrationList({ base }: { base: string }) {
  const { t } = useTranslation();
  const focusHeading = useFocusHeading();
  const location = useLocation();
  const [notice, setNotice] = useState('');
  const toggle = useToggle((text) => {
    setNotice(text);
    focusHeading('integrations-title');
  });
  const list = useIntegrations();
  const cameBack = (location.state as { focus?: string } | null)?.focus === 'list';
  useEffect(() => {
    if (cameBack) focusHeading('integrations-title');
  }, [cameBack, focusHeading]);
  return (
    <>
      <Section id="integrations-title" title={t('integrations.title')}>
        <p className={styles.hint}>{t('integrations.explain')}</p>
        <p role="status" className={styles.muted}>
          {notice}
        </p>
        {/* A failed enable (the disable shows its failure in the dialog). */}
        {toggle.mutation.isError && toggle.confirm.target === null && <Denied error={toggle.mutation.error} />}
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
              <p>{t('integrations.none')}</p>
            ) : (
              <div className={styles.scroll} role="region" aria-label={t('integrations.title')} tabIndex={0}>
                <table className={styles.table}>
                  <thead>
                    <tr>
                      <th scope="col">{t('integrations.integration')}</th>
                      <th scope="col">{t('integrations.account')}</th>
                      <th scope="col">{t('integrations.state')}</th>
                      <th scope="col">{t('integrations.eventTypes')}</th>
                      <th scope="col">{t('integrations.bindingCount')}</th>
                      <th scope="col">{t('common.actions')}</th>
                    </tr>
                  </thead>
                  <tbody>
                    {list.data.items.map((i) => (
                      <tr key={i.id} className={i.enabled ? undefined : styles.rowMuted}>
                        <th scope="row">{nameOf(t, i)}</th>
                        <td>
                          <code>{i.service_account.username}</code>
                        </td>
                        <td>
                          <StateChip enabled={i.enabled} />
                        </td>
                        <td>
                          <EventTypes types={i.event_types} />
                        </td>
                        <td>{i.bindings}</td>
                        <td>
                          <span className={styles.inline}>
                            <Link
                              className={`${controls.button} ${styles.touch}`}
                              to={`${back(base)}&integration=${String(i.id)}`}
                              aria-label={t('integrations.viewOf', { name: nameOf(t, i) })}
                            >
                              {t('integrations.view')}
                            </Link>
                            <ToggleButton i={i} toggle={toggle} />
                          </span>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </>
        )}
      </Section>
      <Section id="integrations-new-title" title={t('integrations.create')}>
        <CreateIntegration
          taken={new Set(list.data?.items.map((i) => i.service_account.id) ?? [])}
          onCreated={(i) => {
            setNotice(t('integrations.created', { name: nameOf(t, i) }));
          }}
        />
      </Section>
      <DisableConfirm toggle={toggle} />
    </>
  );
}

function EventTypes({ types }: { types: string[] }) {
  const { t } = useTranslation();
  if (types.length === 1 && types[0] === '*') return <>{t('integrations.allEvents')}</>;
  return (
    <span className={styles.inline}>
      {types.map((x) => (
        <code key={x}>{x}</code>
      ))}
    </span>
  );
}

/** `a, b c` → `['a', 'b', 'c']`; empty: the server's default (`*`). */
export function parseEventTypes(raw: string): string[] {
  return raw
    .split(/[\s,]+/)
    .map((x) => x.trim())
    .filter((x) => x !== '');
}

function CreateIntegration({ taken, onCreated }: { taken: Set<number>; onCreated: (i: Integration) => void }) {
  const { t } = useTranslation();
  const api = useApi();
  const refresh = useRefreshSessionOn403();
  const create = useCreateIntegration();
  const [account, setAccount] = useState('');
  const [events, setEvents] = useState('');
  const [attempt, setAttempt] = useState(0);
  // Active service accounts that act for no integration yet (one each).
  const accounts = useQuery({
    queryKey: ['service-accounts'],
    queryFn: ({ signal }) => call(() => api.GET('/api/v1/admin/service-accounts', { signal })),
  });
  const choices = (accounts.data?.items ?? []).filter((a) => a.state === 'active' && !taken.has(a.id));
  const types = parseEventTypes(events);
  const accountError = account === '' ? t('integrations.accountRequired') : null;
  const eventsError = types.length > MAX_EVENT_TYPES ? t('integrations.eventTypesTooMany', { max: MAX_EVENT_TYPES }) : null;
  const errors = [accountError, eventsError].filter((e): e is string => e !== null);

  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    if (create.isPending) return;
    setAttempt((x) => x + 1);
    if (errors.length > 0) return;
    create.mutate(
      { kind: 'kanflow', service_account_id: Number(account), ...(types.length === 0 ? {} : { event_types: types }) },
      {
        onSuccess: (made) => {
          setAccount('');
          setEvents('');
          setAttempt(0);
          onCreated(made);
        },
        onError: refresh,
      },
    );
  };
  if (accounts.isPending) return <Loading />;
  if (accounts.isError) {
    return (
      <QueryError
        error={accounts.error}
        onRetry={() => {
          void accounts.refetch();
        }}
      />
    );
  }
  if (choices.length === 0) return <p className={styles.hint}>{t('integrations.noAccounts')}</p>;
  return (
    <form onSubmit={submit} noValidate className={styles.narrow}>
      <ErrorSummary errors={errors} attempt={attempt} />
      <SelectField
        label={t('integrations.account')}
        value={account}
        onChange={setAccount}
        error={attempt > 0 ? accountError : null}
      >
        <option value="">{t('integrations.chooseAccount')}</option>
        {choices.map((a) => (
          <option key={a.id} value={String(a.id)}>
            {t('integrations.accountOption', { name: a.display_name, username: a.username })}
          </option>
        ))}
      </SelectField>
      <TextField
        label={t('integrations.eventTypes')}
        value={events}
        onChange={setEvents}
        error={attempt > 0 ? eventsError : null}
        help={t('integrations.eventTypesHelp')}
      />
      {create.isError && <Denied error={create.error} />}
      <button
        type="submit"
        className={`${controls.primary} ${styles.touch}`}
        aria-disabled={create.isPending ? 'true' : undefined}
      >
        {create.isPending ? t('settings.saving') : t('integrations.createButton')}
      </button>
    </form>
  );
}

function IntegrationDetail({ id, base }: { id: number; base: string }) {
  const { t, i18n } = useTranslation();
  const focusHeading = useFocusHeading();
  const [notice, setNotice] = useState('');
  const toggle = useToggle((text) => {
    setNotice(text);
    focusHeading('integration-detail-title');
  });
  const one = useIntegration(id);
  useEffect(() => {
    focusHeading('integration-detail-title');
  }, [id, focusHeading]);
  const i = one.data;
  return (
    <>
      <p>
        <Link to={back(base)} state={{ focus: 'list' }} className={styles.touch}>
          {t('integrations.backToList')}
        </Link>
      </p>
      <p role="status" className={styles.srOnly}>
        {notice}
      </p>
      <Section id="integration-detail-title" title={i === undefined ? t('integrations.detailTitle') : nameOf(t, i)}>
        {one.isPending && <Loading />}
        {one.isError && (
          <QueryError
            error={one.error}
            onRetry={() => {
              void one.refetch();
            }}
          />
        )}
        {i !== undefined && (
          <>
            <dl className={styles.facts}>
              <dt>{t('integrations.account')}</dt>
              <dd>
                <code>{i.service_account.username}</code>
              </dd>
              <dt>{t('integrations.state')}</dt>
              <dd>
                <StateChip enabled={i.enabled} />
              </dd>
              <dt>{t('integrations.eventTypes')}</dt>
              <dd>
                <EventTypes types={i.event_types} />
              </dd>
              <dt>{t('integrations.createdAt')}</dt>
              <dd>{formatDateTime(i.created_at, i18n.language)}</dd>
              <dt>{t('integrations.updatedAt')}</dt>
              <dd>{formatDateTime(i.updated_at, i18n.language)}</dd>
            </dl>
            {!i.enabled && (
              <p className={styles.warning} role="note">
                {t('integrations.isDisabled', { name: nameOf(t, i) })}
              </p>
            )}
            {toggle.mutation.isError && toggle.confirm.target === null && <Denied error={toggle.mutation.error} />}
            <ToggleButton i={i} toggle={toggle} />
          </>
        )}
      </Section>
      <Section id="integration-bindings-title" title={t('integrations.bindings')}>
        <Bindings id={id} />
      </Section>
      <DisableConfirm toggle={toggle} />
    </>
  );
}

function Bindings({ id }: { id: number }) {
  const { t, i18n } = useTranslation();
  const bindings = useIntegrationBindings(id);
  if (bindings.isPending) return <Loading />;
  if (bindings.isError) {
    return (
      <QueryError
        error={bindings.error}
        onRetry={() => {
          void bindings.refetch();
        }}
      />
    );
  }
  if (bindings.data.items.length === 0) return <p>{t('integrations.noBindings')}</p>;
  return (
    <>
      <p className={styles.hint}>{t('integrations.bindingsExplain')}</p>
      <div className={styles.scroll} role="region" aria-label={t('integrations.bindings')} tabIndex={0}>
        <table className={styles.table}>
          <thead>
            <tr>
              <th scope="col">{t('integrations.project')}</th>
              <th scope="col">{t('integrations.kanflowProject')}</th>
              <th scope="col">{t('integrations.membership')}</th>
              <th scope="col">{t('integrations.boundAt')}</th>
            </tr>
          </thead>
          <tbody>
            {bindings.data.items.map((b) => (
              <tr key={`${String(b.project_id)}/${b.external_project_id}`}>
                <th scope="row">
                  <Link to={`/-/projects/${String(b.project_id)}`}>{t('integrations.projectN', { id: b.project_id })}</Link>
                </th>
                <td>
                  <code>{b.external_project_id}</code>
                </td>
                <td>{b.granted_membership ? t('integrations.grantedMembership') : t('integrations.ownMembership')}</td>
                <td>{formatDateTime(b.created_at, i18n.language)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </>
  );
}
