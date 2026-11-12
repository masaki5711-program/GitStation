// One merge request (`/-/pull/{iid}[/changes]`): the header, the one-line
// status (why it is stopped, what next), and the tabs Overview (description
// and threads) and Changes. The status line and the threads read on their
// own: a failure of one shows in its place only, with its request_id.
// The Overview has a side panel (MrSidePanel.tsx: approvals, checks,
// threads, versions at a glance). It comes first in the document, so on a
// narrow screen it follows the status line and the tabs, before the detail
// sections; on a wide one the grid puts it in the right-hand column. The
// Changes tab keeps the whole width for the diff.
// The Overview starts with the merge conditions panel (U2: every reason,
// approve / withdraw, merge), then the detail of the head's check runs and
// the conflicts predicted with other merge requests (CheckRuns.tsx).
import { useEffect, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link, useLocation } from 'react-router';

import { useAddComment, useDiscussions, useMergeRequest } from '../../../api/mergeRequests';
import type { Discussion, MergeRequest } from '../../../api/types';
import { Loading } from '../../../components/Loading';
import { QueryError } from '../../../components/QueryError';
import { formatDateTime, formatRelative, mrStateLabel } from '../../../features/labels';
import { MarkdownView } from '../../../features/MarkdownView';
import { CheckRunsSection, ConflictsSection } from '../../../features/mr/CheckRuns';
import { CommentForm } from '../../../features/mr/CommentForm';
import { ConditionsPanel } from '../../../features/mr/ConditionsPanel';
import { DiscussionThread, threadElementId } from '../../../features/mr/DiscussionThread';
import { MrChanges } from '../../../features/mr/MrChanges';
import { MrSidePanel } from '../../../features/mr/MrSidePanel';
import { Announcer } from '../../../features/mr/announce';
import { StateBadge, mrTone } from '../../../features/mr/StateBadge';
import { FOCUS_FIRST_UNRESOLVED, StatusLine } from '../../../features/mr/StatusLine';
import { useThreadToggle } from '../../../features/mr/useThreadToggle';
import { useViewedHead, ViewedHeadProvider } from '../../../features/mr/viewedHead';
import { pullUrl } from '../../../features/repoUrls';
import { useDocumentTitle } from '../../../hooks/useDocumentTitle';
import { RepoError } from '../common';
import repo from '../Repo.module.css';
import styles from './Mr.module.css';
import { PageHeader } from '../../../ui/PageHeader';

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

  // The same number as the side panel's "unresolved" (what is left to do).
  const unresolvedCount = threads.data?.discussions.filter((d) => !d.resolved).length ?? 0;
  const firstUnresolved = threads.data?.discussions.find((d) => !d.resolved);
  const focusFirstUnresolved =
    firstUnresolved === undefined
      ? null
      : () => {
          document.getElementById(threadElementId(firstUnresolved.id))?.focus();
        };

  return (
    <ViewedHeadProvider projectId={projectId} project={project} iid={iid} mr={mr.data}>
    <Announcer>
      <Header mr={mr.data} />
      {/* The primary action area: why it is stopped and what to press next. */}
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
              {unresolvedCount > 0 && (
                <>
                  <span className={styles.tabCount} aria-hidden="true" data-testid="mr-tab-count">
                    {unresolvedCount}
                  </span>
                  <span className={repo.srOnly}>{t('mr.view.threadCount', { n: unresolvedCount })}</span>
                </>
              )}
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
        <div className={styles.layout}>
          <div className={styles.sideColumn}>
            <MrSidePanel projectId={projectId} mr={mr.data} threads={threads} />
          </div>
          <div className={styles.mainColumn}>
            <Overview projectId={projectId} project={project} mr={mr.data} threads={threads} />
          </div>
        </div>
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
    </ViewedHeadProvider>
  );
}

/** Markers put into the sentence in place of the author and the time, so
 * both are rendered as React nodes (never parsed as markup), in the order
 * the language puts them. A private-use character (built at run time):
 * never in a dictionary string. */
const MARK = String.fromCharCode(0xe000);
const AUTHOR_MARK = `${MARK}author${MARK}`;
const WHEN_MARK = `${MARK}when${MARK}`;
const MARKS = new RegExp(`(${MARK}(?:author|when)${MARK})`, 'u');

/** "Opened {when} by {author}" with the markers → ['Opened ', WHEN_MARK, ' by ', AUTHOR_MARK]. */
export function openedParts(sentence: string): string[] {
  return sentence.split(MARKS).filter((p) => p !== '');
}

function Header({ mr }: { mr: MergeRequest }) {
  const { t, i18n } = useTranslation();
  return (
    <header className={styles.header}>
      <PageHeader
        title={
          <>
            {mr.title}{' '}
            <span className={`${repo.titleNote} ${styles.number}`}>{t('mr.number', { iid: mr.iid })}</span>
          </>
        }
      />
      <p className={styles.headerMeta}>
        <StateBadge tone={mrTone(mr.state)}>{mrStateLabel(t, mr.state)}</StateBadge>
        {mr.draft && <StateBadge tone="draft">{t('mr.draft')}</StateBadge>}
        {/* One sentence per language: the word order is the dictionary's. */}
        <span data-testid="mr-opened">
          {openedParts(t('mr.openedAgo', { author: AUTHOR_MARK, when: WHEN_MARK })).map((part, i) =>
            part === AUTHOR_MARK ? (
              <span key={i}>{mr.author.display_name}</span>
            ) : part === WHEN_MARK ? (
              <time
                key={i}
                className={styles.number}
                dateTime={mr.created_at}
                title={formatDateTime(mr.created_at, i18n.language)}
                data-testid="mr-opened-at"
              >
                {formatRelative(mr.created_at, i18n.language)}
              </time>
            ) : (
              part
            ),
          )}
        </span>
      </p>
      {/* "source → target": the words for a screen reader, the chips and
          the arrow (drawn by CSS) for the eye. */}
      <p className={styles.branchFlow} data-testid="mr-branches">
        <span className={repo.srOnly}>
          {t('mr.branches', { source: mr.source_branch, target: mr.target_branch })}
        </span>
        <span className={styles.branchFlowVisual} aria-hidden="true">
          <code className={styles.branchChip}>{mr.source_branch}</code>
          <span className={styles.branchArrow} />
          <code className={styles.branchChip}>{mr.target_branch}</code>
        </span>
      </p>
    </header>
  );
}

/** Nothing the person chose holds the focus (see Layout.tsx). */
function focusIsFree(): boolean {
  const active = document.activeElement;
  return (
    active === null ||
    active === document.body ||
    active.tagName === 'MAIN' ||
    active.tagName === 'H1'
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
  const viewed = useViewedHead();
  // The checks of the head on screen (the one approve and merge name).
  const head = viewed.known ? viewed.head : (mr.head_sha ?? null);
  const heading = useRef<HTMLHeadingElement>(null);
  const summary = useRef<HTMLElement>(null);
  const arrived = useRef<string | null>(null);
  /** After a resolve the thread moves under the summary; after a reopen, back. */
  const [focusAfter, setFocusAfter] = useState<{ id: number; resolved: boolean } | null>(null);
  const toggle = useThreadToggle(projectId, mr.iid, (id, resolved) => {
    setFocusAfter({ id, resolved });
  });
  const all = threads.data?.discussions ?? [];
  const open = all.filter((d) => !d.resolved);
  const done = all.filter((d) => d.resolved);

  // Arriving from "next" on another tab: the first unresolved thread, or the
  // threads' section when they could not be read. The guard is set when the
  // focus moves (inside the timer): StrictMode runs this effect twice and the
  // first run's timer is cleared. Only a free focus moves (the body, the main
  // area or a heading the layout focused, as Layout.tsx), so threads that
  // load late never take it from a field.
  useEffect(() => {
    if (!isFocusRequest(location.state) || arrived.current === location.key) return;
    if (threads.isPending) return;
    const first = threads.data?.discussions.find((d) => !d.resolved);
    // After the layout's own move to the h1 on a new address (its effect
    // runs after this one).
    const timer = window.setTimeout(() => {
      arrived.current = location.key;
      if (!focusIsFree()) return;
      const target =
        first === undefined ? null : document.getElementById(threadElementId(first.id));
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
      toggle={toggle}
    />
  );

  return (
    <>
      <ConditionsPanel projectId={projectId} iid={mr.iid} sourceBranch={mr.source_branch} />
      <CheckRunsSection projectId={projectId} project={project} iid={mr.iid} head={head} />
      <ConflictsSection projectId={projectId} project={project} iid={mr.iid} />
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
