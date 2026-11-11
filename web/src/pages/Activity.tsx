// /-/activity (plan 15 §3.3, MR 9): what happened in the projects I take
// part in, newest first, grouped by day (in the browser's time zone), 30 at a
// time ("load more": `before` = the server's next_before). The server decides
// which events I may see; a page may hold fewer than 30.
// Focus is never lost on "load more": the button keeps it while more remain;
// when the last page arrives the button goes away, so focus moves to the
// first new event (or to the "nothing older" line when none came).
import { useEffect, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { useUserActivity, type ActivityEvent } from '../api/activity';
import controls from '../components/controls.module.css';
import { Loading } from '../components/Loading';
import { QueryError } from '../components/QueryError';
import { ActivityItem } from '../features/ActivityItem';
import { formatDate } from '../features/labels';
import { useDocumentTitle } from '../hooks/useDocumentTitle';
import styles from './Activity.module.css';

/** The local calendar day of a moment (`2026-11-11`); '' when unreadable. */
function dayOf(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '';
  const pad = (n: number) => String(n).padStart(2, '0');
  return `${String(d.getFullYear())}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}`;
}

/** Events (newest first) as runs of the same day, in order. */
export function groupByDay(events: ActivityEvent[]): { day: string; events: ActivityEvent[] }[] {
  const groups: { day: string; events: ActivityEvent[] }[] = [];
  for (const event of events) {
    const day = dayOf(event.occurred_at);
    const last = groups.at(-1);
    if (last !== undefined && last.day === day) last.events.push(event);
    else groups.push({ day, events: [event] });
  }
  return groups;
}

const itemId = (seq: number) => `activity-${String(seq)}`;
const END_ID = 'activity-end';

export function Activity() {
  const { t, i18n } = useTranslation();
  useDocumentTitle(t('activity.title'));
  const list = useUserActivity();
  const [message, setMessage] = useState('');
  const [focusTarget, setFocusTarget] = useState<string | null>(null);
  const events = list.data?.pages.flatMap((p) => p.events) ?? [];
  const truncated = list.data?.pages.some((p) => p.truncated) ?? false;

  useEffect(() => {
    if (focusTarget === null) return;
    document.getElementById(focusTarget)?.focus();
    setFocusTarget(null);
  }, [focusTarget]);

  const more = () => {
    if (list.isFetchingNextPage) return;
    const before = events.length;
    setMessage('');
    void list.fetchNextPage().then((r) => {
      if (r.isError) return;
      const all = r.data?.pages.flatMap((p) => p.events) ?? [];
      const added = all.slice(before);
      setMessage(added.length > 0 ? t('activity.moreShown', { n: added.length }) : t('activity.noMore'));
      if (!r.hasNextPage) {
        const first = added[0];
        setFocusTarget(first === undefined ? END_ID : itemId(first.seq));
      }
    });
  };

  return (
    <>
      <h1 className={styles.title}>{t('activity.title')}</h1>
      <p className={styles.explain}>{t('activity.explain')}</p>
      {truncated && <p className={styles.note}>{t('activity.truncated')}</p>}
      {list.isPending && <Loading />}
      {groupByDay(events).map((g) => {
        const first = g.events[0];
        if (first === undefined) return null;
        // Named by its first event: unique even if a day came twice.
        const headingId = `activity-day-${String(first.seq)}`;
        return (
          <section key={headingId} aria-labelledby={headingId} className={styles.day}>
            <h2 id={headingId} className={styles.dayHeading}>
              {formatDate(first.occurred_at, i18n.language)}
            </h2>
            <ul className={styles.list}>
              {g.events.map((e) => (
                <ActivityItem key={e.seq} event={e} id={itemId(e.seq)} />
              ))}
            </ul>
          </section>
        );
      })}
      {list.isError && (
        <QueryError
          error={list.error}
          onRetry={() => {
            // A later page failed: ask for it again; the first: read anew.
            void (list.isFetchNextPageError ? list.fetchNextPage() : list.refetch());
          }}
        />
      )}
      {list.isSuccess && events.length === 0 && !list.hasNextPage && (
        <p className={styles.empty}>{t('activity.empty')}</p>
      )}
      {list.isSuccess && list.hasNextPage && (
        <p className={styles.more}>
          <button
            type="button"
            className={controls.button}
            aria-disabled={list.isFetchingNextPage ? 'true' : undefined}
            onClick={more}
          >
            {list.isFetchingNextPage ? t('common.loading') : t('activity.more')}
          </button>
        </p>
      )}
      <p id={END_ID} tabIndex={-1} role="status" className={styles.status}>
        {message}
      </p>
    </>
  );
}
