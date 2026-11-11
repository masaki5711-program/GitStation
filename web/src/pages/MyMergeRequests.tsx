// /-/merge-requests (plan 15 §3.3, MR 7): my open merge requests across
// projects, one section per scope (mine that need action, the ones waiting
// for my approval, all of mine). Each section is a native disclosure
// (<details>: keyboard and screen readers for free) and reads its list only
// while open, page by page ("load more": `before` = the server's
// next_before). `?scope=` opens that section alone (Home's cards link here);
// without it every section is open. The server decides which rows I may see.
import { useId, useState } from 'react';
import type { TFunction } from 'i18next';
import { useTranslation } from 'react-i18next';
import { Link, useSearchParams } from 'react-router';

import {
  MR_SCOPES,
  isMrScope,
  mrCountText,
  useUserMergeRequests,
  useUserMrCounts,
  type MrScope,
  type UserMergeRequest,
} from '../api/userMergeRequests';
import controls from '../components/controls.module.css';
import { Loading } from '../components/Loading';
import { QueryError } from '../components/QueryError';
import { formatDateTime, formatRelative } from '../features/labels';
import { pullUrl } from '../features/repoUrls';
import { useDocumentTitle } from '../hooks/useDocumentTitle';
import styles from './MyMergeRequests.module.css';

/** Why a merge request waits on its author, in plain words. */
function reasonLabel(t: TFunction, reason: string): string {
  switch (reason) {
    case 'conflict':
      return t('myMrs.reasonConflict');
    case 'conflict_predicted':
      return t('myMrs.reasonConflictPredicted');
    case 'required_check_failed':
      return t('myMrs.reasonCheckFailed');
    default:
      return t('myMrs.reasonOther');
  }
}

function scopeTitle(t: TFunction, scope: MrScope): string {
  switch (scope) {
    case 'attention':
      return t('myMrs.attention');
    case 'approvable':
      return t('myMrs.approvable');
    case 'authored':
      return t('myMrs.authored');
  }
}

function scopeHint(t: TFunction, scope: MrScope): string {
  switch (scope) {
    case 'attention':
      return t('myMrs.hintAttention');
    case 'approvable':
      return t('myMrs.hintApprovable');
    case 'authored':
      return t('myMrs.hintAuthored');
  }
}

/** An empty section says the next step (plan 08), as Home's cards do. */
function scopeEmpty(t: TFunction, scope: MrScope): string {
  switch (scope) {
    case 'attention':
      return t('home.nextAttention');
    case 'approvable':
      return t('home.nextApprovable');
    case 'authored':
      return t('home.nextAuthored');
  }
}

function Row({ mr }: { mr: UserMergeRequest }) {
  const { t, i18n } = useTranslation();
  // Reasons are a set; the server never repeats one, but a key must be unique.
  const reasons = [...new Set(mr.attention)];
  const reference = `${mr.project_path}!${String(mr.iid)}`;
  return (
    <li className={styles.item}>
      <p className={styles.titleLine}>
        <Link to={pullUrl(mr.project_path, mr.iid)} className={styles.title}>
          {mr.title}
        </Link>
        {mr.draft && <span className={styles.draft}>{t('myMrs.draft')}</span>}
      </p>
      <p className={styles.meta}>
        <span className={styles.ref}>{reference}</span>
        {reasons.map((r) => (
          <span key={r} className={styles.reason}>
            {reasonLabel(t, r)}
          </span>
        ))}
        <time dateTime={mr.updated_at} title={formatDateTime(mr.updated_at, i18n.language)} className={styles.time}>
          {t('myMrs.updated', { time: formatRelative(mr.updated_at, i18n.language) })}
        </time>
      </p>
    </li>
  );
}

/** The open section's list: loading, failure (with its request id and a
 * retry), empty, rows, and "load more". */
function ScopeList({ scope }: { scope: MrScope }) {
  const { t } = useTranslation();
  const list = useUserMergeRequests(scope, true);
  const [message, setMessage] = useState('');
  const items = list.data?.pages.flatMap((p) => p.merge_requests) ?? [];

  const more = () => {
    if (list.isFetchingNextPage) return;
    const before = items.length;
    setMessage('');
    void list.fetchNextPage().then((r) => {
      if (r.isError) return;
      const all = r.data?.pages.flatMap((p) => p.merge_requests) ?? [];
      const added = all.length - before;
      if (added > 0) setMessage(t('myMrs.moreShown', { n: added }));
    });
  };

  return (
    <>
      <p className={styles.hint}>{scopeHint(t, scope)}</p>
      {list.isPending && <Loading />}
      {items.length > 0 && (
        <ul className={styles.list}>
          {items.map((mr) => (
            <Row key={mr.id} mr={mr} />
          ))}
        </ul>
      )}
      {list.isError && (
        <QueryError
          error={list.error}
          onRetry={() => {
            // A later page failed: ask for it again; the first: read anew.
            void (list.isFetchNextPageError ? list.fetchNextPage() : list.refetch());
          }}
        />
      )}
      {list.isSuccess && items.length === 0 && !list.hasNextPage && (
        <p className={styles.empty}>{scopeEmpty(t, scope)}</p>
      )}
      {list.isSuccess && list.hasNextPage && (
        <p className={styles.more}>
          <button
            type="button"
            className={controls.button}
            aria-disabled={list.isFetchingNextPage ? 'true' : undefined}
            onClick={more}
          >
            {list.isFetchingNextPage ? t('common.loading') : t('myMrs.more')}
          </button>
        </p>
      )}
      {message !== '' && (
        <p role="status" className={styles.status}>
          {message}
        </p>
      )}
    </>
  );
}

/** A section is labelled by its summary (plain text: the native disclosure
 * button; no heading inside it). The count comes from the shared counts
 * query; when that failed the summary says the count is unknown (the page
 * shows the reason once). */
function ScopeSection({ scope, initiallyOpen }: { scope: MrScope; initiallyOpen: boolean }) {
  const { t } = useTranslation();
  const summaryId = useId();
  const [open, setOpen] = useState(initiallyOpen);
  const counts = useUserMrCounts();
  let countText: string | null = null;
  if (counts.data !== undefined) {
    const c = counts.data[scope];
    countText = t('myMrs.summaryCount', { count: c.count, shown: mrCountText(c) });
  } else if (counts.isError) {
    countText = t('myMrs.countUnknown');
  }
  return (
    <section aria-labelledby={summaryId} className={styles.section}>
      <details
        open={open}
        onToggle={(e) => {
          setOpen(e.currentTarget.open);
        }}
      >
        <summary id={summaryId} className={styles.summary}>
          <span className={styles.summaryTitle}>{scopeTitle(t, scope)}</span>{' '}
          {countText !== null && <span className={styles.count}>{countText}</span>}
        </summary>
        {open && <ScopeList scope={scope} />}
      </details>
    </section>
  );
}

/** The counts failed: said once for the page, with its request id and a retry. */
function CountsError() {
  const counts = useUserMrCounts();
  if (!counts.isError || counts.data !== undefined) return null;
  return (
    <QueryError
      error={counts.error}
      onRetry={() => {
        void counts.refetch();
      }}
    />
  );
}

export function MyMergeRequests() {
  const { t } = useTranslation();
  useDocumentTitle(t('myMrs.title'));
  const [params] = useSearchParams();
  const raw = params.get('scope');
  const scope = isMrScope(raw) ? raw : null;
  return (
    <>
      <h1 className={styles.pageTitle}>{t('myMrs.title')}</h1>
      <p className={styles.explain}>{t('myMrs.explain')}</p>
      <CountsError />
      {/* A new ?scope= (a card or the sidebar while here) opens afresh. */}
      <div key={scope ?? ''}>
        {MR_SCOPES.map((s) => (
          <ScopeSection key={s} scope={s} initiallyOpen={scope === null || scope === s} />
        ))}
      </div>
    </>
  );
}
