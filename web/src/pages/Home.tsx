// Home (plan 15 §3.3, MR 7): count cards for what waits on me (my merge
// requests that need action, the ones waiting for my approval, my open ones,
// unread notifications), each a link to its list; then what I looked at
// recently (this browser only). The projects I take part in moved to
// /-/projects (linked from here); groups live on /-/groups.
// A card with nothing in it says the next step in one line (plan 08), not "0".
import { useMemo, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { useUnreadCount } from '../api/notifications';
import { useUserMrCounts, type UserMergeRequestCount } from '../api/userMergeRequests';
import { badgeText } from '../components/NotificationBell';
import { QueryError } from '../components/QueryError';
import { useDocumentTitle } from '../hooks/useDocumentTitle';
import { readRecent } from '../hooks/recent';
import styles from './Home.module.css';

/** `all`: the "see all" link to the full screen (plan 15 sidebar pages). */
function Block({
  id,
  title,
  all,
  children,
}: {
  id: string;
  title: string;
  all?: { to: string; label: string };
  children: ReactNode;
}) {
  return (
    <section aria-labelledby={id} className={styles.block}>
      <div className={styles.blockTop}>
        <h2 id={id} className={styles.heading}>
          {title}
        </h2>
        {all && (
          <Link to={all.to} className={styles.all}>
            {all.label}
          </Link>
        )}
      </div>
      {children}
    </section>
  );
}

/** What a card shows under its name: the number, the next step when it is
 * 0, a placeholder while it loads, or that it could not be read. */
type CardValue =
  | { status: 'pending' }
  | { status: 'error' }
  | { status: 'success'; count: UserMergeRequestCount };

/** A card is one link; its name is the card's whole text (label, then the
 * number with its unit for screen readers, or the next step). */
function Card({ to, label, next, value }: { to: string; label: string; next: string; value: CardValue }) {
  const { t } = useTranslation();
  let body: ReactNode;
  if (value.status === 'pending') {
    body = <span className={styles.cardNext}>{t('common.loading')}</span>;
  } else if (value.status === 'error') {
    body = <span className={styles.cardNext}>{t('home.countUnknown')}</span>;
  } else if (value.count.count === 0 && !value.count.capped) {
    body = <span className={styles.cardNext}>{next}</span>;
  } else {
    const shown = badgeText(value.count.count, value.count.capped);
    body = (
      <>
        <span className={styles.cardCount} aria-hidden="true">
          {shown}
        </span>
        <span className={styles.srOnly}>{t('home.cardCount', { shown })}</span>
      </>
    );
  }
  return (
    <li>
      <Link to={to} className={styles.card}>
        <span className={styles.cardLabel}>{label}</span> {body}
      </Link>
    </li>
  );
}

function valueOf<T>(
  query: { status: 'pending' | 'error' | 'success'; data: T | undefined },
  pick: (data: T) => UserMergeRequestCount,
): CardValue {
  if (query.status === 'success' && query.data !== undefined) return { status: 'success', count: pick(query.data) };
  return query.status === 'error' ? { status: 'error' } : { status: 'pending' };
}

export function Home() {
  const { t } = useTranslation();
  useDocumentTitle(t('home.title'));
  const recent = useMemo(() => readRecent(), []);
  // The same queries as the sidebar's badge and the header's bell (one
  // request each, shared cache).
  const counts = useUserMrCounts();
  const unread = useUnreadCount();

  return (
    <>
      <div className={styles.top}>
        <h1 className={styles.title}>{t('home.title')}</h1>
      </div>

      <Block id="home-work" title={t('home.work')} all={{ to: '/-/projects', label: t('home.allProjects') }}>
        <ul className={styles.cards}>
          <Card
            to="/-/merge-requests?scope=attention"
            label={t('home.cardAttention')}
            next={t('home.nextAttention')}
            value={valueOf(counts, (c) => c.attention)}
          />
          <Card
            to="/-/merge-requests?scope=approvable"
            label={t('home.cardApprovable')}
            next={t('home.nextApprovable')}
            value={valueOf(counts, (c) => c.approvable)}
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
        {/* The bell says when the unread count failed; the merge request
            counts are this page's own, so their reason is shown here. */}
        {counts.isError && (
          <QueryError
            error={counts.error}
            onRetry={() => {
              void counts.refetch();
            }}
          />
        )}
      </Block>

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
