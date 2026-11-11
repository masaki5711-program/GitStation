// A group's section: its name (the group page), members, settings for its
// Owner (GroupUpdate; or an administrator), and what is directly inside it.
//
// The list API has no parent filter, so "inside" comes from the whole
// visible lists the group page reads anyway (same queries, same keys). The
// sidebar does not ask for them itself on the group's other screens: there it
// shows what is already known, if anything. It lists at most CHILD_CAP of
// each kind; "see everything" leads to the group page whenever there is more,
// the lists were cut short (truncated), or they are not known here.
import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';

import { call } from '../../api/client';
import { useApi } from '../../api/context';
import { groupsQuery, parentPath, projectsQuery } from '../../api/lists';
import { mayCreateSubgroup } from '../../features/groupTreeModel';
import { encodePath } from '../../features/repoUrls';
import { useSession } from '../../session/session';
import { InlineError } from '../InlineError';
import styles from '../Sidebar.module.css';
import type { SidebarScope } from '../sidebarScope';
import { ItemList, NavItem, SectionHead, titleOf, type Item, type SectionProps } from './parts';

export const CHILD_CAP = 10;

export function GroupSection({
  scope,
  view,
  collapsed,
  onNavigate,
}: SectionProps & { scope: SidebarScope; view: string[] }) {
  const { t } = useTranslation();
  const api = useApi();
  const isAdmin = useSession().user.is_admin;
  const base = `/${encodePath(scope.path)}`;
  const word = view[0] ?? null;
  const onTop = word === null;
  const group = useQuery({
    queryKey: ['group', scope.id],
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/groups/{id}', { signal, params: { path: { id: scope.id } } })),
  });
  // Only on the group page, and only once the group itself could be read.
  const listChildren = onTop && group.isSuccess;
  const groups = useQuery({ ...groupsQuery(api), enabled: listChildren });
  const projects = useQuery({ ...projectsQuery(api), enabled: listChildren });

  const canManage = group.isSuccess && mayCreateSubgroup(group.data.role, isAdmin);
  const items: Item[] = [
    { to: `${base}/-/members`, icon: 'members', text: t('members.title'), current: word === 'members' },
    ...(canManage
      ? [{ to: `${base}/-/settings`, icon: 'settings' as const, text: t('settings.title'), current: word === 'settings' }]
      : []),
  ];
  const title = titleOf(group, (g) => ({ to: base, icon: 'groups', text: g.display_name, current: onTop }));

  const child = (path: string, name: string, icon: 'groups' | 'projects'): Item => ({
    to: `/${encodePath(path)}`,
    icon,
    text: name,
    current: false,
  });
  const subgroups = (groups.data?.items ?? [])
    .filter((g) => parentPath(g.path) === scope.path)
    .map((g) => child(g.path, g.display_name, 'groups'));
  const inside = (projects.data?.items ?? [])
    .filter((p) => parentPath(p.path) === scope.path)
    .map((p) => child(p.path, p.display_name, 'projects'));
  const failed = groups.isError ? groups : projects.isError ? projects : null;
  const known = groups.data !== undefined && projects.data !== undefined;
  const more =
    groups.data === undefined ||
    projects.data === undefined ||
    groups.data.truncated ||
    projects.data.truncated ||
    subgroups.length > CHILD_CAP ||
    inside.length > CHILD_CAP;
  const showAll: Item = { to: base, icon: 'more', text: t('nav.showAll'), current: false };

  return (
    <>
      <SectionHead title={title} collapsed={collapsed} onNavigate={onNavigate} />
      <ItemList items={items} collapsed={collapsed} onNavigate={onNavigate} />
      {failed !== null && (
        <InlineError
          error={failed.error}
          hidden={collapsed}
          onRetry={() => {
            void failed.refetch();
          }}
        />
      )}
      {subgroups.length > 0 && (
        <>
          <h3 className={collapsed ? styles.srOnly : styles.heading}>{t('namespace.subgroups')}</h3>
          <ItemList items={subgroups.slice(0, CHILD_CAP)} collapsed={collapsed} onNavigate={onNavigate} />
        </>
      )}
      {inside.length > 0 && (
        <>
          <h3 className={collapsed ? styles.srOnly : styles.heading}>{t('namespace.projects')}</h3>
          <ItemList items={inside.slice(0, CHILD_CAP)} collapsed={collapsed} onNavigate={onNavigate} />
        </>
      )}
      {group.isSuccess && failed === null && more && !(onTop && !known) && (
        <NavItem item={showAll} collapsed={collapsed} onNavigate={onNavigate} className={styles.back} />
      )}
    </>
  );
}
