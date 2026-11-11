// Home (plan 15 §3.3, MR 7): count cards for what waits on me (my merge
// requests that need action, the ones waiting for my approval, my open ones,
// unread notifications), each a link to its list; then the latest 10
// events of my projects (MR 9; headed "Activity", the sidebar's name, all of
// them on /-/activity); then what I looked at recently (this browser only). The projects I take part in moved to
// /-/projects (a small link here); groups live on /-/groups.
// A card's link is named by its label and number only; what the card says
// besides (the next step when it is 0, loading, unknown) is its description
// (plan 08: one line, not a bare "0"). A card that waits on me (needs action,
// waiting for my approval) stands out when it is not 0.
import { useId, useMemo, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { useRecentActivity } from '../api/activity';
import { useUnreadCount } from '../api/notifications';
import { isNone, mrCountText, useUserMrCounts } from '../api/userMergeRequests';
import { Loading } from '../components/Loading';
import { QueryError } from '../components/QueryError';
import { ActivityItem } from '../features/ActivityItem';
import { useDocumentTitle } from '../hooks/useDocumentTitle';
import { readRecent } from '../hooks/recent';
import styles from './Home.module.css';

function Block({ id, title, children }: { id: string; title: string; children: ReactNode }) {
  return (
    <section aria-labelledby={id} className={styles.block}>
      <h2 id={id} className={styles.heading}>
        {title}
      </h2>
      {children}
    </section>
  );
}

/** A count as the API gives it (the unread count has no `truncated`). */
type CardCount = { count: number; capped: boolean; truncated?: boolean };

/** What a card knows: its number, nothing yet, or that it could not be read. */
type CardValue =
  | { status: 'pending' }
  | { status: 'error' }
  | { status: 'success'; count: CardCount };

function Card({
  to,
  label,
  next,
  value,
  waitsOnMe = false,
}: {
  to: string;
  label: string;
  next: string;
  value: CardValue;
  /** Stands out when not 0 (it waits on me). */
  waitsOnMe?: boolean;
}) {
  const { t } = useTranslation();
  const descriptionId = useId();
  let count: ReactNode = null;
  let description: string | null = null;
  let emphasis = false;
  if (value.status === 'pending') {
    description = t('common.loading');
  } else if (value.status === 'error') {
    description = t('home.countUnknown');
  } else {
    const n = value.count.count;
    const shown = mrCountText(value.count);
    const spoken = <span className={styles.srOnly}>{t('home.cardCount', { count: n, shown })}</span>;
    if (isNone(value.count)) {
      count = spoken;
      description = next;
    } else {
      emphasis = waitsOnMe;
      count = (
        <>
          <span className={styles.cardCount} aria-hidden="true">
            {shown}
          </span>
          {spoken}
        </>
      );
    }
  }
  return (
    <li className={emphasis ? `${styles.card} ${styles.cardWaiting}` : styles.card}>
      <Link
        to={to}
        className={styles.cardLink}
        {...(description === null ? {} : { 'aria-describedby': descriptionId })}
      >
        <span className={styles.cardLabel}>{label}</span> {count}
      </Link>
      {description !== null && (
        <p id={descriptionId} className={styles.cardNext}>
          {description}
        </p>
      )}
    </li>
  );
}

/** A query's state as a card's value (a later failed poll keeps the last number). */
function valueOf<T>(query: { status: 'pending' | 'error' | 'success'; data: T | undefined }, pick: (data: T) => CardCount): CardValue {
  if (query.data !== undefined) return { status: 'success', count: pick(query.data) };
  return query.status === 'error' ? { status: 'error' } : { status: 'pending' };
}

/** The latest events (plan 15 §3.3: 10), and the way to all of them. */
function RecentActivity() {
  const { t } = useTranslation();
  const recent = useRecentActivity();
  const events = recent.data?.events ?? [];
  return (
    <Block id="home-activity" title={t('home.activity')}>
      <p className={styles.lead}>{t('home.activityLead')}</p>
      {recent.isPending && <Loading />}
      {recent.isError && (
        <QueryError
          error={recent.error}
          onRetry={() => {
            void recent.refetch();
          }}
        />
      )}
      {recent.isSuccess && events.length === 0 && <p className={styles.hint}>{t('activity.empty')}</p>}
      {events.length > 0 && (
        <ul className={styles.activity}>
          {events.map((e) => (
            <ActivityItem key={e.seq} event={e} />
          ))}
        </ul>
      )}
      <p className={styles.activityAll}>
        <Link to="/-/activity">{t('home.activityAll')}</Link>
      </p>
    </Block>
  );
}

export function Home() {
  const { t } = useTranslation();
  useDocumentTitle(t('home.title'));
  const recent = useMemo(() => readRecent(), []);
  // The same queries as the sidebar's badge and the header's bell (one
  // request each, shared cache).
  const counts = useUserMrCounts();
  const unread = useUnreadCount();
  const countsFailed = counts.isError && counts.data === undefined;
  const unreadFailed = unread.isError && unread.data === undefined;

  return (
    <>
      <div className={styles.top}>
        <h1 className={styles.title}>{t('home.title')}</h1>
      </div>

      <Block id="home-work" title={t('home.work')}>
        <ul className={styles.cards}>
          <Card
            to="/-/merge-requests?scope=attention"
            label={t('home.cardAttention')}
            next={t('home.nextAttention')}
            value={valueOf(counts, (c) => c.attention)}
            waitsOnMe
          />
          <Card
            to="/-/merge-requests?scope=approvable"
            label={t('home.cardApprovable')}
            next={t('home.nextApprovable')}
            value={valueOf(counts, (c) => c.approvable)}
            waitsOnMe
          />
          <Card
            to="/-/merge-requests?scope=authored"
            label={t('home.cardAuthored')}
            next={t('home.nextAuthored')}
            value={valueOf(counts, (c) => c.authored)}
          />
          <Card
            to="/-/notifications?unread=1"
            label={t('home.cardUnread')}
            next={t('home.nextUnread')}
            value={valueOf(unread, (u) => u)}
          />
        </ul>
        {/* One failure notice for the cards: the merge request counts' when
            they failed (its retry asks for the unread count too), else the
            unread count's. */}
        {countsFailed && (
          <QueryError
            error={counts.error}
            onRetry={() => {
              void counts.refetch();
              if (unreadFailed) void unread.refetch();
            }}
          />
        )}
        {!countsFailed && unreadFailed && (
          <QueryError
            error={unread.error}
            onRetry={() => {
              void unread.refetch();
            }}
          />
        )}
      </Block>

      <p className={styles.projectsLink}>
        <Link to="/-/projects">{t('home.allProjects')}</Link>
      </p>

      <RecentActivity />

      {recent.length > 0 && (
        <Block id="home-recent" title={t('home.recent')}>
          <ul className={styles.recent}>
            {recent.map((r) => (
              <li key={r.path}>
                <Link to={`/${r.path}`}>{r.name}</Link> <code className={styles.path}>{r.path}</code>
              </li>
            ))}
          </ul>
          <p className={styles.hint}>{t('home.recentHint')}</p>
        </Block>
      )}
    </>
  );
}
