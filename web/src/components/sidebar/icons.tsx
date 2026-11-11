// The sidebar's icons: 24px strokes in the text colour, hidden from
// assistive tech (every item has its name as text).
import type { ReactNode } from 'react';

import styles from '../Sidebar.module.css';

export function Icon({ children }: { children: ReactNode }) {
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

export const ICONS = {
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
  more: (
    <Icon>
      <circle cx="5" cy="12" r="1" />
      <circle cx="12" cy="12" r="1" />
      <circle cx="19" cy="12" r="1" />
    </Icon>
  ),
  settings: (
    <Icon>
      <circle cx="12" cy="12" r="3" />
      <path d="M12 2v3M12 19v3M2 12h3M19 12h3M4.9 4.9l2.1 2.1M17 17l2.1 2.1M4.9 19.1 7 17M17 7l2.1-2.1" />
    </Icon>
  ),
};

export type IconKey = keyof typeof ICONS;
