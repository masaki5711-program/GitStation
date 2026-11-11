// The sidebar (plan 15 §3.1). It follows where I am:
//
// - on a project's pages: the project (its top page) and the project's
//   screens that exist (no placeholders, plan 10); webhooks and settings only
//   for a role that may change them (the server decides again);
// - on a group's page: the group, its members and settings, and what is
//   directly inside it (subgroups, projects);
// - elsewhere: "My work" (Home, Projects, Groups). A project or group section
//   starts with a link back to it.
//
// The scope comes from the namespace resolver (sidebarScope.tsx); the names
// and roles from the same queries the pages use (one request each).
//
// - Wide screens: a column; it collapses to icons, the names stay as
//   visually hidden text. The choice is kept in localStorage (not a secret;
//   storage may be missing or throw, then the column is simply expanded).
// - Narrow screens (< 768px): hidden until the header's menu button opens it
//   (Layout owns that state, the Esc key and the focus return).
import { useQuery } from '@tanstack/react-query';
import type { ReactNode } from 'react';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useLocation } from 'react-router';

import { call } from '../api/client';
import { useApi } from '../api/context';
import { groupsQuery, parentPath, projectsQuery } from '../api/lists';
import { mayCreateSubgroup } from '../features/groupTreeModel';
import { encodePath } from '../features/repoUrls';
import { useSession } from '../session/session';
import styles from './Sidebar.module.css';
import { useSidebarScope, type SidebarScope } from './sidebarScope';

const COLLAPSED_KEY = 'gitstation.sidebar.collapsed';

export function readCollapsed(): boolean {
  try {
    return window.localStorage.getItem(COLLAPSED_KEY) === '1';
  } catch {
    return false;
  }
}

function storeCollapsed(collapsed: boolean): void {
  try {
    window.localStorage.setItem(COLLAPSED_KEY, collapsed ? '1' : '0');
  } catch {
    // Not remembered; the column still collapses for this page view.
  }
}

function Icon({ children }: { children: ReactNode }) {
  return (
    <svg
      className={styles.icon}
      viewBox="0 0 24 24"
      width="20"
      height="20"
      aria-hidden="true"
      focusable="false"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      {children}
    </svg>
  );
}

const ICONS = {
  home: (
    <Icon>
      <path d="M3 11 12 4l9 7" />
      <path d="M5 10v10h5v-6h4v6h5V10" />
    </Icon>
  ),
  projects: (
    <Icon>
      <path d="M3 6a1 1 0 0 1 1-1h5l2 2h9a1 1 0 0 1 1 1v10a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1Z" />
    </Icon>
  ),
  groups: (
    <Icon>
      <circle cx="9" cy="8" r="3" />
      <path d="M3 20a6 6 0 0 1 12 0" />
      <circle cx="17" cy="9" r="2.5" />
      <path d="M16 14.5a5 5 0 0 1 5 5.5" />
    </Icon>
  ),
  back: (
    <Icon>
      <path d="M10 6 4 12l6 6" />
      <path d="M4 12h16" />
    </Icon>
  ),
  files: (
    <Icon>
      <path d="M6 3h8l4 4v14H6Z" />
      <path d="M14 3v4h4" />
    </Icon>
  ),
  commits: (
    <Icon>
      <circle cx="12" cy="12" r="3" />
      <path d="M3 12h6M15 12h6" />
    </Icon>
  ),
  branches: (
    <Icon>
      <circle cx="6" cy="5" r="2" />
      <circle cx="6" cy="19" r="2" />
      <circle cx="18" cy="7" r="2" />
      <path d="M6 7v10M18 9c0 5-12 3-12 8" />
    </Icon>
  ),
  tags: (
    <Icon>
      <path d="M3 3h8l10 10-8 8L3 11Z" />
      <circle cx="7.5" cy="7.5" r="1.5" />
    </Icon>
  ),
  compare: (
    <Icon>
      <path d="M7 4v12M4 13l3 3 3-3" />
      <path d="M17 20V8M14 11l3-3 3 3" />
    </Icon>
  ),
  pulls: (
    <Icon>
      <circle cx="6" cy="6" r="2" />
      <circle cx="6" cy="18" r="2" />
      <circle cx="18" cy="18" r="2" />
      <path d="M6 8v8M18 16V9a3 3 0 0 0-3-3h-4" />
    </Icon>
  ),
  documents: (
    <Icon>
      <path d="M5 4h14v16H5Z" />
      <path d="M9 9h6M9 13h6M9 17h3" />
    </Icon>
  ),
  members: (
    <Icon>
      <circle cx="12" cy="8" r="3.5" />
      <path d="M5 20a7 7 0 0 1 14 0" />
    </Icon>
  ),
  webhooks: (
    <Icon>
      <path d="M10 14a4 4 0 0 1 0-5l3-3a4 4 0 0 1 5 5l-1.5 1.5" />
      <path d="M14 10a4 4 0 0 1 0 5l-3 3a4 4 0 0 1-5-5l1.5-1.5" />
    </Icon>
  ),
  settings: (
    <Icon>
      <circle cx="12" cy="12" r="3" />
      <path d="M12 2v3M12 19v3M2 12h3M19 12h3M4.9 4.9l2.1 2.1M17 17l2.1 2.1M4.9 19.1 7 17M17 7l2.1-2.1" />
    </Icon>
  ),
};

type IconKey = keyof typeof ICONS;

interface Item {
  to: string;
  icon: IconKey;
  text: string;
  current: boolean;
}

/** The last segment of a path (`acme/forge-web` → `forge-web`): the name
 * shown until the real one is loaded. */
const lastSegment = (path: string) => path.slice(path.lastIndexOf('/') + 1);

function useProjectItems(scope: SidebarScope, view: string[]): { title: Item; items: Item[] } {
  const { t } = useTranslation();
  const api = useApi();
  const isAdmin = useSession().user.is_admin;
  const project = useQuery({
    queryKey: ['project', scope.id],
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/projects/{id}', { signal, params: { path: { id: scope.id } } })),
  });
  const base = `/${encodePath(scope.path)}`;
  const word = view[0] ?? null;
  // The same rule as the project's own navigation (RepoViews): changing
  // webhooks and settings needs Maintainer or above (ProjectUpdate).
  const role = project.data?.role ?? null;
  const canManage = isAdmin || role === 'maintainer' || role === 'owner';
  const item = (words: string[], path: string, icon: IconKey, text: string): Item => ({
    to: `${base}/-/${path}`,
    icon,
    text,
    current: word !== null && words.includes(word),
  });
  return {
    title: {
      to: base,
      icon: 'projects',
      text: project.data?.display_name ?? lastSegment(scope.path),
      current: word === null,
    },
    items: [
      item(['tree', 'blob', 'blame'], 'tree', 'files', t('repo.files')),
      item(['commits', 'commit'], 'commits', 'commits', t('repo.commits')),
      item(['branches'], 'branches', 'branches', t('repo.branches')),
      item(['tags'], 'tags', 'tags', t('repo.tags')),
      item(['compare'], 'compare', 'compare', t('repo.compare')),
      item(['pulls', 'pull'], 'pulls', 'pulls', t('repo.pulls')),
      item(['documents'], 'documents', 'documents', t('docs.title')),
      item(['members'], 'members', 'members', t('members.title')),
      ...(canManage
        ? [
            item(['webhooks'], 'webhooks', 'webhooks', t('webhooks.title')),
            item(['settings'], 'settings', 'settings', t('settings.title')),
          ]
        : []),
    ],
  };
}

function useGroupItems(scope: SidebarScope, view: string[]) {
  const { t } = useTranslation();
  const api = useApi();
  const isAdmin = useSession().user.is_admin;
  const group = useQuery({
    queryKey: ['group', scope.id],
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/groups/{id}', { signal, params: { path: { id: scope.id } } })),
  });
  // The lists the group page reads too (no parent filter in the API).
  const groups = useQuery(groupsQuery(api));
  const projects = useQuery(projectsQuery(api));
  const base = `/${encodePath(scope.path)}`;
  const word = view[0] ?? null;
  // Changing a group (GroupUpdate) needs its Owner, or an administrator.
  const canManage = mayCreateSubgroup(group.data?.role ?? null, isAdmin);
  const title: Item = {
    to: base,
    icon: 'groups',
    text: group.data?.display_name ?? lastSegment(scope.path),
    current: word === null,
  };
  const items: Item[] = [
    {
      to: `${base}/-/members`,
      icon: 'members',
      text: t('members.title'),
      current: word === 'members',
    },
    ...(canManage
      ? [
          {
            to: `${base}/-/settings`,
            icon: 'settings' as const,
            text: t('settings.title'),
            current: word === 'settings',
          },
        ]
      : []),
  ];
  const child = (path: string, name: string, icon: IconKey): Item => ({
    to: `/${encodePath(path)}`,
    icon,
    text: name,
    current: false,
  });
  const children = {
    failed: groups.isError || projects.isError,
    groups: (groups.data?.items ?? [])
      .filter((g) => parentPath(g.path) === scope.path)
      .map((g) => child(g.path, g.display_name, 'groups')),
    projects: (projects.data?.items ?? [])
      .filter((p) => parentPath(p.path) === scope.path)
      .map((p) => child(p.path, p.display_name, 'projects')),
  };
  return { title, items, children };
}

export function Sidebar({
  id,
  narrow,
  open,
  onNavigate,
}: {
  id: string;
  narrow: boolean;
  /** Narrow screens only: the header's menu button opened it. */
  open: boolean;
  /** A link to `to` was followed (closes the narrow-screen panel). */
  onNavigate: (to: string) => void;
}) {
  const { t } = useTranslation();
  const { pathname } = useLocation();
  const found = useSidebarScope(pathname);
  const [stored, setStored] = useState(readCollapsed);
  // Narrow screens always show the names.
  const collapsed = !narrow && stored;

  const classes = [styles.sidebar, narrow ? styles.narrow : '', collapsed ? styles.collapsed : '']
    .filter((c) => c !== '')
    .join(' ');

  let label: string;
  let body: ReactNode;
  if (found?.scope.kind === 'project') {
    label = t('nav.projectMenu');
    body = <ProjectSection scope={found.scope} view={found.view} collapsed={collapsed} onNavigate={onNavigate} />;
  } else if (found?.scope.kind === 'group') {
    label = t('nav.groupMenu');
    body = <GroupSection scope={found.scope} view={found.view} collapsed={collapsed} onNavigate={onNavigate} />;
  } else {
    label = t('nav.myWork');
    body = <MyWork pathname={pathname} collapsed={collapsed} onNavigate={onNavigate} />;
  }

  return (
    <nav id={id} aria-label={label} className={classes} hidden={narrow && !open}>
      {!narrow && (
        <button
          type="button"
          className={styles.toggle}
          onClick={() => {
            setStored(!stored);
            storeCollapsed(!stored);
          }}
        >
          <Icon>{collapsed ? <path d="m9 6 6 6-6 6" /> : <path d="m15 6-6 6 6 6" />}</Icon>
          <Label collapsed={collapsed} text={collapsed ? t('nav.expand') : t('nav.collapse')} />
        </button>
      )}
      {body}
    </nav>
  );
}

function Label({ collapsed, text }: { collapsed: boolean; text: string }) {
  return <span className={collapsed ? styles.srOnly : styles.label}>{text}</span>;
}

function NavItem({
  item,
  collapsed,
  onNavigate,
  className,
}: {
  item: Item;
  collapsed: boolean;
  onNavigate: (to: string) => void;
  className?: string | undefined;
}) {
  return (
    <Link
      to={item.to}
      className={className ?? styles.link ?? ''}
      {...(item.current ? { 'aria-current': 'page' as const } : {})}
      onClick={() => {
        onNavigate(item.to);
      }}
    >
      {ICONS[item.icon]}
      <Label collapsed={collapsed} text={item.text} />
    </Link>
  );
}

function ItemList({
  items,
  collapsed,
  onNavigate,
}: {
  items: Item[];
  collapsed: boolean;
  onNavigate: (to: string) => void;
}) {
  return (
    <ul className={styles.list}>
      {items.map((item) => (
        <li key={item.to}>
          <NavItem item={item} collapsed={collapsed} onNavigate={onNavigate} />
        </li>
      ))}
    </ul>
  );
}

function MyWork({
  pathname,
  collapsed,
  onNavigate,
}: {
  pathname: string;
  collapsed: boolean;
  onNavigate: (to: string) => void;
}) {
  const { t } = useTranslation();
  const items: Item[] = [
    { to: '/', icon: 'home', text: t('nav.home'), current: pathname === '/' },
    { to: '/-/projects', icon: 'projects', text: t('nav.projects'), current: pathname === '/-/projects' },
    { to: '/-/groups', icon: 'groups', text: t('nav.groups'), current: pathname === '/-/groups' },
  ];
  return (
    <>
      <h2 className={collapsed ? styles.srOnly : styles.heading}>{t('nav.myWork')}</h2>
      <ItemList items={items} collapsed={collapsed} onNavigate={onNavigate} />
    </>
  );
}

/** "My work" as the way back, and the section's own name (its top page). */
function SectionHead({
  title,
  collapsed,
  onNavigate,
}: {
  title: Item;
  collapsed: boolean;
  onNavigate: (to: string) => void;
}) {
  const { t } = useTranslation();
  const back: Item = { to: '/', icon: 'back', text: t('nav.myWork'), current: false };
  return (
    <>
      <NavItem item={back} collapsed={collapsed} onNavigate={onNavigate} className={styles.back} />
      <div className={styles.contextTitle}>
        <NavItem item={title} collapsed={collapsed} onNavigate={onNavigate} />
      </div>
    </>
  );
}

function ProjectSection({
  scope,
  view,
  collapsed,
  onNavigate,
}: {
  scope: SidebarScope;
  view: string[];
  collapsed: boolean;
  onNavigate: (to: string) => void;
}) {
  const { title, items } = useProjectItems(scope, view);
  return (
    <>
      <SectionHead title={title} collapsed={collapsed} onNavigate={onNavigate} />
      <ItemList items={items} collapsed={collapsed} onNavigate={onNavigate} />
    </>
  );
}

function GroupSection({
  scope,
  view,
  collapsed,
  onNavigate,
}: {
  scope: SidebarScope;
  view: string[];
  collapsed: boolean;
  onNavigate: (to: string) => void;
}) {
  const { t } = useTranslation();
  const { title, items, children } = useGroupItems(scope, view);
  return (
    <>
      <SectionHead title={title} collapsed={collapsed} onNavigate={onNavigate} />
      <ItemList items={items} collapsed={collapsed} onNavigate={onNavigate} />
      {children.failed && !collapsed && <p className={styles.note}>{t('nav.childrenFailed')}</p>}
      {children.groups.length > 0 && (
        <>
          <h3 className={collapsed ? styles.srOnly : styles.heading}>{t('namespace.subgroups')}</h3>
          <ItemList items={children.groups} collapsed={collapsed} onNavigate={onNavigate} />
        </>
      )}
      {children.projects.length > 0 && (
        <>
          <h3 className={collapsed ? styles.srOnly : styles.heading}>{t('namespace.projects')}</h3>
          <ItemList items={children.projects} collapsed={collapsed} onNavigate={onNavigate} />
        </>
      )}
    </>
  );
}
