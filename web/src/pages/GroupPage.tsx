// A group (or a person's own namespace): the groups and projects directly
// under it, with create buttons. The list API has no parent filter, so the
// whole visible lists are read (capped, and the cap is said) and filtered by
// parent path here.
import { useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';

import { call } from '../api/client';
import { useApi } from '../api/context';
import { groupsQuery, parentPath, projectsQuery } from '../api/lists';
import { Breadcrumb } from '../components/Breadcrumb';
import { MovedNotice } from '../components/MovedNotice';
import controls from '../components/controls.module.css';
import { QueryError } from '../components/QueryError';
import { CreateGroupDialog, CreateProjectDialog } from '../features/CreateDialogs';
import { GroupList, ProjectList } from '../features/Lists';
import { roleLabel } from '../features/labels';
import { useDocumentTitle } from '../hooks/useDocumentTitle';
import { useSession } from '../session/session';
import styles from './Namespace.module.css';

function Children({ path }: { path: string }) {
  const { t } = useTranslation();
  const api = useApi();
  const groups = useQuery(groupsQuery(api));
  const projects = useQuery(projectsQuery(api));

  const subgroups = groups.data?.items.filter((g) => parentPath(g.path) === path) ?? [];
  const inside = projects.data?.items.filter((p) => parentPath(p.path) === path) ?? [];
  const loading = groups.isPending || projects.isPending;
  const truncated = groups.data?.truncated === true || projects.data?.truncated === true;

  return (
    <>
      {loading && <p role="status">{t('common.loading')}</p>}
      {groups.isError && (
        <QueryError
          error={groups.error}
          onRetry={() => {
            void groups.refetch();
          }}
        />
      )}
      {projects.isError && (
        <QueryError
          error={projects.error}
          onRetry={() => {
            void projects.refetch();
          }}
        />
      )}
      {groups.isSuccess && subgroups.length > 0 && (
        <section aria-labelledby="ns-groups" className={styles.block}>
          <h2 id="ns-groups" className={styles.heading}>
            {t('namespace.subgroups')}
          </h2>
          <GroupList items={subgroups} />
        </section>
      )}
      {projects.isSuccess && inside.length > 0 && (
        <section aria-labelledby="ns-projects" className={styles.block}>
          <h2 id="ns-projects" className={styles.heading}>
            {t('namespace.projects')}
          </h2>
          <ProjectList items={inside} />
        </section>
      )}
      {groups.isSuccess && projects.isSuccess && subgroups.length === 0 && inside.length === 0 && (
        <div className={styles.empty}>
          <p>{t('namespace.empty')}</p>
        </div>
      )}
      {truncated && <p className={styles.hint}>{t('common.truncated')}</p>}
    </>
  );
}

export function GroupPage({ id, path }: { id: number; path: string }) {
  const { t } = useTranslation();
  const api = useApi();
  const session = useSession();
  const [dialog, setDialog] = useState<'project' | 'group' | null>(null);
  const group = useQuery({
    queryKey: ['group', id],
    queryFn: ({ signal }) =>
      call(() => api.GET('/api/v1/groups/{id}', { signal, params: { path: { id } } })),
  });
  useDocumentTitle(group.data?.display_name ?? path);

  if (group.isPending) return <p role="status">{t('common.loading')}</p>;
  if (group.isError) {
    return (
      <QueryError
        error={group.error}
        onRetry={() => {
          void group.refetch();
        }}
      />
    );
  }
  const g = group.data;
  const role = roleLabel(t, g.role);
  const canCreate = g.role !== null || session.user.is_admin;
  return (
    <>
      <Breadcrumb path={g.path} />
      <MovedNotice />
      <div className={styles.top}>
        <div>
          <h1 className={styles.title}>{g.display_name}</h1>
          <p className={styles.meta}>
            <span className={controls.badge}>{t('namespace.group')}</span>
            {role !== null && <span className={controls.badge}>{role}</span>}
          </p>
          {g.description !== '' && <p>{g.description}</p>}
        </div>
        {canCreate && (
          <div className={styles.actions}>
            <button
              type="button"
              className={controls.primary}
              onClick={() => {
                setDialog('project');
              }}
            >
              {t('namespace.newProject')}
            </button>
            <button
              type="button"
              className={controls.button}
              onClick={() => {
                setDialog('group');
              }}
            >
              {t('namespace.newSubgroup')}
            </button>
          </div>
        )}
      </div>
      <Children path={g.path} />
      <CreateProjectDialog
        open={dialog === 'project'}
        onClose={() => {
          setDialog(null);
        }}
        parent={{ namespaceId: g.namespace_id, path: g.path }}
      />
      <CreateGroupDialog
        open={dialog === 'group'}
        onClose={() => {
          setDialog(null);
        }}
        parent={{ groupId: g.id, path: g.path }}
      />
    </>
  );
}

export function UserPage({ path, namespaceId }: { path: string; namespaceId: number }) {
  const { t } = useTranslation();
  const session = useSession();
  const [open, setOpen] = useState(false);
  useDocumentTitle(path);
  const own = session.user.username.toLowerCase() === path.toLowerCase();
  return (
    <>
      <Breadcrumb path={path} />
      <MovedNotice />
      <div className={styles.top}>
        <div>
          <h1 className={styles.title}>{path}</h1>
          <p className={styles.meta}>
            <span className={controls.badge}>{t('namespace.user')}</span>
          </p>
        </div>
        {(own || session.user.is_admin) && (
          <div className={styles.actions}>
            <button
              type="button"
              className={controls.primary}
              onClick={() => {
                setOpen(true);
              }}
            >
              {t('namespace.newProject')}
            </button>
          </div>
        )}
      </div>
      <Children path={path} />
      <CreateProjectDialog
        open={open}
        onClose={() => {
          setOpen(false);
        }}
        parent={{ namespaceId, path }}
      />
    </>
  );
}
