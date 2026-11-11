// One failed job of the diagnosis (plan 04 P-9): the masked excerpt
// (focusable, so a keyboard can scroll it), how often it happened, the merge
// requests that fixed the same failure, and its log. Error codes stay in
// the logs (CLAUDE.md contract 3).
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import type { CiFailure } from '../../../api/pipelines';
import { formatDateTime, formatRelative } from '../../../features/labels';
import { pipelineUrl, pullUrl } from '../../../features/repoUrls';
import styles from './Pipelines.module.css';

export function FailureView({ f, project, pipelineId }: { f: CiFailure; project: string; pipelineId: number }) {
  const { t, i18n } = useTranslation();
  return (
    <article className={styles.failure} data-testid="pipeline-failure">
      <h3>{t('pipelines.diagnosis.job', { job: f.job, stage: f.stage })}</h3>
      {f.pending ? (
        <p className={styles.hint}>{t('pipelines.diagnosis.pending')}</p>
      ) : (
        <>
          {f.excerpt !== '' && (
            <pre className={styles.log} tabIndex={0} aria-label={t('pipelines.diagnosis.excerpt', { job: f.job })}>
              {f.excerpt}
            </pre>
          )}
          {f.occurrences > 1 && (
            <p
              className={styles.hint}
              {...(f.first_seen_at === null ? {} : { title: formatDateTime(f.first_seen_at, i18n.language) })}
            >
              {f.first_seen_at === null
                ? t('pipelines.diagnosis.seen', { count: f.occurrences })
                : t('pipelines.diagnosis.seenSince', {
                    count: f.occurrences,
                    date: formatRelative(f.first_seen_at, i18n.language),
                  })}
            </p>
          )}
          {f.known_fixes.length > 0 && (
            <>
              <p>{t('pipelines.diagnosis.knownFixes')}</p>
              <ul className={styles.problems}>
                {f.known_fixes.map((k) => (
                  <li key={k.merge_request_id}>
                    <Link to={pullUrl(project, k.iid)}>
                      {t('pipelines.diagnosis.fix', { iid: k.iid, title: k.title })}
                    </Link>
                  </li>
                ))}
              </ul>
            </>
          )}
        </>
      )}
      <p>
        <Link to={pipelineUrl(project, pipelineId, f.job_id)}>
          {t('pipelines.diagnosis.openLog', { job: f.job })}
        </Link>
      </p>
    </article>
  );
}
