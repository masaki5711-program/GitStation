// One merge request (`/-/pull/{iid}[/changes]`): the header, the one-line
// status (why it is stopped, what next), and the tabs Overview (description
// and threads) and Changes. The status line and the threads read on their
// own: a failure of one shows in its place only, with its request_id.
// Approve and merge are U2.
import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useLocation } from 'react-router';

import { useAddComment, useDiscussions, useMergeRequest } from '../../../api/mergeRequests';
import type { Discussion, MergeRequest } from '../../../api/types';
import { Loading } from '../../../components/Loading';
import { QueryError } from '../../../components/QueryError';
import { formatDate, formatDateTime, mrStateLabel } from '../../../features/labels';
import { MarkdownView } from '../../../features/MarkdownView';
import { CommentForm } from '../../../features/mr/CommentForm';
import { DiscussionThread, threadElementId } from '../../../features/mr/DiscussionThread';
import { MrChanges } from '../../../features/mr/MrChanges';
import { Announcer } from '../../../features/mr/announce';
import { StateBadge, mrTone } from '../../../features/mr/StateBadge';
import { FOCUS_FIRST_UNRESOLVED, StatusLine } from '../../../features/mr/StatusLine';
import { pullUrl } from '../../../features/repoUrls';
import { useDocumentTitle } from '../../../hooks/useDocumentTitle';
import { RepoError } from '../common';
import repo from '../Repo.module.css';
import styles from './Mr.module.css';

export type MrTab = 'overview' | 'changes';

export function MergeRequestScreen({
  projectId,
  project,
  iid,
  tab,
}: {
  projectId: number;
  project: string;
  /** Parsed once from the address (`parseIid`); never an invalid number. */
  iid: number;
  tab: MrTab;
}) {
  const { t } = useTranslation();
  const mr = useMergeRequest(projectId, iid);
  const threads = useDiscussions(projectId, iid);
  useDocumentTitle(mr.data === undefined ? null : `${mr.data.title} ${t('mr.number', { iid })}`);

  if (mr.isPending) return <Loading />;
  if (mr.isError) {
    return (
      <RepoError
        error={mr.error}
        onRetry={() => {
          void mr.refetch();
        }}
      />
    );
  }

  const firstUnresolved = threads.data?.discussions.find((d) => !d.resolved);
  const focusFirstUnresolved =
    firstUnresolved === undefined
      ? null
      : () => {
          document.getElementById(threadElementId(firstUnresolved.id))?.focus();
        };

  return (
    <Announcer>
      <Header mr={mr.data} />
      <StatusLine
        projectId={projectId}
        project={project}
        iid={iid}
        onFirstUnresolved={tab === 'overview' ? focusFirstUnresolved : null}
      />
      <nav aria-label={t('mr.view.tabsLabel')} className={styles.filter}>
        <ul className={styles.filterList}>
          <li>
            <Link
              to={pullUrl(project, iid)}
              className={styles.filterLink}
              {...(tab === 'overview' ? { 'aria-current': 'page' as const } : {})}
            >
              {t('mr.view.overview')}
            </Link>
          </li>
          <li>
            <Link
              to={pullUrl(project, iid, 'changes')}
              className={styles.filterLink}
              {...(tab === 'changes' ? { 'aria-current': 'page' as const } : {})}
            >
              {t('mr.view.changes')}
            </Link>
          </li>
        </ul>
      </nav>
      {tab === 'overview' ? (
        <Overview projectId={projectId} project={project} mr={mr.data} threads={threads} />
      ) : (
        <section aria-labelledby="mr-changes-title">
          <h2 id="mr-changes-title" className={repo.sectionTitle}>
            {t('mr.view.changes')}
          </h2>
          <MrChanges
            projectId={projectId}
            project={project}
            iid={iid}
            targetBranch={mr.data.target_branch}
          />
        </section>
      )}
    </Announcer>
  );
}

function Header({ mr }: { mr: MergeRequest }) {
  const { t, i18n } = useTranslation();
  return (
    <header>
      <h1 className={repo.title}>
        {mr.title}{' '}
        <span className={`${repo.titleNote} ${styles.number}`}>{t('mr.number', { iid: mr.iid })}</span>
      </h1>
      <p className={repo.meta}>
        <StateBadge tone={mrTone(mr.state)}>{mrStateLabel(t, mr.state)}</StateBadge>
        {mr.draft && <StateBadge tone="draft">{t('mr.draft')}</StateBadge>}
        <span title={formatDateTime(mr.created_at, i18n.language)}>
          {t('mr.opened', {
            author: mr.author.display_name,
            date: formatDate(mr.created_at, i18n.language),
          })}
        </span>
        <span>{t('mr.branches', { source: mr.source_branch, target: mr.target_branch })}</span>
      </p>
    </header>
  );
}

function isFocusRequest(state: unknown): boolean {
  return (
    typeof state === 'object' &&
    state !== null &&
    'focus' in state &&
    state.focus === FOCUS_FIRST_UNRESOLVED.focus
  );
}

function Overview({
  projectId,
  project,
  mr,
  threads,
}: {
  projectId: number;
  project: string;
  mr: MergeRequest;
  threads: ReturnType<typeof useDiscussions>;
}) {
  const { t } = useTranslation();
  const location = useLocation();
  const comment = useAddComment(projectId, mr.iid);
  const heading = useRef<HTMLHeadingElement>(null);
  const summary = useRef<HTMLElement>(null);
  const arrived = useRef<string | null>(null);
  /** After a resolve the thread moves under the summary; after a reopen, back. */
  const [focusAfter, setFocusAfter] = useState<{ id: number; resolved: boolean } | null>(null);
  const all = threads.data?.discussions ?? [];
  const open = all.filter((d) => !d.resolved);
  const done = all.filter((d) => d.resolved);

  // Arriving from "next" on another tab: the first unresolved thread, or the
  // threads' section when they could not be read.
  useEffect(() => {
    if (!isFocusRequest(location.state) || arrived.current === location.key) return;
    if (threads.isPending) return;
    arrived.current = location.key;
    const first = threads.data?.discussions.find((d) => !d.resolved);
    const target = first === undefined ? null : document.getElementById(threadElementId(first.id));
    // After the layout's own move to the h1 on a new address (its effect
    // runs after this one).
    const timer = window.setTimeout(() => {
      (target ?? heading.current)?.focus();
    }, 0);
    return () => {
      window.clearTimeout(timer);
    };
  }, [location, threads.isPending, threads.data]);

  // Once the list shows the thread in its new place (the refetch may render
  // after the mutation's callback).
  useEffect(() => {
    if (focusAfter === null) return;
    const now = threads.data?.discussions.find((d) => d.id === focusAfter.id);
    if (now === undefined || now.resolved !== focusAfter.resolved) return;
    if (focusAfter.resolved) summary.current?.focus();
    else document.getElementById(threadElementId(focusAfter.id))?.focus();
    setFocusAfter(null);
  }, [focusAfter, threads.data]);

  const thread = (d: Discussion) => (
    <DiscussionThread
      key={d.id}
      projectId={projectId}
      iid={mr.iid}
      project={project}
      refName={mr.target_branch}
      discussion={d}
      onToggled={(id, resolved) => {
        setFocusAfter({ id, resolved });
      }}
    />
  );

  return (
    <>
      <section aria-labelledby="mr-description-title">
        <h2 id="mr-description-title" className={repo.sectionTitle}>
          {t('mr.view.description')}
        </h2>
        {mr.description.trim() === '' ? (
          <p className={repo.hint}>{t('mr.view.noDescription')}</p>
        ) : (
          <MarkdownView
            source={mr.description}
            project={project}
            refName={mr.target_branch}
            dir=""
            headingOffset={2}
          />
        )}
      </section>
      <section aria-labelledby="mr-threads-title">
        <h2 id="mr-threads-title" ref={heading} tabIndex={-1} className={repo.sectionTitle}>
          {t('mr.threads.title')}
        </h2>
        {threads.isPending && <Loading />}
        {threads.isError && (
          <QueryError
            error={threads.error}
            onRetry={() => {
              void threads.refetch();
            }}
          />
        )}
        {threads.isSuccess && (
          <>
            {threads.data.truncated && (
              <p className={repo.warning} role="status" data-testid="mr-threads-truncated">
                {t('mr.threads.truncated')}
              </p>
            )}
            {all.length === 0 && <p className={repo.hint}>{t('mr.threads.empty')}</p>}
            {/* Unresolved first: they are what is left to do. */}
            {open.map(thread)}
            {done.length > 0 && (
              <details className={styles.resolvedGroup}>
                <summary ref={summary} className={styles.resolvedSummary}>
                  {t('mr.threads.resolvedCount', { n: done.length })}
                </summary>
                {done.map(thread)}
              </details>
            )}
          </>
        )}
        <CommentForm
          label={t('mr.comment.label')}
          submitLabel={t('mr.comment.submit')}
          pending={comment.isPending}
          error={comment.isError ? comment.error : null}
          onSend={(body, sent) => {
            comment.mutate({ body }, { onSuccess: sent });
          }}
        />
      </section>
    </>
  );
}
