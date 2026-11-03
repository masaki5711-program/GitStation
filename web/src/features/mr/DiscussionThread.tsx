// One thread of a merge request: its notes (Markdown, never raw HTML), the
// resolved state with the button that changes it, and a reply box opened on
// demand. An inline thread names its `path:line` and says when a later
// version changed that line (`outdated`) or when that could not be told
// (`outdated_unknown`: look again). The element takes the focus when the
// status line's "next" points here (`threadElementId`), and after a reply is
// sent; closing the reply box without sending puts it back on Reply. The
// article is described by its state and `path:line`.
import { useEffect, useId, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import { useReply } from '../../api/mergeRequests';
import type { Discussion } from '../../api/types';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { formatDateTime } from '../labels';
import { MarkdownView } from '../MarkdownView';
import { CommentForm } from './CommentForm';
import { StateBadge } from './StateBadge';
import type { ThreadToggle } from './useThreadToggle';
import styles from './Mr.module.css';

/** The DOM id of a thread (the status line's "next" focuses it). */
export const threadElementId = (id: number) => `mr-thread-${id}`;

export function DiscussionThread({
  projectId,
  iid,
  project,
  refName,
  discussion,
  headingLevel = 3,
  toggle,
}: {
  projectId: number;
  iid: number;
  project: string;
  /** The ref relative links in the notes resolve against (the target branch). */
  refName: string;
  discussion: Discussion;
  /** 3 on the Overview (under "Threads"); 4 inside a diff (under the file's h3). */
  headingLevel?: 3 | 4;
  /** Resolve / reopen, owned by the list (it outlives this thread's move). */
  toggle: ThreadToggle;
}) {
  const { t, i18n } = useTranslation();
  const headingId = useId();
  const stateId = useId();
  const positionId = useId();
  const article = useRef<HTMLElement>(null);
  const replyButton = useRef<HTMLButtonElement>(null);
  const [replying, setReplying] = useState(false);
  /** Where the focus goes once the reply box has closed. */
  const [focusAfter, setFocusAfter] = useState<'reply' | 'thread' | null>(null);
  useEffect(() => {
    if (replying || focusAfter === null) return;
    (focusAfter === 'reply' ? replyButton.current : article.current)?.focus();
    setFocusAfter(null);
  }, [replying, focusAfter]);
  const reply = useReply(projectId, iid);
  const togglePending = toggle.pendingId === discussion.id;
  const toggleFailed = toggle.failed?.id === discussion.id ? toggle.failed.error : null;
  const position = discussion.position;
  const Heading = headingLevel === 3 ? 'h3' : 'h4';

  return (
    <article
      ref={article}
      id={threadElementId(discussion.id)}
      tabIndex={-1}
      aria-labelledby={headingId}
      aria-describedby={position === null ? stateId : `${stateId} ${positionId}`}
      className={discussion.resolved ? `${styles.thread} ${styles.resolved}` : styles.thread}
      data-testid={`thread-${discussion.id}`}
    >
      <header className={styles.threadHead}>
        <Heading id={headingId} className={styles.threadTitle}>
          {t('mr.thread.title', { author: discussion.author.display_name })}
        </Heading>
        {position !== null && (
          <code id={positionId} className={styles.position}>
            {position.path}:{position.line}
          </code>
        )}
        <StateBadge id={stateId} tone={discussion.resolved ? 'resolved' : 'unresolved'}>
          {discussion.resolved ? t('mr.thread.resolved') : t('mr.thread.unresolved')}
        </StateBadge>
      </header>
      {(discussion.outdated || discussion.outdated_unknown) && (
        <p className={styles.outdated}>
          {discussion.outdated_unknown ? t('mr.thread.outdatedUnknown') : t('mr.thread.outdated')}
        </p>
      )}
      <ol className={styles.notes}>
        {discussion.notes.map((n) => (
          <li key={n.id} className={styles.note}>
            <p className={styles.noteMeta}>
              <span className={styles.noteAuthor}>{n.author.display_name}</span> {'·'}{' '}
              {formatDateTime(n.created_at, i18n.language)}
            </p>
            <div className={styles.noteBody}>
              <MarkdownView
                source={n.body}
                project={project}
                refName={refName}
                dir=""
                headingOffset={headingLevel}
              />
            </div>
          </li>
        ))}
      </ol>
      <div className={styles.threadActions}>
        {!replying && (
          <button
            ref={replyButton}
            type="button"
            className={controls.button}
            onClick={() => {
              setReplying(true);
            }}
          >
            {t('mr.thread.reply')}
          </button>
        )}
        <button
          type="button"
          className={controls.button}
          aria-disabled={togglePending ? 'true' : undefined}
          onClick={() => {
            toggle.toggle(discussion);
          }}
        >
          {discussion.resolved ? t('mr.thread.unresolve') : t('mr.thread.resolve')}
        </button>
      </div>
      {toggleFailed !== null && <QueryError error={toggleFailed} />}
      {replying && (
        <CommentForm
          label={t('mr.thread.replyLabel')}
          submitLabel={t('mr.thread.replySubmit')}
          pending={reply.isPending}
          error={reply.isError ? reply.error : null}
          autoFocus
          onCancel={() => {
            setReplying(false);
            setFocusAfter('reply');
          }}
          onSend={(body, done) => {
            reply.mutate(
              { discussionId: discussion.id, body },
              {
                onSuccess: () => {
                  done();
                  setReplying(false);
                  setFocusAfter('thread');
                },
              },
            );
          }}
        />
      )}
    </article>
  );
}
