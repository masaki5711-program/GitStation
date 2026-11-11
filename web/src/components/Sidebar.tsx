// The sidebar (plan 15 §3.1). It follows where I am:
//
// - on a project's pages: the project section (sidebar/ProjectSection.tsx);
// - on a group's pages: the group section (sidebar/GroupSection.tsx);
// - elsewhere: "My work" (sidebar/MyWork.tsx). A project or group section
//   starts with a link back to it.
//
// The scope comes from the namespace resolver (sidebarScope.tsx).
//
// - Wide screens: a column; it collapses to icons, the names stay as
//   visually hidden text. The choice is kept in localStorage (not a secret;
//   storage may be missing or throw, then the column is simply expanded).
// - Narrow screens (< 768px): hidden until the header's menu button opens it
//   (Layout owns that state, the Esc key and the focus return).
import type { ReactNode } from 'react';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { useLocation } from 'react-router';

import styles from './Sidebar.module.css';
import { GroupSection } from './sidebar/GroupSection';
import { Icon } from './sidebar/icons';
import { MyWork } from './sidebar/MyWork';
import { Label } from './sidebar/parts';
import { ProjectSection } from './sidebar/ProjectSection';
import { useSidebarScope } from './sidebarScope';

const COLLAPSED_KEY = 'gitstation.sidebar.collapsed';

function readCollapsed(): boolean {
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

  const section = { collapsed, onNavigate };
  let label: string;
  let body: ReactNode;
  if (found?.scope.kind === 'project') {
    label = t('nav.projectMenu');
    body = <ProjectSection scope={found.scope} view={found.view} {...section} />;
  } else if (found?.scope.kind === 'group') {
    label = t('nav.groupMenu');
    body = <GroupSection scope={found.scope} view={found.view} {...section} />;
  } else {
    label = t('nav.myWork');
    body = <MyWork pathname={pathname} {...section} />;
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
