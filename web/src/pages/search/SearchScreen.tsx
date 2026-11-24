// /-/search?q=…&scope=… (plan 20 K5): the results of the search box. The
// query and the kind filter live in the address (a result can be shared and
// the back button works). Hits come page by page ("more": the server's
// next_cursor); there is no total, so the screen says how many it shows.
// States: no query yet / too short (no request), loading, failure (the
// server's message and request id, with a retry), nothing found (the next
// step), hits.
import type { TFunction } from 'i18next';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useNavigate, useSearchParams } from 'react-router';

import {
  MIN_QUERY_CHARS,
  SEARCH_KINDS,
  isSearchKind,
  queryChars,
  snippetPieces,
  useSearch,
  type SearchHit,
  type SearchKind,
} from '../../api/search';
import controls from '../../components/controls.module.css';
import { QueryError } from '../../components/QueryError';
import { SkeletonList } from '../../components/Skeleton';
import { formatDateTime, formatRelative } from '../../features/labels';
import { encodePath, pullUrl } from '../../features/repoUrls';
import { useDocumentTitle } from '../../hooks/useDocumentTitle';
import { EmptyState } from '../../ui/EmptyState';
import { PageHeader } from '../../ui/PageHeader';
import styles from './SearchScreen.module.css';

function kindLabel(t: TFunction, kind: string): string {
  switch (kind) {
    case 'project':
      return t('search.kindProject');
    case 'merge_request':
      return t('search.kindMergeRequest');
    default:
      return t('search.kindOther');
  }
}

/** The address of a hit: a merge request's page, else its project's. */
function hitUrl(hit: SearchHit): string {
  if (hit.kind === 'merge_request' && hit.iid !== null) return pullUrl(hit.project_path, hit.iid);
  return `/${encodePath(hit.project_path)}`;
}

function searchUrl(q: string, kind: SearchKind | null): string {
  const p = new URLSearchParams({ q });
  if (kind !== null) p.set('scope', kind);
  return `/-/search?${p.toString()}`;
}

function Hit({ hit }: { hit: SearchHit }) {
  const { t, i18n } = useTranslation();
  const { snippet } = hit;
  const where = hit.kind === 'merge_request' && hit.iid !== null ? `${hit.project_path}!${String(hit.iid)}` : hit.project_path;
  return (
    <li className={styles.item}>
      <p className={styles.titleLine}>
        <span className={styles.kind}>{kindLabel(t, hit.kind)}</span>
        <Link to={hitUrl(hit)} className={styles.title}>
          {hit.title}
        </Link>
      </p>
      <p className={styles.meta}>
        <span className={styles.path}>{where}</span>
        <time dateTime={hit.updated_at} title={formatDateTime(hit.updated_at, i18n.language)}>
          {t('search.updated', { time: formatRelative(hit.updated_at, i18n.language) })}
        </time>
      </p>
      {snippet.text !== '' && (
        <p
          className={[styles.snippet, snippet.cut_before ? styles.cutBefore : '', snippet.cut_after ? styles.cutAfter : '']
            .filter(Boolean)
            .join(' ')}
        >
          {snippetPieces(snippet).map((p, i) =>
            p.mark ? (
              <mark key={i} className={styles.mark}>
                {p.text}
              </mark>
            ) : (
              <span key={i}>{p.text}</span>
            ),
          )}
        </p>
      )}
      {hit.truncated && <p className={styles.note}>{t('search.truncated')}</p>}
    </li>
  );
}

function Results({ q, kind }: { q: string; kind: SearchKind | null }) {
  const { t } = useTranslation();
  const list = useSearch(q, kind, true);
  const items = list.data?.pages.flatMap((p) => p.items) ?? [];

  return (
    <>
      {list.isPending && <SkeletonList />}
      {list.isSuccess && items.length > 0 && (
        <p role="status" className={styles.status}>
          {t('search.shown', { count: items.length })}
        </p>
      )}
      {items.length > 0 && (
        <ol className={styles.list}>
          {items.map((hit) => (
            <Hit key={`${hit.kind}:${String(hit.project_id)}:${String(hit.iid ?? 0)}`} hit={hit} />
          ))}
        </ol>
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
        <EmptyState
          icon="search"
          live
          reason={t('search.noResults', { q })}
          hints={[t('search.noResultsHint')]}
          action={
            kind !== null ? (
              <Link to={searchUrl(q, null)} className={controls.button}>
                {t('search.allKinds')}
              </Link>
            ) : (
              <Link to="/-/projects" className={controls.button}>
                {t('search.toProjects')}
              </Link>
            )
          }
        />
      )}
      {list.isSuccess && list.hasNextPage && (
        <div className={styles.more}>
          {items.length === 0 && <p className={styles.note}>{t('search.notYet')}</p>}
          <button
            type="button"
            className={controls.button}
            aria-disabled={list.isFetchingNextPage ? 'true' : undefined}
            onClick={() => {
              if (!list.isFetchingNextPage) void list.fetchNextPage();
            }}
          >
            {list.isFetchingNextPage ? t('common.loading') : t('search.more')}
          </button>
        </div>
      )}
    </>
  );
}

export function SearchScreen() {
  const { t } = useTranslation();
  const navigate = useNavigate();
  const [params] = useSearchParams();
  const q = (params.get('q') ?? '').trim();
  const rawKind = params.get('scope');
  const kind = isSearchKind(rawKind) ? rawKind : null;
  const [text, setText] = useState(q);
  const [shownQ, setShownQ] = useState(q);
  // A new ?q= (the header box while here) puts its words in this box.
  if (shownQ !== q) {
    setShownQ(q);
    setText(q);
  }
  useDocumentTitle(q === '' ? t('search.title') : t('search.titleFor', { q }));
  const long = queryChars(q) >= MIN_QUERY_CHARS;

  return (
    <>
      <PageHeader title={t('search.title')} subtitle={t('search.explain')} />
      <form
        className={styles.form}
        onSubmit={(e) => {
          e.preventDefault();
          const next = text.trim();
          if (next !== '') void navigate(searchUrl(next, kind));
        }}
      >
        <input
          type="search"
          name="q"
          className={`${controls.input} ${styles.input}`}
          aria-label={t('search.label')}
          placeholder={t('header.search')}
          value={text}
          onChange={(e) => {
            setText(e.target.value);
          }}
        />
        <button type="submit" className={controls.primary}>
          {t('search.submit')}
        </button>
      </form>
      {q !== '' && (
        <nav aria-label={t('search.kinds')} className={styles.kinds}>
          {[null, ...SEARCH_KINDS].map((k) => (
            <Link
              key={k ?? 'all'}
              to={searchUrl(q, k)}
              className={styles.kindLink}
              aria-current={k === kind ? 'page' : undefined}
            >
              {k === null ? t('search.kindAll') : kindLabel(t, k)}
            </Link>
          ))}
        </nav>
      )}
      {q === '' && <EmptyState icon="search" reason={t('search.prompt')} />}
      {q !== '' && !long && <EmptyState icon="search" live reason={t('search.tooShort', { n: MIN_QUERY_CHARS })} />}
      {long && <Results key={`${q}\n${kind ?? ''}`} q={q} kind={kind} />}
    </>
  );
}
