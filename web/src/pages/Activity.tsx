// /-/activity (plan 15 §3.3, MR 9): what happened in the projects I take
// part in, newest first, grouped by day (in the browser's time zone), 30 at a
// time ("load more": `before` = the server's next_before). The server decides
// which events I may see; a page may hold fewer than 30.
// Focus is never lost on "load more" (it follows the render, not the
// request): the button keeps it while more remain;
// when the last page arrives the button goes away, so focus moves to the
// first new event (or to the "nothing older" line when none came).
import { useEffect, useMemo, useRef, useState } from 'react';
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
const ERROR_ID = 'activity-error';

/** A day's heading: the date, or a plain word when the moment is unreadable. */
function dayTitle(iso: string, language: string, unknown: string): string {
  return Number.isNaN(new Date(iso).getTime()) ? unknown : formatDate(iso, language);
}

/** A "load more" in flight: how many events were shown before it, and
 * whether its fetch was seen running (the focus waits for it to end). */
interface PendingLoad {
  from: number;
  started: boolean;
}

export function Activity() {
  const { t, i18n } = useTranslation();
  useDocumentTitle(t('activity.title'));
  const list = useUserActivity();
  // How many events were shown when the last "load more" was pressed (its
  // outcome line is derived from it); null before the first.
  const [lastLoad, setLastLoad] = useState<number | null>(null);
  const pending = useRef<PendingLoad | null>(null);
  const events = useMemo(() => list.data?.pages.flatMap((p) => p.events) ?? [], [list.data]);
  const truncated = list.data?.pages.some((p) => p.truncated) ?? false;
  const fetchingMore = list.isFetchingNextPage;

  // Focus follows what was rendered, once the load has ended: while more
  // remain the button keeps it; after the last page it moves to the first new
  // event (or the "nothing older" line); after a failure, to the retry.
  useEffect(() => {
    const load = pending.current;
    if (load === null) return;
    if (fetchingMore) {
      load.started = true;
      return;
    }
    if (!load.started) return;
    pending.current = null;
    if (list.isError) {
      // The notice's last button is its retry (the first copies the request id).
      const buttons = document.getElementById(ERROR_ID)?.querySelectorAll('button');
      buttons?.[buttons.length - 1]?.focus();
      return;
    }
    if (list.hasNextPage) return;
    const first = events[load.from];
    document.getElementById(first === undefined ? END_ID : itemId(first.seq))?.focus();
  }, [fetchingMore, list.isError, list.hasNextPage, events]);

  const more = () => {
    if (fetchingMore) return;
    pending.current = { from: events.length, started: false };
    setLastLoad(events.length);
    void list.fetchNextPage();
  };

  let status = '';
  if (fetchingMore) status = t('common.loading');
  else if (lastLoad !== null && !list.isError) {
    const added = events.length - lastLoad;
    status = added > 0 ? t('activity.moreShown', { n: added }) : t('activity.noMore');
  }

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
              {dayTitle(first.occurred_at, i18n.language, t('activity.dateUnknown'))}
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
        <div id={ERROR_ID}>
          <QueryError
            error={list.error}
            onRetry={() => {
              // A later page failed: ask for it again (the rows stay); the
              // first: read anew.
              if (list.isFetchNextPageError) {
                pending.current = { from: events.length, started: false };
                void list.fetchNextPage();
              } else {
                void list.refetch();
              }
            }}
          />
        </div>
      )}
      {list.isSuccess && events.length === 0 && !list.hasNextPage && (
        <p className={styles.empty}>{t('activity.empty')}</p>
      )}
      {list.isSuccess && list.hasNextPage && (
        <p className={styles.more}>
          <button
            type="button"
            className={controls.button}
            aria-disabled={fetchingMore ? 'true' : undefined}
            onClick={more}
          >
            {fetchingMore ? t('common.loading') : t('activity.more')}
          </button>
        </p>
      )}
      {/* The live region: "loading" while the next page is read, then what
          it added. */}
      <p id={END_ID} tabIndex={-1} role="status" className={styles.status}>
        {status}
      </p>
    </>
  );
}
