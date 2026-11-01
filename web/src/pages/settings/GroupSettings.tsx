// `/{group}/-/settings` and `/{group}/-/members`: the group's name and
// description, move / rename, and its members. (A group has no visibility of
// its own and cannot be deleted through the API yet.)
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { call } from '../../api/client';
import { useApi } from '../../api/context';
import { Breadcrumb } from '../../components/Breadcrumb';
import { Loading } from '../../components/Loading';
import { MovedNotice } from '../../components/MovedNotice';
import { QueryError } from '../../components/QueryError';
import { encodePath } from '../../features/repoUrls';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { Section } from './common';
import { GeneralForm, MoveForm } from './Forms';
import { MembersScreen } from './Members';
import styles from './Settings.module.css';

export function GroupViews({ groupId, word }: { groupId: number; word: 'settings' | 'members' }) {
  const { t } = useTranslation();
  const api = useApi();
  const queryClient = useQueryClient();
  const group = useQuery({
    queryKey: ['group', groupId],
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/groups/{id}', { signal, params: { path: { id: groupId } } })),
  });
  useDocumentTitle(word === 'settings' ? t('settings.title') : t('members.title'));
  if (group.isPending) return <Loading />;
  if (group.isError) {
    return (
      <QueryError
        error={group.error}
        onRetry={() => {
          void group.refetch();
        }}
      />
    );
  }
  const g = group.data;
  const base = `/${encodePath(g.path)}`;
  return (
    <>
      <Breadcrumb path={g.path} />
      <MovedNotice />
      <nav aria-label={t('repo.navLabel')} className={styles.tabs}>
        <Link to={base} className={styles.tab}>
          {g.display_name}
        </Link>
        <Link
          to={`${base}/-/settings`}
          className={styles.tab}
          {...(word === 'settings' ? { 'aria-current': 'page' as const } : {})}
        >
          {t('settings.title')}
        </Link>
        <Link
          to={`${base}/-/members`}
          className={styles.tab}
          {...(word === 'members' ? { 'aria-current': 'page' as const } : {})}
        >
          {t('members.title')}
        </Link>
      </nav>
      {word === 'members' ? (
        <MembersScreen scope={{ kind: 'group', id: groupId }} />
      ) : (
        <>
          <h1 className={styles.title}>{t('settings.title')}</h1>
          <Section id="general-title" title={t('settings.general')}>
            <GeneralForm
              displayName={g.display_name}
              description={g.description}
              save={async (body) => {
                const next = await call(() =>
                  api.PATCH('/api/v1/groups/{id}', { params: { path: { id: groupId } }, body }),
                );
                queryClient.setQueryData(['group', groupId], next);
                void queryClient.invalidateQueries({ queryKey: ['groups'] });
                return next;
              }}
            />
          </Section>
          <Section id="move-title" title={t('settings.move.title')}>
            <MoveForm kind="group" id={groupId} path={g.path} />
          </Section>
        </>
      )}
    </>
  );
}
