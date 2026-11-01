// The project top page (plan 10 §4.3): summary, clone URL and the state of
// the repository. Browsing files, commits and the rest is the next screen
// group; this page says what is there (default branch, counts) or, for an
// empty repository, how to make the first push.
import { useQuery } from '@tanstack/react-query';
import { useEffect } from 'react';
import { useTranslation } from 'react-i18next';

import { Link } from 'react-router';

import { call } from '../api/client';
import { useSpec } from '../api/repo';
import { encodePath } from '../features/repoUrls';
import { TreeListing } from './repo/TreeListing';
import { Loading } from '../components/Loading';
import { useApi } from '../api/context';
import { Breadcrumb } from '../components/Breadcrumb';
import controls from '../components/controls.module.css';
import { CopyButton } from '../components/CopyButton';
import { MovedNotice } from '../components/MovedNotice';
import { QueryError } from '../components/QueryError';
import { formatDate, kindLabel, roleLabel, visibilityLabel } from '../features/labels';
import { portalUrl } from '../features/portal';
import { useDocumentTitle } from '../hooks/useDocumentTitle';
import { rememberProject } from '../hooks/recent';
import styles from './Namespace.module.css';

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
  const base = `/${encodePath(path)}`;
  const { t } = useTranslation();
  const api = useApi();
  const refs = useQuery({
    queryKey: ['refs', id],
    queryFn: ({ signal }) =>
      call(
        () =>
          api.GET('/api/v1/projects/{id}/repository/refs', {
            signal,
            params: { path: { id } },
          }),
      ),
  });

  if (refs.isPending) return <Loading />;
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
  const branches = list.filter((r) => r.type === 'branch').length;
  const tags = list.filter((r) => r.type === 'tag').length;
  return (
    <section className={styles.card} aria-labelledby="repo-state">
      <h2 id="repo-state">{t('project.repository')}</h2>
      <dl className={styles.summary}>
        {branch !== null && (
          <>
            <dt>{t('project.defaultBranch')}</dt>
            <dd>
              <code>{branch}</code>
            </dd>
          </>
        )}
        <dt>{t('project.branches')}</dt>
        <dd>{branches}</dd>
        <dt>{t('project.tags')}</dt>
        <dd>{tags}</dd>
      </dl>
      {refs.data.truncated && <p className={styles.hint}>{t('project.refsTruncated')}</p>}
      <nav aria-label={t('repo.navLabel')}>
        <ul className={styles.links}>
          <li>
            <Link to={`${base}/-/tree`}>{t('repo.files')}</Link>
          </li>
          <li>
            <Link to={`${base}/-/commits`}>{t('repo.commits')}</Link>
          </li>
          <li>
            <Link to={`${base}/-/branches`}>{t('repo.branches')}</Link>
          </li>
          <li>
            <Link to={`${base}/-/tags`}>{t('repo.tags')}</Link>
          </li>
          <li>
            <Link to={`${base}/-/compare`}>{t('repo.compare')}</Link>
          </li>
          <li>
            <Link to={`${base}/-/members`}>{t('members.title')}</Link>
          </li>
          <li>
            <Link to={`${base}/-/settings`}>{t('settings.title')}</Link>
          </li>
        </ul>
      </nav>
    </section>
  );
}

/** The root of the default branch, with its README, under the summary. */
function ProjectFiles({ projectId, project }: { projectId: number; project: string }) {
  const spec = useSpec(projectId, '');
  if (spec.data === undefined) return null;
  return <TreeListing projectId={projectId} project={project} spec={spec.data} />;
}

export function ProjectPage({ id }: { id: number }) {
  const { t, i18n } = useTranslation();
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
        <span className={controls.badge}>{kindLabel(t, p.kind)}</span>
        <span className={controls.badge}>{visibilityLabel(t, p.visibility)}</span>
        {role !== null && <span className={controls.badge}>{role}</span>}
      </p>
      {p.description !== '' && <p>{p.description}</p>}
      {p.kind === 'spec' && (
        <p>
          <Link className={controls.primary} to={portalUrl(p.path)}>
            {t('portal.openSpec')}
          </Link>
        </p>
      )}
      <dl className={styles.summary}>
        <dt>{t('project.path')}</dt>
        <dd>
          <code>{p.path}</code>
        </dd>
        <dt>{t('project.created')}</dt>
        <dd>{formatDate(p.created_at, i18n.language)}</dd>
      </dl>

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

      <RepositoryState id={p.id} name={p.name} path={p.path} cloneUrl={cloneUrl} />
      <ProjectFiles projectId={p.id} project={p.path} />
    </>
  );
}
