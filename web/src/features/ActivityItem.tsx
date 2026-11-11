// One event of the caller's activity (plan 15 MR 9), as /-/activity and
// Home's "recent updates" draw it: one plain sentence for its type (who did
// what), then where (the project, the merge request) and when (relative, the
// moment in the title). A type this screen does not know yet gets a general
// sentence (the API may add types); the raw type name is never shown.
import type { TFunction } from 'i18next';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import type { ActivityEvent } from '../api/activity';
import { formatDateTime, formatRelative } from './labels';
import { encodePath, pullUrl } from './repoUrls';
import styles from './ActivityItem.module.css';

/** Who did it: the display name, else the username; no actor (a system
 * event, a removed user) is "someone". */
function actorName(t: TFunction, event: ActivityEvent): string {
  if (event.actor === null) return t('activity.someone');
  if (event.actor.display_name.trim() !== '') return event.actor.display_name;
  return event.actor.username;
}

/** The sentence for an event: who did what, in plain words. */
export function activitySentence(t: TFunction, event: ActivityEvent): string {
  const actor = actorName(t, event);
  switch (event.type) {
    case 'project.created':
      return t('activity.projectCreated', { actor });
    case 'repository.pushed':
      return event.ref_count === null
        ? t('activity.pushedNoCount', { actor })
        : t('activity.pushed', { actor, count: event.ref_count });
    case 'merge_request.opened':
      return t('activity.mrOpened', { actor });
    case 'merge_request.closed':
      return t('activity.mrClosed', { actor });
    case 'merge_request.reopened':
      return t('activity.mrReopened', { actor });
    case 'merge_request.commented':
      return t('activity.mrCommented', { actor });
    case 'merge_request.merged':
      return t('activity.mrMerged', { actor });
    case 'merge_request.approved':
      return t('activity.mrApproved', { actor });
    default:
      return t('activity.other', { actor });
  }
}

export const projectUrl = (path: string) => `/${encodePath(path)}`;

/** A list item; `id` lets the page move focus to it after "load more". */
export function ActivityItem({ event, id }: { event: ActivityEvent; id?: string }) {
  const { t, i18n } = useTranslation();
  const mr = event.merge_request;
  const mrRef = mr === null ? '' : `!${String(mr.iid)}`;
  return (
    <li className={styles.item} {...(id === undefined ? {} : { id, tabIndex: -1 })}>
      <p className={styles.sentence}>{activitySentence(t, event)}</p>
      <p className={styles.meta}>
        {mr !== null && (
          <Link to={pullUrl(event.project_path, mr.iid)} className={styles.mr}>
            {mr.title === null || mr.title === '' ? mrRef : `${mrRef} ${mr.title}`}
          </Link>
        )}
        <Link to={projectUrl(event.project_path)} className={styles.project}>
          {event.project_path}
        </Link>
        <time
          dateTime={event.occurred_at}
          title={formatDateTime(event.occurred_at, i18n.language)}
          className={styles.time}
        >
          {formatRelative(event.occurred_at, i18n.language)}
        </time>
      </p>
    </li>
  );
}
