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
  useUserMergeRequests,
  useUserMrCounts,
  type MrScope,
  type UserMergeRequest,
} from '../api/userMergeRequests';
import controls from '../components/controls.module.css';
import { Loading } from '../components/Loading';
import { badgeText } from '../components/NotificationBell';
import { QueryError } from '../components/QueryError';
import { formatDateTime } from '../features/labels';
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

function scopeEmpty(t: TFunction, scope: MrScope): string {
  switch (scope) {
    case 'attention':
      return t('myMrs.emptyAttention');
    case 'approvable':
      return t('myMrs.emptyApprovable');
    case 'authored':
      return t('myMrs.emptyAuthored');
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
        <time dateTime={mr.updated_at}>
          {t('myMrs.updated', { time: formatDateTime(mr.updated_at, i18n.language) })}
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
        <ul className={styles.list} aria-label={scopeTitle(t, scope)}>
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
      <p role="status" className={styles.status}>
        {message}
      </p>
    </>
  );
}

function ScopeSection({ scope, initiallyOpen }: { scope: MrScope; initiallyOpen: boolean }) {
  const { t } = useTranslation();
  const headingId = useId();
  const [open, setOpen] = useState(initiallyOpen);
  const counts = useUserMrCounts();
  const count = counts.isSuccess ? counts.data[scope] : null;
  return (
    <section aria-labelledby={headingId} className={styles.section}>
      <details
        open={open}
        onToggle={(e) => {
          setOpen(e.currentTarget.open);
        }}
      >
        <summary className={styles.summary}>
          <h2 id={headingId} className={styles.heading}>
            {scopeTitle(t, scope)}{' '}
            {count !== null && (
              <span className={styles.count}>
                {t('myMrs.summaryCount', { shown: badgeText(count.count, count.capped) })}
              </span>
            )}
          </h2>
        </summary>
        {open && <ScopeList scope={scope} />}
      </details>
    </section>
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
      {/* A new ?scope= (a card or the sidebar while here) opens afresh. */}
      <div key={scope ?? ''}>
        {MR_SCOPES.map((s) => (
          <ScopeSection key={s} scope={s} initiallyOpen={scope === null || scope === s} />
        ))}
      </div>
    </>
  );
}
