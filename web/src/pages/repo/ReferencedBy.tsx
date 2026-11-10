// The "documents that refer to this one" panel of the document screen: the
// documents (and translations) that refer to this one, with the lines that
// do, a page at a time ("Show more" by `next_after`). A referrer the reader
// may not see is its id only (plan 13 Q14): the server sends no title, and
// the panel says why. When the server could not read every document, a
// notice says so and offers a reload; empty, loading and failure each say
// what to do next. The screen keys this panel by document and revision, so
// a new document starts with a fresh paging state.
import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { useReferrers } from '../../api/documents';
import type { Referrer, Referrers } from '../../api/types';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { documentUrl } from '../../features/repoUrls';
import { RepoError } from './common';
import styles from './Documents.module.css';

type DocIncomplete = NonNullable<Referrers['incomplete']>;

export function ReferencedBy({
  projectId,
  project,
  docId,
  keep,
}: {
  projectId: number;
  project: string;
  docId: string;
  keep: { ref?: string };
}) {
  const { t } = useTranslation();
  const referrers = useReferrers(projectId, docId, keep.ref);
  const list = useRef<HTMLUListElement>(null);
  const before = useRef<number | null>(null);
  // The live region's text and a counter: a new counter re-mounts the text,
  // so the same words ("50 more", then "50 more") are announced again.
  const [announce, setAnnounce] = useState({ text: '', n: 0 });
  const count = referrers.data?.pages.reduce((n, p) => n + p.referrers.length, 0) ?? 0;
  // After "Show more" the focus goes to the first new referrer, and the
  // live region says how many came (the button may be gone).
  useEffect(() => {
    const from = before.current;
    if (from === null || count <= from) return;
    const item = list.current?.querySelectorAll(':scope > li')[from];
    (item?.querySelector('a') ?? item?.querySelector('code'))?.focus();
    setAnnounce((a) => ({ text: t('docs.referrers.loaded', { count: count - from }), n: a.n + 1 }));
    before.current = null;
  }, [count, t]);
  // A failed "Show more" moves nothing: a later success must not steal the
  // focus for a click the reader no longer waits on.
  useEffect(() => {
    if (referrers.errorUpdatedAt > 0) before.current = null;
  }, [referrers.errorUpdatedAt]);

  const reload = () => {
    before.current = null;
    void referrers.refetch();
  };

  let body;
  if (referrers.isPending) {
    body = <Loading />;
  } else if (referrers.isError && referrers.data === undefined) {
    body = <RepoError error={referrers.error} onRetry={reload} />;
  } else {
    const pages = referrers.data.pages;
    const first = pages[0];
    const all = pages.flatMap((p) => p.referrers);
    const incomplete = pages.map((p) => p.incomplete).find((i): i is DocIncomplete => i != null);
    body = (
      <>
        {incomplete !== undefined && (
          <div className={styles.warning} role="status" data-testid="referrers-incomplete">
            <p className={styles.warningText}>
              {all.length === 0
                ? t('docs.referrers.incompleteEmpty')
                : t('docs.referrers.incomplete')}{' '}
              <span className={styles.codeSuffix}>
                {t('docs.referrers.code', { code: incomplete.error_code })}
              </span>
            </p>
            <button
              type="button"
              className={`${controls.button} ${styles.busy}`}
              aria-disabled={referrers.isFetching ? 'true' : undefined}
              onClick={() => {
                if (referrers.isFetching) return;
                reload();
              }}
            >
              {referrers.isFetching ? t('common.loading') : t('docs.referrers.reload')}
            </button>
          </div>
        )}
        {all.length === 0 ? (
          incomplete === undefined && (
            <p data-testid="referrers-empty">{t('docs.referrers.empty', { id: docId })}</p>
          )
        ) : (
          <>
            {first !== undefined && first.total !== null && (
              <p className={styles.meta}>{t('docs.referrers.total', { count: first.total })}</p>
            )}
            <ul className={styles.refs} ref={list}>
              {all.map((r) => (
                <ReferrerItem
                  key={r.lang === null ? r.id : `${r.id}.${r.lang}`}
                  r={r}
                  project={project}
                  keep={keep}
                />
              ))}
            </ul>
          </>
        )}
        <p role="status" className={styles.srOnly} data-testid="referrers-announce">
          <span key={announce.n}>{announce.text}</span>
        </p>
        {referrers.isError && (
          <QueryError
            error={referrers.error}
            onRetry={() => {
              before.current = all.length;
              void referrers.fetchNextPage();
            }}
          />
        )}
        {referrers.hasNextPage && !referrers.isError && (
          <p>
            <button
              type="button"
              className={`${controls.button} ${styles.busy}`}
              aria-disabled={referrers.isFetchingNextPage ? 'true' : undefined}
              onClick={() => {
                if (referrers.isFetchingNextPage) return;
                before.current = all.length;
                void referrers.fetchNextPage();
              }}
            >
              {referrers.isFetchingNextPage ? t('common.loading') : t('docs.referrers.more')}
            </button>
          </p>
        )}
      </>
    );
  }

  return (
    <section aria-labelledby="doc-referrers" className={styles.section} data-testid="referrers">
      <h2 id="doc-referrers">{t('docs.referrers.title')}</h2>
      {body}
    </section>
  );
}

function LangBadge({ lang }: { lang: string | null }) {
  const { t } = useTranslation();
  if (lang === null) return null;
  return (
    <>
      {' '}
      <span className={styles.langBadge}>{t('docs.referrers.translation', { lang })}</span>
    </>
  );
}

function ReferrerItem({
  r,
  project,
  keep,
}: {
  r: Referrer;
  project: string;
  keep: { ref?: string };
}) {
  const { t } = useTranslation();
  if (r.hidden) {
    return (
      <li data-hidden="true">
        <code tabIndex={-1}>{r.id}</code>
        <LangBadge lang={r.lang} />{' '}
        <span className={styles.refNote}>{t('docs.referrers.hidden')}</span>
      </li>
    );
  }
  const to = documentUrl(project, r.id, { ...keep, ...(r.lang === null ? {} : { lang: r.lang }) });
  return (
    <li>
      <Link to={to}>
        <code>{r.id}</code>
        {r.title !== null && (
          <>
            {' '}
            <span className={styles.refTitle}>{r.title}</span>
          </>
        )}
      </Link>
      <LangBadge lang={r.lang} />
      {r.refs.length > 0 && (
        <ul className={styles.refLines}>
          {r.refs.map((x, i) => (
            <li key={`${x.line}-${x.block ?? ''}-${i}`} className={styles.refLine}>
              {x.block === null
                ? t('docs.referrers.line', { line: x.line })
                : t('docs.referrers.lineBlock', { line: x.line, block: x.block })}
            </li>
          ))}
        </ul>
      )}
    </li>
  );
}
