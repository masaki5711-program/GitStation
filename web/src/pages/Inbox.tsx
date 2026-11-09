// `/-/notifications` (M3 U4): the signed-in person's in-app notifications,
// newest first. All or unread only (the `?unread=1` query, so a link can
// point at either); each item says what happened in one sentence and links
// to the merge request (following the link marks it read); "Mark as read"
// per item and "Mark all as read" (up to the newest item shown, so one that
// arrives meanwhile stays unread). The server lists only what the person may
// still see. Results are announced in a live region (the same words twice
// are read out twice); the focus never falls to the page body: after a mark
// it moves to the next item's link (or the heading), after "Mark all" to the
// heading, after "Show more" to the first new item. A refusal shows its
// reason and request id.
import { useCallback, useEffect, useRef, useState } from 'react';
import type { TFunction } from 'i18next';
import { useTranslation } from 'react-i18next';
import { Link, useSearchParams } from 'react-router';

import {
  useMarkAllRead,
  useMarkRead,
  useNotifications,
  type InAppNotification,
} from '../api/notifications';
import { Loading } from '../components/Loading';
import { QueryError } from '../components/QueryError';
import controls from '../components/controls.module.css';
import { formatDateTime } from '../features/labels';
import { pullUrl } from '../features/repoUrls';
import { useDocumentTitle } from '../hooks/useDocumentTitle';
import { useSession } from '../session/session';
import { Denied } from './settings/common';
import styles from './Inbox.module.css';

/** What happened, in one sentence (a kind this screen does not know gets a
 * general sentence). */
function sentence(t: TFunction, item: InAppNotification): string {
  const actor = item.actor_name;
  switch (item.kind) {
    case 'mr_approved':
      return actor === null ? t('inbox.kind.approvedBySystem') : t('inbox.kind.approved', { actor });
    case 'mr_merged':
      return actor === null ? t('inbox.kind.mergedBySystem') : t('inbox.kind.merged', { actor });
    case 'mr_commented':
      return actor === null ? t('inbox.kind.commentedBySystem') : t('inbox.kind.commented', { actor });
    case 'mr_conflict_predicted':
      return t('inbox.kind.conflictPredicted');
    default:
      return t('inbox.kind.other');
  }
}

/** A polite message: emptied first, the words a moment later, so the same
 * words twice are still a change that is read out. */
function useAnnouncement(): [string, (text: string) => void] {
  const [message, setMessage] = useState('');
  const timer = useRef<number | undefined>(undefined);
  const announce = useCallback((text: string) => {
    setMessage('');
    window.clearTimeout(timer.current);
    if (text === '') return;
    timer.current = window.setTimeout(() => {
      setMessage(text);
    }, 50);
  }, []);
  useEffect(
    () => () => {
      window.clearTimeout(timer.current);
    },
    [],
  );
  return [message, announce];
}

export function Inbox() {
  const { t } = useTranslation();
  const session = useSession();
  const [params, setParams] = useSearchParams();
  const unread = params.get('unread') === '1';
  const list = useNotifications(unread);
  const markAll = useMarkAllRead();
  const [message, announce] = useAnnouncement();
  const heading = useRef<HTMLHeadingElement>(null);
  const listRef = useRef<HTMLUListElement>(null);
  useDocumentTitle(t('inbox.title'));

  const items = list.data?.pages.flatMap((p) => p.items) ?? [];
  const newest = items[0]?.id;
  const anyUnread = items.some((i) => i.read_at === null);

  /** Focus item `id`'s link, or the heading when it is not on the page. */
  const focusItem = (id: number | undefined) => {
    window.setTimeout(() => {
      const link =
        id === undefined ? null : listRef.current?.querySelector<HTMLAnchorElement>(`li[data-id="${String(id)}"] a`);
      if (link !== null && link !== undefined) link.focus();
      else heading.current?.focus();
    }, 0);
  };

  /** After item `id` was marked: its neighbour below, else above. */
  const afterMarked = (id: number) => {
    const at = items.findIndex((i) => i.id === id);
    const next = items[at + 1] ?? items[at - 1];
    announce(t('inbox.marked'));
    focusItem(next?.id);
  };

  const show = (onlyUnread: boolean) => {
    announce('');
    setParams(onlyUnread ? { unread: '1' } : {});
  };

  const more = () => {
    if (list.isFetchingNextPage) return;
    const before = items.length;
    void list.fetchNextPage().then((r) => {
      // A failed fetch shows its error where the list is; nothing to say or
      // to focus then.
      if (r.isError) return;
      const all = r.data?.pages.flatMap((p) => p.items) ?? [];
      const added = all.slice(before);
      if (added.length === 0) return;
      announce(t('inbox.moreShown', { count: added.length }));
      focusItem(added[0]?.id);
    });
  };

  return (
    <section aria-labelledby="inbox-title">
      <h1 id="inbox-title" ref={heading} tabIndex={-1} className={styles.title}>
        {t('inbox.title')}
      </h1>
      <p className={styles.hint}>
        {t('inbox.explain')}{' '}
        <Link to={`/${session.user.username}/-/settings?tab=notifications`}>{t('inbox.settings')}</Link>
      </p>
      <div className={styles.toolbar}>
        <div role="group" aria-label={t('inbox.filter')} className={styles.filter}>
          <button
            type="button"
            className={controls.button}
            aria-pressed={!unread}
            onClick={() => {
              show(false);
            }}
          >
            {t('inbox.all')}
          </button>
          <button
            type="button"
            className={controls.button}
            aria-pressed={unread}
            onClick={() => {
              show(true);
            }}
          >
            {t('inbox.unreadOnly')}
          </button>
        </div>
        {list.isSuccess && anyUnread && (
          <button
            type="button"
            className={controls.button}
            aria-disabled={markAll.isPending ? 'true' : undefined}
            onClick={() => {
              if (markAll.isPending) return;
              announce('');
              markAll.mutate(newest, {
                onSuccess: () => {
                  announce(t('inbox.markedAll'));
                  focusItem(undefined);
                },
              });
            }}
          >
            {markAll.isPending ? t('inbox.marking') : t('inbox.markAll')}
          </button>
        )}
      </div>
      {markAll.isError && <Denied error={markAll.error} />}
      {list.isPending && <Loading />}
      {list.isError && (
        <QueryError
          error={list.error}
          onRetry={() => {
            void list.refetch();
          }}
        />
      )}
      {list.isSuccess && items.length === 0 && !list.hasNextPage && (
        <p className={styles.empty}>{unread ? t('inbox.emptyUnread') : t('inbox.empty')}</p>
      )}
      {items.length > 0 && (
        <ul ref={listRef} className={styles.list} aria-label={unread ? t('inbox.unreadOnly') : t('inbox.all')}>
          {items.map((item) => (
            <Item key={item.id} item={item} onMarked={afterMarked} />
          ))}
        </ul>
      )}
      {list.isSuccess && list.hasNextPage && (
        <p>
          <button
            type="button"
            className={controls.button}
            aria-disabled={list.isFetchingNextPage ? 'true' : undefined}
            onClick={more}
          >
            {list.isFetchingNextPage ? t('common.loading') : t('inbox.more')}
          </button>
        </p>
      )}
      <p role="status" className={styles.status}>
        {message}
      </p>
    </section>
  );
}

function Item({ item, onMarked }: { item: InAppNotification; onMarked: (id: number) => void }) {
  const { t, i18n } = useTranslation();
  const mark = useMarkRead();
  const isUnread = item.read_at === null;
  const reference = `${item.project_path}!${String(item.merge_request_iid)}`;
  const markRead = (byButton: boolean) => {
    if (!isUnread || mark.isPending) return;
    mark.mutate(item.id, {
      onSuccess: () => {
        if (byButton) onMarked(item.id);
      },
    });
  };
  return (
    <li data-id={item.id} className={isUnread ? `${styles.item} ${styles.unread}` : styles.item}>
      <div className={styles.body}>
        <p className={styles.sentence}>
          {isUnread && <span className={styles.dot}>{t('inbox.unreadMark')}</span>}
          {sentence(t, item)}
        </p>
        <p className={styles.target}>
          <Link
            to={pullUrl(item.project_path, item.merge_request_iid)}
            onClick={() => {
              markRead(false);
            }}
          >
            <span className={styles.ref}>{reference}</span>{' '}
            {item.merge_request_title}
          </Link>
        </p>
        <p className={styles.time}>
          <time dateTime={item.created_at}>{formatDateTime(item.created_at, i18n.language)}</time>
        </p>
        {mark.isError && <Denied error={mark.error} />}
      </div>
      {isUnread && (
        <button
          type="button"
          className={controls.button}
          aria-disabled={mark.isPending ? 'true' : undefined}
          onClick={() => {
            markRead(true);
          }}
        >
          {t('inbox.markRead')}
          <span className={styles.srOnly}>{t('inbox.markReadTarget', { title: item.merge_request_title })}</span>
        </button>
      )}
    </li>
  );
}
