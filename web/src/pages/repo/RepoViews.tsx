// `/{project}/-/{word}/…` for a project that exists (ADR-0015 screen words):
// the repository screens. The project's name and a small navigation frame
// every screen; each screen has its own h1. Words that are real but not built
// yet (settings, members: the next release) say so; any other word is 404.
import { useQuery } from '@tanstack/react-query';
import { useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { call } from '../../api/client';
import { useApi } from '../../api/context';
import { Loading } from '../../components/Loading';
import { MovedNotice } from '../../components/MovedNotice';
import { encodePath } from '../../features/repoUrls';
import { rememberProject } from '../../hooks/recent';
import { NotFound, Unavailable } from '../NotFound';
import { BlameScreen } from './BlameScreen';
import { BlobScreen } from './BlobScreen';
import { CommitScreen } from './CommitScreen';
import { CommitsScreen } from './CommitsScreen';
import { CompareScreen } from './CompareScreen';
import { RefsScreen } from './RefsScreen';
import { RepoError } from './common';
import { TreeScreen } from './TreeScreen';
import styles from './Repo.module.css';

const NOT_YET = ['settings', 'members'];
const SCREEN_WORDS = ['tree', 'blob', 'blame', 'commits', 'commit', 'compare', 'branches', 'tags'];

export function RepoViews({
  projectId,
  project,
  view,
}: {
  projectId: number;
  /** Canonical path of the project. */
  project: string;
  /** The segments after `/-/`. */
  view: string[];
}) {
  const { t } = useTranslation();
  const api = useApi();
  const word = view[0] ?? '';
  const tail = view.slice(1);
  const info = useQuery({
    queryKey: ['project', projectId],
    queryFn: ({ signal }) =>
      call(() =>
        api.GET('/api/v1/projects/{id}', { signal, params: { path: { id: projectId } } }),
      ),
  });
  const known = info.data;
  useEffect(() => {
    if (known !== undefined) rememberProject({ path: known.path, name: known.display_name });
  }, [known]);

  if (NOT_YET.includes(word)) return <Unavailable backTo={`/${encodePath(project)}`} />;
  if (!SCREEN_WORDS.includes(word)) return <NotFound />;
  if (info.isPending) return <Loading />;
  if (info.isError) {
    return (
      <RepoError
        error={info.error}
        onRetry={() => {
          void info.refetch();
        }}
      />
    );
  }
  const spec = tail.join('/');
  let screen;
  switch (word) {
    case 'tree':
      screen = <TreeScreen projectId={projectId} project={project} spec={spec} />;
      break;
    case 'blob':
      screen = <BlobScreen projectId={projectId} project={project} spec={spec} />;
      break;
    case 'blame':
      screen = <BlameScreen projectId={projectId} project={project} spec={spec} />;
      break;
    case 'commits':
      screen = <CommitsScreen projectId={projectId} project={project} spec={spec} />;
      break;
    case 'commit':
      screen =
        tail.length === 1 ? (
          <CommitScreen projectId={projectId} project={project} oid={tail[0] ?? ''} />
        ) : (
          <NotFound />
        );
      break;
    case 'compare':
      screen = <CompareScreen projectId={projectId} project={project} spec={spec} />;
      break;
    case 'branches':
      screen = <RefsScreen projectId={projectId} project={project} type="branch" />;
      break;
    default:
      screen = <RefsScreen projectId={projectId} project={project} type="tag" />;
  }

  const base = `/${encodePath(project)}`;
  const items = [
    { words: ['tree', 'blob', 'blame'], to: `${base}/-/tree`, label: t('repo.files') },
    { words: ['commits', 'commit'], to: `${base}/-/commits`, label: t('repo.commits') },
    { words: ['branches'], to: `${base}/-/branches`, label: t('repo.branches') },
    { words: ['tags'], to: `${base}/-/tags`, label: t('repo.tags') },
    { words: ['compare'], to: `${base}/-/compare`, label: t('repo.compare') },
  ];
  return (
    <>
      <MovedNotice />
      <nav aria-label={t('repo.navLabel')} className={styles.projectNav}>
        <Link to={base} className={styles.projectName}>
          {info.data.display_name}
        </Link>
        <ul className={styles.navList}>
          {items.map((item) => (
            <li key={item.to}>
              <Link
                to={item.to}
                className={styles.navLink}
                {...(item.words.includes(word) ? { 'aria-current': 'page' as const } : {})}
              >
                {item.label}
              </Link>
            </li>
          ))}
        </ul>
      </nav>
      {screen}
    </>
  );
}
