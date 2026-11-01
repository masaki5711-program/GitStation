// `/{username}/-/settings`: the signed-in person's account — tokens, sessions,
// projects waiting for deletion and (administrators) users. The tab is the
// `?tab=` query, so a link can point at one; no server change is needed (the
// shell is served for `/{namespace}/-/settings`).
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { Link, useSearchParams } from 'react-router';

import { call, callNoContent } from '../../api/client';
import { useApi } from '../../api/context';
import type { components } from '../../api/schema.gen';
import { fetchAllPages } from '../../api/pages';
import type { Project } from '../../api/types';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { formatDateTime } from '../../features/labels';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { SESSION_KEY, useSession } from '../../session/session';
import { AdminUsers } from './AdminUsers';
import { ConfirmDialog, Denied, Section, focusHeading, useConfirm, useRefreshSessionOn403 } from './common';
import styles from './Settings.module.css';
import { Tokens } from './Tokens';

type Tab = 'tokens' | 'sessions' | 'trash' | 'admin';

export function Account({ path }: { path: string }) {
  const { t } = useTranslation();
  const session = useSession();
  const [params] = useSearchParams();
  const wanted = params.get('tab');
  const tab: Tab =
    wanted === 'sessions' || wanted === 'trash' || (wanted === 'admin' && session.user.is_admin)
      ? wanted
      : 'tokens';
  useDocumentTitle(t('account.title'));
  const base = `/${path}/-/settings`;
  const tabs: { id: Tab; label: string }[] = [
    { id: 'tokens', label: t('tokens.title') },
    { id: 'sessions', label: t('sessions.title') },
    { id: 'trash', label: t('trash.title') },
    ...(session.user.is_admin ? [{ id: 'admin' as const, label: t('admin.title') }] : []),
  ];
  return (
    <>
      <h1 className={styles.title}>{t('account.title')}</h1>
      <nav aria-label={t('account.tabs')}>
        <ul className={styles.tabs}>
          {tabs.map((x) => (
            <li key={x.id}>
              <Link
                to={x.id === 'tokens' ? base : `${base}?tab=${x.id}`}
                className={styles.tab}
                {...(tab === x.id ? { 'aria-current': 'page' as const } : {})}
              >
                {x.label}
              </Link>
            </li>
          ))}
        </ul>
      </nav>
      {tab === 'tokens' && <Tokens />}
      {tab === 'sessions' && <Sessions />}
      {tab === 'trash' && <Trash />}
      {tab === 'admin' && <AdminUsers />}
    </>
  );
}

type SessionItem = components['schemas']['SessionItem'];

function Sessions() {
  const { t, i18n } = useTranslation();
  const api = useApi();
  const queryClient = useQueryClient();
  const refresh = useRefreshSessionOn403();
  const confirm = useConfirm<SessionItem>();
  const list = useQuery({
    queryKey: ['sessions'],
    queryFn: ({ signal }) => call(() => api.GET('/api/v1/user/sessions', { signal })),
  });
  const revoke = useMutation({
    mutationFn: (item: SessionItem) =>
      callNoContent(() =>
        api.DELETE('/api/v1/user/sessions/{public_id}', {
          // docs/api/openapi.json declares `{public_id}` as an integer, but it
          // is 16 hex characters (SessionItem.public_id): a defect of the
          // generated document (handoff MR11), worked around here.
          params: { path: { public_id: item.public_id as unknown as number } },
        }),
      ),
    onSuccess: async (_r, item) => {
      confirm.close();
      // Ending this very session signs the person out: read it again, the
      // gate then shows the sign-in.
      await queryClient.invalidateQueries({ queryKey: item.current ? SESSION_KEY : ['sessions'] });
      focusHeading('sessions-title');
    },
    onError: refresh,
  });
  return (
    <Section id="sessions-title" title={t('sessions.title')}>
      <p className={styles.hint}>{t('sessions.explain')}</p>
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
          <div className={styles.scroll} role="region" aria-label={t('sessions.title')} tabIndex={0}>
            <table className={styles.table}>
              <thead>
                <tr>
                  <th scope="col">{t('sessions.started')}</th>
                  <th scope="col">{t('sessions.lastSeen')}</th>
                  <th scope="col">{t('sessions.idleExpires')}</th>
                  <th scope="col">{t('common.actions')}</th>
                </tr>
              </thead>
              <tbody>
                {list.data.items.map((s) => (
                  <tr key={s.public_id}>
                    <th scope="row">
                      {formatDateTime(s.created_at, i18n.language)}{' '}
                      {s.current && <span className={controls.badge}>{t('sessions.current')}</span>}
                    </th>
                    <td>{formatDateTime(s.last_seen_at, i18n.language)}</td>
                    <td>{formatDateTime(s.idle_expires_at, i18n.language)}</td>
                    <td>
                      <button
                        type="button"
                        className={styles.danger}
                        aria-label={t('sessions.revokeOf', {
                          date: formatDateTime(s.created_at, i18n.language),
                        })}
                        onClick={() => {
                          revoke.reset();
                          confirm.ask(s);
                        }}
                      >
                        {t('sessions.revoke')}
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      )}
      <ConfirmDialog
        open={confirm.target !== null}
        title={t('sessions.revokeTitle')}
        message={
          <p>
            {confirm.target?.current === true ? t('sessions.revokeCurrent') : t('sessions.revokeBody')}
          </p>
        }
        confirmLabel={t('sessions.revoke')}
        busy={revoke.isPending}
        error={revoke.isError ? revoke.error : null}
        onConfirm={() => {
          if (confirm.target !== null) revoke.mutate(confirm.target);
        }}
        onCancel={confirm.close}
      />
    </Section>
  );
}

/** Projects waiting for deletion that the person may restore. The purge date
 * is only in the answer to the delete request, so it is shown for the
 * projects deleted in this tab session; for the others the list says they are
 * waiting and that the date is not known here. */
function Trash() {
  const { t, i18n } = useTranslation();
  const api = useApi();
  const queryClient = useQueryClient();
  const refresh = useRefreshSessionOn403();
  const list = useQuery({
    queryKey: ['projects', 'pending_deletion'],
    queryFn: () =>
      fetchAllPages<Project>((cursor) =>
        call(() =>
          api.GET('/api/v1/projects', {
            params: {
              query: { limit: 100, state: 'pending_deletion', ...(cursor === undefined ? {} : { cursor }) },
            },
          }),
        ),
      ),
  });
  const restore = useMutation({
    mutationFn: (id: number) =>
      call(() => api.POST('/api/v1/projects/{id}/restore', { params: { path: { id } } })),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ['projects'] });
      focusHeading('trash-title');
    },
    onError: refresh,
  });
  return (
    <Section id="trash-title" title={t('trash.title')}>
      <p className={styles.hint}>{t('trash.explain')}</p>
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
        (list.data.items.length === 0 ? (
          <p>{t('trash.none')}</p>
        ) : (
          <ul className={styles.results}>
            {list.data.items.map((p) => {
              const deletion = queryClient.getQueryData<components['schemas']['DeletionBody']>([
                'deletion',
                p.id,
              ]);
              return (
                <li key={p.id}>
                  <span>
                    {p.display_name} <code>{p.path}</code>
                  </span>
                  <span className={styles.muted}>
                    {deletion === undefined
                      ? t('trash.dateUnknown')
                      : t('trash.purgeOn', { date: formatDateTime(deletion.purge_after, i18n.language) })}
                  </span>
                  <button
                    type="button"
                    className={controls.button}
                    aria-label={t('trash.restoreOf', { name: p.display_name })}
                    aria-disabled={restore.isPending ? 'true' : undefined}
                    onClick={() => {
                      if (!restore.isPending) restore.mutate(p.id);
                    }}
                  >
                    {t('trash.restore')}
                  </button>
                </li>
              );
            })}
          </ul>
        ))}
      {restore.isError && <Denied error={restore.error} />}
    </Section>
  );
}
