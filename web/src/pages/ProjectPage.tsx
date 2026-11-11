// The project top page (plan 10 §4.3): summary, the latest commit of the
// default branch, clone URL, the files with their README and, beside them
// (below on a narrow screen), the project's facts: default branch, branch and
// tag counts, links to the README and the merge requests. An empty
// repository says how to make the first push. The project's screens are in
// the sidebar.
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { ApiFailure, call } from '../api/client';
import { refParam, useSpec, useTree } from '../api/repo';
import {
  blobUrl,
  branchesUrl,
  commitUrl,
  isReadmeName,
  pullsUrl,
  tagsUrl,
} from '../features/repoUrls';
import { TreeListing } from './repo/TreeListing';
import { shortOid } from './repo/common';
import { Loading } from '../components/Loading';
import { useApi } from '../api/context';
import { Breadcrumb } from '../components/Breadcrumb';
import controls from '../components/controls.module.css';
import { CopyButton } from '../components/CopyButton';
import { MovedNotice } from '../components/MovedNotice';
import { QueryError } from '../components/QueryError';
import { formatDate, formatDateTime, formatRelative, roleLabel, visibilityLabel } from '../features/labels';
import { useDocumentTitle } from '../hooks/useDocumentTitle';
import { rememberProject } from '../hooks/recent';
import styles from './Namespace.module.css';

/** A new project's repository is created by a worker job (FG-REPO-0016
 * answers 503 until it is there, normally within seconds): keep asking for
 * about half a minute instead of showing an error right after creating. */
const PROVISIONING_RETRIES = 30;
const PROVISIONING_DELAY_MS = 1000;

function isNotReady(error: unknown): boolean {
  return error instanceof ApiFailure && error.failure.kind === 'api' && error.failure.status === 503;
}

/** The refs, waiting while a new repository is provisioned. One query (the
 * repository screens' key) for the state, the commit strip and the panel. */
function useProjectRefs(id: number) {
  const api = useApi();
  // Any other failure keeps the app's retry policy.
  const fallback = useQueryClient().getDefaultOptions().queries?.retry;
  return useQuery({
    queryKey: ['refs', id],
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/repository/refs', {
          signal,
          params: { path: { id } },
        }),
      ),
    retry: (failureCount, error) => {
      if (isNotReady(error)) return failureCount < PROVISIONING_RETRIES;
      if (typeof fallback === 'function') return fallback(failureCount, error);
      if (typeof fallback === 'number') return failureCount < fallback;
      return fallback ?? false;
    },
    retryDelay: PROVISIONING_DELAY_MS,
  });
}

/** The default branch's newest commit on one line. A failure stays here
 * (with its request id); the rest of the page does not depend on it. */
function LatestCommit({ id, path, branch }: { id: number; path: string; branch: string }) {
  const { t, i18n } = useTranslation();
  const api = useApi();
  const ref = `refs/heads/${branch}`;
  const latest = useQuery({
    queryKey: ['commits-latest', id, ref],
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}/repository/commits', {
          signal,
          params: { path: { id }, query: { ref, limit: 1 } },
        }),
      ),
  });
  if (latest.isPending) return <Loading />;
  if (latest.isError) {
    return (
      <QueryError
        error={latest.error}
        onRetry={() => {
          void latest.refetch();
        }}
      />
    );
  }
  const commit = latest.data.commits[0];
  if (commit === undefined) return null;
  const sha = shortOid(commit.oid);
  return (
    <section className={styles.commitStrip} aria-labelledby="latest-commit">
      <h2 id="latest-commit" className={styles.srOnly}>
        {t('project.latestCommit')}
      </h2>
      <p className={styles.commitSubject}>{commit.subject}</p>
      <p className={styles.commitMeta}>
        <span>{commit.author_name}</span>
        <time dateTime={commit.authored_at} title={formatDateTime(commit.authored_at, i18n.language)}>
          {formatRelative(commit.authored_at, i18n.language)}
        </time>
        <Link to={commitUrl(path, commit.oid)} aria-label={t('project.openCommit', { sha })}>
          <code>{sha}</code>
        </Link>
      </p>
    </section>
  );
}

function RepositoryState({
  id,
  name,
  path,
  cloneUrl,
}: {
  id: number;
  name: string;
  path: string;
  cloneUrl: string;
}) {
  const { t } = useTranslation();
  const refs = useProjectRefs(id);

  if (refs.isPending) {
    // The UI sees no error code (contract 3): on this route a 503 is the
    // provisioning answer; a real outage shows its error after the wait.
    if (!isNotReady(refs.failureReason)) return <Loading />;
    return (
      <p role="status" className={styles.hint}>
        {t('project.preparing')}
      </p>
    );
  }
  if (refs.isError) {
    return (
      <QueryError
        error={refs.error}
        onRetry={() => {
          void refs.refetch();
        }}
      />
    );
  }
  const { default_branch: branch, refs: list } = refs.data;
  const commands = t('project.emptyCommands', { url: cloneUrl, name });
  if (branch === null && list.length === 0) {
    return (
      <section className={styles.card} aria-labelledby="repo-empty">
        <h2 id="repo-empty">{t('project.emptyTitle')}</h2>
        <p>{t('project.emptyBody')}</p>
        <pre className={styles.pre}>
          <code>{commands}</code>
        </pre>
        <CopyButton text={commands} label={t('project.copyCommands')} />
      </section>
    );
  }
  if (branch === null) return null;
  return <LatestCommit id={id} path={path} branch={branch} />;
}

/** The README at the default branch's root, if any: the same tree query as
 * the file list below, so no second request. */
function useReadmeUrl(projectId: number, project: string): string | null {
  const spec = useSpec(projectId, '');
  const rev = spec.data?.rev;
  const tree = useTree(projectId, rev === undefined ? '' : refParam(rev), '', rev !== undefined);
  if (rev === undefined || tree.data === undefined) return null;
  const readme = tree.data.entries.find((e) => e.type === 'blob' && isReadmeName(e.name));
  return readme === undefined ? null : blobUrl(project, rev.name, readme.path);
}

/** The project's facts and shortcuts, beside the files. */
function ProjectInfo({ id, path, created }: { id: number; path: string; created: string }) {
  const { t, i18n } = useTranslation();
  const refs = useProjectRefs(id);
  const readme = useReadmeUrl(id, path);
  const data = refs.data;
  const branches = data?.refs.filter((r) => r.type === 'branch').length;
  const tags = data?.refs.filter((r) => r.type === 'tag').length;
  return (
    <aside className={styles.info} aria-labelledby="project-info">
      <h2 id="project-info">{t('project.info')}</h2>
      <dl className={styles.facts}>
        {data !== undefined && data.default_branch !== null && (
          <>
            <dt>{t('project.defaultBranch')}</dt>
            <dd>
              <code>{data.default_branch}</code>
            </dd>
          </>
        )}
        {branches !== undefined && tags !== undefined && (
          <>
            <dt>{t('project.branches')}</dt>
            <dd>
              <Link to={branchesUrl(path)}>{branches}</Link>
            </dd>
            <dt>{t('project.tags')}</dt>
            <dd>
              <Link to={tagsUrl(path)}>{tags}</Link>
            </dd>
          </>
        )}
        <dt>{t('project.created')}</dt>
        <dd>{formatDate(created, i18n.language)}</dd>
      </dl>
      {data?.truncated === true && <p className={styles.hint}>{t('project.refsTruncated')}</p>}
      <ul className={styles.infoLinks}>
        {readme !== null && (
          <li>
            <Link to={readme}>{t('project.readme')}</Link>
          </li>
        )}
        <li>
          <Link to={pullsUrl(path)}>{t('project.mergeRequests')}</Link>
        </li>
      </ul>
    </aside>
  );
}

/** The root of the default branch, with its README, under the summary. */
function ProjectFiles({ projectId, project }: { projectId: number; project: string }) {
  const spec = useSpec(projectId, '');
  if (spec.data === undefined) return null;
  return <TreeListing projectId={projectId} project={project} spec={spec.data} />;
}

export function ProjectPage({ id }: { id: number }) {
  const { t } = useTranslation();
  const api = useApi();
  const project = useQuery({
    queryKey: ['project', id],
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/projects/{id}', { signal, params: { path: { id } } })),
  });
  const data = project.data;
  useDocumentTitle(data?.display_name ?? null);
  useEffect(() => {
    if (data !== undefined) rememberProject({ path: data.path, name: data.display_name });
  }, [data]);

  if (project.isPending) return <Loading />;
  if (project.isError) {
    return (
      <QueryError
        error={project.error}
        onRetry={() => {
          void project.refetch();
        }}
      />
    );
  }
  const p = project.data;
  const cloneUrl = `${window.location.origin}/${p.path}.git`;
  const role = roleLabel(t, p.role);
  return (
    <>
      <Breadcrumb path={p.path} />
      <MovedNotice />
      <h1 className={styles.title}>{p.display_name}</h1>
      <p className={styles.meta}>
        <span className={controls.badge}>{visibilityLabel(t, p.visibility)}</span>
        {role !== null && <span className={controls.badge}>{role}</span>}
      </p>
      {p.description !== '' && <p>{p.description}</p>}

      <div className={styles.projectGrid}>
        <div className={styles.projectMain}>
          <RepositoryState id={p.id} name={p.name} path={p.path} cloneUrl={cloneUrl} />
          <section className={styles.card} aria-labelledby="clone-title">
            <h2 id="clone-title">{t('project.clone')}</h2>
            <div className={styles.cloneRow}>
              <code className={styles.clone} data-testid="clone-url">
                {cloneUrl}
              </code>
              <CopyButton text={cloneUrl} label={t('project.copyClone')} />
            </div>
            <p className={styles.hint}>{t('project.cloneHint')}</p>
          </section>
          <ProjectFiles projectId={p.id} project={p.path} />
        </div>
        <ProjectInfo id={p.id} path={p.path} created={p.created_at} />
      </div>
    </>
  );
}
