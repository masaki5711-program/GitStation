// The approval's facts, made by code and always listed in full (plan 19
// §3.4): the range since the environment's last succeeded deployment, the
// merge requests, migrations, protected paths, CI and deploy configuration,
// permission code, forge/risk, failed checks and freeze windows. Shown
// whatever the AI summary says or whether there is one. When git could not
// read the range, the lists say "unknown", never "none".
import type { ReactNode } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import type { ApprovalFacts as Facts } from '../../../api/deployments';
import { listState } from '../../../features/deployApproval';
import { commitUrl, compareUrl, pullUrl } from '../../../features/repoUrls';
import styles from './Deploy.module.css';

function FactList<T>({
  facts,
  items,
  render,
  testId,
}: {
  facts: Facts;
  items: readonly T[];
  render: (item: T) => ReactNode;
  testId: string;
}) {
  const { t } = useTranslation();
  const state = listState(facts, items);
  if (state === 'unknown') {
    return (
      <span className={styles.unknown} data-testid={testId} data-state="unknown">
        {t('deploy.facts.unknown')}
      </span>
    );
  }
  if (state === 'none') {
    return (
      <span data-testid={testId} data-state="none">
        {t('deploy.facts.none')}
      </span>
    );
  }
  return (
    <ul data-testid={testId} data-state="listed">
      {items.map((item, i) => (
        // The lists are the server's, in its order, and never edited here.
        <li key={String(i)}>{render(item)}</li>
      ))}
    </ul>
  );
}

export function ApprovalFacts({ project, sha, facts }: { project: string; sha: string; facts: Facts }) {
  const { t } = useTranslation();
  const base = facts.base_sha ?? null;
  return (
    <section aria-labelledby="deploy-facts-title" data-testid="deploy-facts">
      <h2 id="deploy-facts-title" className={styles.sectionTitle}>
        {t('deploy.facts.title')}
      </h2>
      <p className={styles.hint}>{t('deploy.facts.lead')}</p>
      {facts.range === 'unavailable' && (
        <p className={styles.warning} role="note" data-testid="facts-range-unavailable">
          {t('deploy.facts.rangeUnavailable', { code: facts.range_error_code ?? '' })}
        </p>
      )}
      {(facts.lists_truncated || facts.commits_truncated) && (
        <p className={styles.warning} role="note" data-testid="facts-truncated">
          {t('deploy.facts.truncated')}
        </p>
      )}
      <dl className={styles.facts}>
        <dt>{t('deploy.facts.range')}</dt>
        <dd data-testid="facts-range">
          {facts.range === 'first_deployment' && t('deploy.facts.firstDeployment')}
          {facts.range === 'read' && base !== null && (
            <>
              {t('deploy.facts.since', { count: facts.commits })}{' '}
              <Link to={compareUrl(project, base, sha)}>{t('deploy.facts.openCompare')}</Link>{' '}
              <Link to={commitUrl(project, base)} aria-label={t('deploy.facts.openBase', { sha: base.slice(0, 8) })}>
                <code>{base.slice(0, 8)}</code>
              </Link>
            </>
          )}
          {facts.range === 'read' && base === null && t('deploy.facts.since', { count: facts.commits })}
          {facts.range === 'unavailable' && <span className={styles.unknown}>{t('deploy.facts.unknown')}</span>}
        </dd>

        <dt>{t('deploy.facts.risk')}</dt>
        <dd data-testid="facts-risk">
          {facts.risk == null ? t('deploy.facts.notAssessed') : <code>{facts.risk}</code>}
        </dd>

        <dt>{t('deploy.facts.mergeRequests')}</dt>
        <dd>
          <FactList
            facts={facts}
            items={facts.merge_requests}
            testId="facts-mrs"
            render={(m) => (
              <>
                <Link to={pullUrl(project, m.iid)}>{t('deploy.facts.mr', { iid: m.iid })}</Link> {m.title}
                {m.risk != null && (
                  <>
                    {' '}
                    <code>{t('deploy.facts.mrRisk', { risk: m.risk })}</code>
                  </>
                )}
              </>
            )}
          />
        </dd>

        <dt>{t('deploy.facts.migrations')}</dt>
        <dd>
          <FactList facts={facts} items={facts.migrations} testId="facts-migrations" render={(m) => <code>{m}</code>} />
        </dd>

        <dt>{t('deploy.facts.protectedPaths')}</dt>
        <dd>
          <FactList
            facts={facts}
            items={facts.protected_paths}
            testId="facts-protected"
            render={(p) => (
              <>
                <code>{p.path}</code> {t('deploy.facts.rule', { rule: p.rule })}
              </>
            )}
          />
        </dd>

        <dt>{t('deploy.facts.ciConfig')}</dt>
        <dd>
          <FactList facts={facts} items={facts.ci_config} testId="facts-ci" render={(p) => <code>{p}</code>} />
        </dd>

        <dt>{t('deploy.facts.permissions')}</dt>
        <dd>
          <FactList facts={facts} items={facts.permissions} testId="facts-permissions" render={(p) => <code>{p}</code>} />
        </dd>

        <dt>{t('deploy.facts.failedChecks')}</dt>
        <dd>
          {/* The checks are of the commit itself: known even without the range. */}
          {facts.failed_checks.length === 0 ? (
            <span data-testid="facts-checks" data-state="none">
              {t('deploy.facts.none')}
            </span>
          ) : (
            <ul data-testid="facts-checks" data-state="listed">
              {facts.failed_checks.map((c) => (
                <li key={c.name}>
                  <code>{c.name}</code> {t('deploy.facts.conclusion', { conclusion: c.conclusion })}
                </li>
              ))}
            </ul>
          )}
        </dd>

        <dt>{t('deploy.facts.freeze')}</dt>
        <dd data-testid="facts-freeze">
          {facts.frozen_now && <strong>{t('deploy.facts.frozenNow')}</strong>}
          {facts.freeze_windows.length === 0 ? (
            !facts.frozen_now && t('deploy.facts.none')
          ) : (
            <ul>
              {facts.freeze_windows.map((w, i) => (
                <li key={`${w}-${String(i)}`}>
                  <code>{w}</code>
                </li>
              ))}
            </ul>
          )}
        </dd>
      </dl>
    </section>
  );
}
