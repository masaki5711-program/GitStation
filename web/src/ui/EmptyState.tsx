// The empty state of a list (plan 08 §1-7: why it is empty and what to press
// next): an icon (decorative, hidden from assistive tech), the reason in one
// line, optional short hints, and one primary action. `compact` drops the
// dashed box when the state sits inside a card.
import type { ReactNode } from 'react';

import styles from './EmptyState.module.css';

export type EmptyIcon = 'folder' | 'group' | 'inbox' | 'activity' | 'mergeRequest';

const PATHS: Record<EmptyIcon, ReactNode> = {
  folder: <path d="M3 6a1 1 0 0 1 1-1h5l2 2h9a1 1 0 0 1 1 1v10a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1Z" />,
  group: (
    <>
      <circle cx="9" cy="8" r="3" />
      <path d="M3 19a6 6 0 0 1 12 0" />
      <path d="M16 5a3 3 0 0 1 0 6M18 19a6 6 0 0 0-2-4.5" />
    </>
  ),
  inbox: (
    <>
      <path d="M3 13h5l1 3h6l1-3h5" />
      <path d="M5 5h14l2 8v6H3v-6Z" />
    </>
  ),
  activity: <path d="M3 12h4l3-7 4 14 3-7h4" />,
  mergeRequest: (
    <>
      <circle cx="6" cy="6" r="2" />
      <circle cx="6" cy="18" r="2" />
      <circle cx="18" cy="18" r="2" />
      <path d="M6 8v8M18 16V9a2 2 0 0 0-2-2h-4M14 5l-2 2 2 2" />
    </>
  ),
};

export function EmptyState({
  icon,
  reason,
  hints = [],
  action,
  compact = false,
  live = false,
}: {
  icon: EmptyIcon;
  /** Why the list is empty, in one line. */
  reason: string;
  hints?: string[];
  /** The one next step (a button or a link). */
  action?: ReactNode;
  compact?: boolean;
  /** The reason is announced (it replaces a list the person just changed, e.g. a filter). */
  live?: boolean;
}) {
  return (
    <div className={compact ? styles.compact : styles.root}>
      <span className={styles.icon}>
        <svg
          viewBox="0 0 24 24"
          width="22"
          height="22"
          aria-hidden="true"
          focusable="false"
          fill="none"
          stroke="currentColor"
          strokeWidth="2"
          strokeLinecap="round"
          strokeLinejoin="round"
        >
          {PATHS[icon]}
        </svg>
      </span>
      <p className={styles.reason} {...(live ? { role: 'status' } : {})}>
        {reason}
      </p>
      {hints.map((h) => (
        <p key={h} className={styles.hint}>
          {h}
        </p>
      ))}
      {action !== undefined && <div className={styles.action}>{action}</div>}
    </div>
  );
}
