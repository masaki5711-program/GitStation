// `/{project}/-/{word}/…` for a project that exists (ADR-0015 screen words):
// the repository screens, settings and members. The project's name and a
// small navigation frame every screen; each screen has its own h1. Any other
// word is 404.
import { useQuery } from '@tanstack/react-query';
import { useEffect } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { call } from '../../api/client';
import { useApi } from '../../api/context';
import { Loading } from '../../components/Loading';
import { MovedNotice } from '../../components/MovedNotice';
import { encodePath } from '../../features/repoUrls';
import { useSession } from '../../session/session';
import { rememberProject } from '../../hooks/recent';
import { NotFound } from '../NotFound';
import { BlameScreen } from './BlameScreen';
import { BlobScreen } from './BlobScreen';
import { CommitScreen } from './CommitScreen';
import { CommitsScreen } from './CommitsScreen';
import { CompareScreen } from './CompareScreen';
import { DocumentScreen } from './DocumentScreen';
import { DocumentsScreen } from './DocumentsScreen';
import { parseId } from '../../api/pipelines';
import { PipelineScreen } from './pipelines/PipelineScreen';
import { PipelinesScreen } from './pipelines/PipelinesScreen';
import { RefsScreen } from './RefsScreen';
import { RepoError } from './common';
import { TreeScreen } from './TreeScreen';
import { parseIid } from '../../api/mergeRequests';
import { MergeRequestScreen } from './mr/MergeRequestScreen';
import { MergeRequestsScreen } from './mr/MergeRequestsScreen';
import { NewMergeRequestScreen } from './mr/NewMergeRequestScreen';
import { DeliveriesScreen } from '../webhooks/DeliveriesScreen';
import { WebhooksScreen } from '../webhooks/WebhooksScreen';
import { parseHookId } from '../webhooks/common';
import { MembersScreen } from '../settings/Members';
import { ProjectSettings } from '../settings/ProjectSettings';
import styles from './Repo.module.css';

const SCREEN_WORDS = ['tree', 'blob', 'blame', 'commits', 'commit', 'compare', 'branches', 'tags', 'settings', 'members', 'pulls', 'pull', 'webhooks', 'documents', 'pipelines'];

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
  const isAdmin = useSession().user.is_admin;
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

  if (!SCREEN_WORDS.includes(word)) return <NotFound />;
  // `/-/pull/{iid}[/changes]`: an address that is not one is "not found"
  // before anything is asked about it.
  const iid = word === 'pull' ? parseIid(tail[0] ?? '') : null;
  const mrTab =
    tail.length === 1 ? 'overview' : tail.length === 2 && tail[1] === 'changes' ? 'changes' : null;
  if (word === 'pull' && (iid === null || mrTab === null)) return <NotFound />;
  // `/-/pipelines[/{id}]`: the same rule for a pipeline id.
  const pipelineId = word === 'pipelines' && tail.length === 1 ? parseId(tail[0]) : null;
  if (word === 'pipelines' && tail.length > 0 && (tail.length > 1 || pipelineId === null)) {
    return <NotFound />;
  }
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
    case 'pulls':
      screen =
        tail.length === 0 ? (
          <MergeRequestsScreen projectId={projectId} project={project} />
        ) : tail.length === 1 && tail[0] === 'new' ? (
          <NewMergeRequestScreen projectId={projectId} project={project} />
        ) : (
          <NotFound />
        );
      break;
    case 'pull':
      screen =
        iid !== null && mrTab !== null ? (
          <MergeRequestScreen projectId={projectId} project={project} iid={iid} tab={mrTab} />
        ) : (
          <NotFound />
        );
      break;
    case 'documents':
      screen =
        tail.length === 0 ? (
          <DocumentsScreen projectId={projectId} project={project} />
        ) : tail.length === 1 ? (
          <DocumentScreen projectId={projectId} project={project} docId={tail[0] ?? ''} />
        ) : (
          <NotFound />
        );
      break;
    case 'pipelines':
      screen =
        pipelineId === null ? (
          <PipelinesScreen projectId={projectId} project={project} />
        ) : (
          <PipelineScreen key={pipelineId} projectId={projectId} project={project} id={pipelineId} />
        );
      break;
    case 'settings':
      screen = <ProjectSettings projectId={projectId} />;
      break;
    case 'members':
      screen = <MembersScreen scope={{ kind: 'project', id: projectId }} />;
      break;
    case 'webhooks': {
      const hookId = tail.length === 2 && tail[1] === 'deliveries' ? parseHookId(tail[0] ?? '') : null;
      screen =
        tail.length === 0 ? (
          <WebhooksScreen projectId={projectId} project={project} />
        ) : hookId !== null ? (
          <DeliveriesScreen projectId={projectId} project={project} hookId={hookId} />
        ) : (
          <NotFound />
        );
      break;
    }
    case 'branches':
      screen = <RefsScreen projectId={projectId} project={project} type="branch" />;
      break;
    default:
      screen = <RefsScreen projectId={projectId} project={project} type="tag" />;
  }

  const base = `/${encodePath(project)}`;
  // Webhooks are for a Maintainer or above; the server decides again.
  const role = info.data.role;
  const canManageHooks = role === 'maintainer' || role === 'owner' || isAdmin;
  const items = [
    { words: ['tree', 'blob', 'blame'], to: `${base}/-/tree`, label: t('repo.files') },
    { words: ['commits', 'commit'], to: `${base}/-/commits`, label: t('repo.commits') },
    { words: ['branches'], to: `${base}/-/branches`, label: t('repo.branches') },
    { words: ['tags'], to: `${base}/-/tags`, label: t('repo.tags') },
    { words: ['compare'], to: `${base}/-/compare`, label: t('repo.compare') },
    { words: ['pulls', 'pull'], to: `${base}/-/pulls`, label: t('repo.pulls') },
    { words: ['documents'], to: `${base}/-/documents`, label: t('docs.title') },
    { words: ['pipelines'], to: `${base}/-/pipelines`, label: t('pipelines.title') },
    { words: ['members'], to: `${base}/-/members`, label: t('members.title') },
    ...(canManageHooks
      ? [{ words: ['webhooks'], to: `${base}/-/webhooks`, label: t('webhooks.title') }]
      : []),
    { words: ['settings'], to: `${base}/-/settings`, label: t('settings.title') },
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
