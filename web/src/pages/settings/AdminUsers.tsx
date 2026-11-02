// Administration: the user list (filter by state), disable and end sessions.
// Disabling yourself, or the last administrator, is refused by the server
// (FG-AUTH-0009); the reason is shown. There is no "force" here.
import { useInfiniteQuery, useMutation, useQueryClient } from '@tanstack/react-query';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';

import { call } from '../../api/client';
import { useApi } from '../../api/context';
import type { components } from '../../api/schema.gen';
import { SelectField } from '../../components/Field';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { formatDate } from '../../features/labels';
import { ConfirmDialog, Denied, Section, useConfirm, useFocusHeading, useRefreshSessionOn403 } from './common';
import styles from './Settings.module.css';

type AdminUser = components['schemas']['AdminUser'];
type Action = { user: AdminUser; what: 'disable' | 'sessions' };

export function AdminUsers() {
  const { t, i18n } = useTranslation();
  const focusHeading = useFocusHeading();
  const api = useApi();
  const queryClient = useQueryClient();
  const refresh = useRefreshSessionOn403();
  const [state, setState] = useState('');
  const [notice, setNotice] = useState<string | null>(null);
  const confirm = useConfirm<Action>();
  const key = ['admin-users', state];
  const users = useInfiniteQuery({
    queryKey: key,
    initialPageParam: undefined as string | undefined,
    queryFn: ({ signal, pageParam }) =>
      call(() =>
        api.GET('/api/v1/admin/users', {
          signal,
          params: {
            query: {
              limit: 50,
              ...(pageParam === undefined ? {} : { cursor: pageParam }),
              ...(state === '' ? {} : { state }),
            },
          },
        }),
      ),
    getNextPageParam: (last) => (last.has_more ? (last.next_cursor ?? undefined) : undefined),
  });
  const act = useMutation({
    mutationFn: (a: Action) =>
      a.what === 'disable'
        ? call(() =>
            api.POST('/api/v1/admin/users/{id}/disable', { params: { path: { id: a.user.id } } }),
          ).then(() => t('admin.disabledDone', { name: a.user.display_name }))
        : call(() =>
            api.DELETE('/api/v1/admin/users/{id}/sessions', { params: { path: { id: a.user.id } } }),
          ).then((r) =>
            t('admin.sessionsDone', { name: a.user.display_name, sessions: r.revoked_sessions }),
          ),
    onSuccess: async (message) => {
      setNotice(message);
      confirm.close();
      await queryClient.invalidateQueries({ queryKey: ['admin-users'] });
      focusHeading('admin-title');
    },
    onError: refresh,
  });
  const rows = users.data?.pages.flatMap((p) => p.items) ?? [];
  const target = confirm.target;
  return (
    <Section id="admin-title" title={t('admin.title')}>
      <p className={styles.hint}>{t('admin.explain')}</p>
      <div className={styles.narrow}>
        <SelectField
          label={t('admin.filter')}
          value={state}
          onChange={(v) => {
            setState(v);
            setNotice(null);
          }}
        >
          <option value="">{t('admin.all')}</option>
          <option value="active">{t('admin.active')}</option>
          <option value="disabled">{t('admin.disabled')}</option>
        </SelectField>
      </div>
      <p role="status" className={styles.muted}>
        {notice ?? ''}
      </p>
      {users.isPending && <Loading />}
      {users.isError && (
        <QueryError
          error={users.error}
          onRetry={() => {
            void users.refetch();
          }}
        />
      )}
      {users.isSuccess &&
        (rows.length === 0 ? (
          <p>{t('admin.none')}</p>
        ) : (
          <div className={styles.scroll} role="region" aria-label={t('admin.title')} tabIndex={0}>
            <table className={styles.table}>
              <thead>
                <tr>
                  <th scope="col">{t('admin.user')}</th>
                  <th scope="col">{t('admin.kind')}</th>
                  <th scope="col">{t('tokens.state')}</th>
                  <th scope="col">{t('admin.created')}</th>
                  <th scope="col">{t('common.actions')}</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((u) => (
                  <tr key={u.id}>
                    <th scope="row">
                      {u.display_name} <code>{u.username}</code>
                      {u.is_admin && <span className={controls.badge}>{t('admin.isAdmin')}</span>}
                    </th>
                    <td>{u.kind === 'human' ? t('admin.kindHuman') : u.kind === 'bot' ? t('admin.kindBot') : u.kind === 'service_account' ? t('admin.kindService') : u.kind}</td>
                    <td>{u.state === 'active' ? t('admin.active') : t('admin.disabled')}</td>
                    <td>{formatDate(u.created_at, i18n.language)}</td>
                    <td>
                      <span className={styles.inline}>
                        {u.state === 'active' && (
                          <button
                            type="button"
                            className={styles.dangerQuiet}
                            aria-label={t('admin.disableOf', { name: u.display_name })}
                            onClick={() => {
                              act.reset();
                              confirm.ask({ user: u, what: 'disable' });
                            }}
                          >
                            {t('admin.disable')}
                          </button>
                        )}
                        <button
                          type="button"
                          className={styles.dangerQuiet}
                          aria-label={t('admin.endSessionsOf', { name: u.display_name })}
                          onClick={() => {
                            act.reset();
                            confirm.ask({ user: u, what: 'sessions' });
                          }}
                        >
                          {t('admin.endSessions')}
                        </button>
                      </span>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        ))}
      {users.hasNextPage && (
        <p>
          <button
            type="button"
            className={controls.button}
            aria-disabled={users.isFetchingNextPage ? 'true' : undefined}
            onClick={() => {
              if (!users.isFetchingNextPage) void users.fetchNextPage();
            }}
          >
            {t('admin.more')}
          </button>
        </p>
      )}
      {users.isFetchNextPageError && <Denied error={users.error} />}
      <ConfirmDialog
        open={target !== null}
        title={target?.what === 'disable' ? t('admin.disableTitle') : t('admin.endSessionsTitle')}
        message={
          <p>
            {target?.what === 'disable'
              ? t('admin.disableBody', { name: target.user.display_name })
              : t('admin.endSessionsBody', { name: target?.user.display_name ?? '' })}
          </p>
        }
        confirmLabel={target?.what === 'disable' ? t('admin.disable') : t('admin.endSessions')}
        busy={act.isPending}
        error={act.isError ? act.error : null}
        onConfirm={() => {
          if (target !== null) act.mutate(target);
        }}
        onCancel={confirm.close}
      />
    </Section>
  );
}
