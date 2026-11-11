// A project's section: its name (the top page) and the project's screens
// that exist (no placeholders, plan 10). Webhooks and settings only for a
// role that may change them (ProjectUpdate: Maintainer or above, or an
// administrator; the server decides again) — and only once the role is
// known: while the project loads, or when it failed, they are not offered.
import { useQuery } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';

import { call } from '../../api/client';
import { useApi } from '../../api/context';
import { encodePath } from '../../features/repoUrls';
import { useSession } from '../../session/session';
import type { SidebarScope } from '../sidebarScope';
import type { IconKey } from './icons';
import { ItemList, SectionHead, titleOf, type Item, type SectionProps } from './parts';

export function ProjectSection({
  scope,
  view,
  collapsed,
  onNavigate,
}: SectionProps & { scope: SidebarScope; view: string[] }) {
  const { t } = useTranslation();
  const api = useApi();
  const isAdmin = useSession().user.is_admin;
  // The same query (and key) as the project's pages: no second request.
  const project = useQuery({
    queryKey: ['project', scope.id],
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/projects/{id}', { signal, params: { path: { id: scope.id } } })),
  });
  const base = `/${encodePath(scope.path)}`;
  const word = view[0] ?? null;
  const role = project.data?.role ?? null;
  const canManage = project.isSuccess && (isAdmin || role === 'maintainer' || role === 'owner');
  const item = (words: string[], path: string, icon: IconKey, text: string): Item => ({
    to: `${base}/-/${path}`,
    icon,
    text,
    current: word !== null && words.includes(word),
  });
  const items = [
    item(['tree', 'blob', 'blame'], 'tree', 'files', t('repo.files')),
    item(['commits', 'commit'], 'commits', 'commits', t('repo.commits')),
    item(['branches'], 'branches', 'branches', t('repo.branches')),
    item(['tags'], 'tags', 'tags', t('repo.tags')),
    item(['compare'], 'compare', 'compare', t('repo.compare')),
    item(['pulls', 'pull'], 'pulls', 'pulls', t('repo.pulls')),
    item(['documents'], 'documents', 'documents', t('docs.title')),
    item(['pipelines'], 'pipelines', 'pipelines', t('pipelines.title')),
    item(['members'], 'members', 'members', t('members.title')),
    ...(canManage
      ? [
          item(['webhooks'], 'webhooks', 'webhooks', t('webhooks.title')),
          item(['settings'], 'settings', 'settings', t('settings.title')),
        ]
      : []),
  ];
  const title = titleOf(project, (p) => ({
    to: base,
    icon: 'projects',
    text: p.display_name,
    current: word === null,
  }));
  return (
    <>
      <SectionHead title={title} collapsed={collapsed} onNavigate={onNavigate} />
      <ItemList items={items} collapsed={collapsed} onNavigate={onNavigate} />
    </>
  );
}
