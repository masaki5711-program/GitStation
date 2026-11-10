// The "referenced by" panel of the document screen: the documents (and
// translations) that refer to this one, with the lines that do, a page at a
// time ("Show more" by `next_after`). A more confidential referrer is its id
// only (plan 13 Q14): the server sends no title, and the panel says why.
// When the server could not read every document, a notice says a referrer
// may be missing; empty, loading and failure each say what to do next.
import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { useReferrers } from '../../api/documents';
import type { Referrer, Referrers } from '../../api/types';
import { Loading } from '../../components/Loading';
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
  const [announce, setAnnounce] = useState('');
  const count = referrers.data?.pages.reduce((n, p) => n + p.referrers.length, 0) ?? 0;
  // After "Show more" the focus goes to the first new referrer, and the
  // live region says how many came (the button may be gone).
  useEffect(() => {
    const from = before.current;
    if (from === null || count <= from) return;
    const item = list.current?.querySelectorAll(':scope > li')[from];
    (item?.querySelector('a') ?? item?.querySelector('code'))?.focus();
    setAnnounce(t('docs.referrers.loaded', { count: count - from }));
    before.current = null;
  }, [count, t]);

  let body;
  if (referrers.isPending) {
    body = <Loading />;
  } else if (referrers.isError && referrers.data === undefined) {
    body = (
      <RepoError
        error={referrers.error}
        onRetry={() => {
          void referrers.refetch();
        }}
      />
    );
  } else {
    const pages = referrers.data.pages;
    const first = pages[0];
    const all = pages.flatMap((p) => p.referrers);
    const incomplete = pages.map((p) => p.incomplete).find((i): i is DocIncomplete => i != null);
    body = (
      <>
        {incomplete !== undefined && (
          <p className={styles.warning} role="status" data-testid="referrers-incomplete">
            {t('docs.referrers.incomplete', { code: incomplete.error_code })}
          </p>
        )}
        {all.length === 0 ? (
          <p data-testid="referrers-empty">{t('docs.referrers.empty', { id: docId })}</p>
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
        <p role="status" className={styles.srOnly}>
          {announce}
        </p>
        {referrers.isError && (
          <RepoError
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
              className={controls.button}
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
        <code tabIndex={-1}>{r.id}</code> <span className={styles.refNote}>{t('docs.ref.hidden')}</span>
      </li>
    );
  }
  const to = documentUrl(project, r.id, { ...keep, ...(r.lang === null ? {} : { lang: r.lang }) });
  return (
    <li>
      <Link to={to}>
        {r.id} {r.title ?? ''}
      </Link>
      {r.lang !== null && (
        <>
          {' '}
          <span className={styles.langBadge}>
            {t('docs.referrers.translation', { lang: r.lang })}
          </span>
        </>
      )}
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
