// The header's bell (M3 U4): a link to the notifications page that names the
// unread count (the server's, of what the person may still see; polled with
// backoff, no websockets). The number is drawn as a badge (hidden from
// assistive tech) and spoken through the link's own hidden text (no
// aria-label: the visible number would not be part of it); when it grows
// while the page is open, a polite live region says so once. A failed poll leaves the bell without a number (the
// link's name says the count could not be read); the page shows the reason.
import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useLocation } from 'react-router';

import { useUnreadCount } from '../api/notifications';
import styles from './NotificationBell.module.css';

/** What the badge shows: up to 99, then "99+". */
export function badgeText(count: number, capped: boolean): string {
  return count > 99 || (capped && count >= 99) ? '99+' : String(count);
}

export function NotificationBell() {
  const { t } = useTranslation();
  const unread = useUnreadCount();
  const location = useLocation();
  const here = location.pathname === '/-/notifications';
  const [announcement, setAnnouncement] = useState('');
  const previous = useRef<number | null>(null);
  const count = unread.data?.count ?? null;

  useEffect(() => {
    if (count === null) return;
    const before = previous.current;
    previous.current = count;
    // The first answer is not news; only a growing count is announced.
    if (before !== null && count > before) {
      setAnnouncement(t('inbox.announceNew', { count }));
    }
  }, [count, t]);

  let label: string;
  if (unread.isError && count === null) label = t('inbox.bellUnknown');
  else if (count !== null && count > 0)
    label = t('inbox.bellUnread', { count, shown: badgeText(count, unread.data?.capped ?? false) });
  else label = t('inbox.bell');

  return (
    <>
      <Link to="/-/notifications" className={styles.bell} {...(here ? { 'aria-current': 'page' as const } : {})}>
        <span className={styles.srOnly}>{label}</span>
        <svg className={styles.icon} viewBox="0 0 24 24" width="20" height="20" aria-hidden="true" focusable="false">
          <path
            fill="currentColor"
            d="M12 2a6 6 0 0 0-6 6v3.6l-1.7 3A1 1 0 0 0 5.2 16h13.6a1 1 0 0 0 .9-1.4l-1.7-3V8a6 6 0 0 0-6-6Zm0 20a3 3 0 0 0 2.8-2H9.2A3 3 0 0 0 12 22Z"
          />
        </svg>
        {count !== null && count > 0 && (
          <span className={styles.badge} aria-hidden="true">
            {badgeText(count, unread.data?.capped ?? false)}
          </span>
        )}
      </Link>
      <span role="status" className={styles.srOnly}>
        {announcement}
      </span>
    </>
  );
}
