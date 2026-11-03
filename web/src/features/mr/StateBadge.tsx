// The one state badge of the merge request screens (list, header, threads):
// a word and a colour, never the colour alone.
import type { ReactNode } from 'react';

import styles from './Mr.module.css';

export type BadgeTone = 'open' | 'merged' | 'closed' | 'draft' | 'resolved' | 'unresolved';

const CLASS: Record<BadgeTone, string | undefined> = {
  open: styles.badgeOpen,
  merged: styles.badgeMerged,
  closed: styles.badgeClosed,
  draft: styles.badgeDraft,
  resolved: styles.badgeResolved,
  unresolved: styles.badgeUnresolved,
};

export function StateBadge({ tone, id, children }: { tone: BadgeTone; id?: string; children: ReactNode }) {
  return (
    <span className={CLASS[tone]} data-badge={tone} {...(id === undefined ? {} : { id })}>
      {children}
    </span>
  );
}

/** A merge request's state as a badge tone. */
export function mrTone(state: string): BadgeTone {
  return state === 'merged' ? 'merged' : state === 'closed' ? 'closed' : 'open';
}
