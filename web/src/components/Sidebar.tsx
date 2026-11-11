// The "My work" sidebar (plan 15 §3.1): Home, Projects, Groups. An item is
// added when its screen exists (no placeholders, plan 10): Activity comes
// with plan 15 MR 9. Notifications, settings and admin stay in the header.
//
// - Wide screens: a column; it collapses to icons, the names stay as
//   visually hidden text. The choice is kept in localStorage (not a secret;
//   storage may be missing or throw, then the column is simply expanded).
// - Narrow screens (< 768px): hidden until the header's menu button opens it
//   (Layout owns that state, the Esc key and the focus return).
import type { ReactNode } from 'react';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { NavLink } from 'react-router';

import styles from './Sidebar.module.css';

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
};

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
  const [stored, setStored] = useState(readCollapsed);
  // Narrow screens always show the names.
  const collapsed = !narrow && stored;
  const label = (text: string) => <span className={collapsed ? styles.srOnly : styles.label}>{text}</span>;

  const items = [
    { to: '/', key: 'home', text: t('nav.home') },
    { to: '/-/projects', key: 'projects', text: t('nav.projects') },
    { to: '/-/groups', key: 'groups', text: t('nav.groups') },
  ] as const;

  const classes = [styles.sidebar, narrow ? styles.narrow : '', collapsed ? styles.collapsed : '']
    .filter((c) => c !== '')
    .join(' ');

  return (
    <nav id={id} aria-label={t('nav.myWork')} className={classes} hidden={narrow && !open}>
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
          {label(collapsed ? t('nav.expand') : t('nav.collapse'))}
        </button>
      )}
      <h2 className={collapsed ? styles.srOnly : styles.heading}>{t('nav.myWork')}</h2>
      <ul className={styles.list}>
        {items.map((item) => (
          <li key={item.key}>
            <NavLink to={item.to} end               className={styles.link ?? ''}
              onClick={() => {
                onNavigate(item.to);
              }}
            >
              {ICONS[item.key]}
              {label(item.text)}
            </NavLink>
          </li>
        ))}
      </ul>
    </nav>
  );
}
