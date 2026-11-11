// /-/groups (plan 15 §3.3, MR 4): every group I may see (`GET /api/v1/groups`)
// as a nested list built from the paths, with the create actions:
// - "New group" (top level): administrators only (plan 15 §6-1);
// - "New subgroup": where my effective role is Owner (or I am an administrator);
// - "New project": where my effective role is Maintainer or above (or admin).
// The roles come from the list itself; the server decides again, and a refusal
// is shown in the dialog as user_message + request_id. A create navigates to
// the new group / project (the dialogs do it).
//
// TODO(plan 15 MR 3): group visibility (setting and badge) and reading the
// children with `?parent_id=` need the server change of plan 15 MR 3.
import { useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';

import { useApi } from '../api/context';
import { groupsQuery } from '../api/lists';
import controls from '../components/controls.module.css';
import { Loading } from '../components/Loading';
import { QueryError } from '../components/QueryError';
import { CreateGroupDialog, CreateProjectDialog } from '../features/CreateDialogs';
import { GroupTree, type CreateTarget } from '../features/GroupTree';
import { buildGroupTree } from '../features/groupTreeModel';
import lists from '../features/Lists.module.css';
import { useDocumentTitle } from '../hooks/useDocumentTitle';
import { useSession } from '../session/session';
import styles from './Home.module.css';

type Dialog = 'group' | 'subgroup' | 'project' | null;

export function GroupsPage() {
  const { t } = useTranslation();
  const api = useApi();
  const { user } = useSession();
  useDocumentTitle(t('groups.title'));
  const [dialog, setDialog] = useState<Dialog>(null);
  // Kept after closing, so the dialog stays mounted and the browser returns
  // the focus to the button that opened it.
  const [target, setTarget] = useState<CreateTarget | null>(null);
  const groups = useQuery(groupsQuery(api));
  const close = () => {
    setDialog(null);
  };

  const createButton = user.is_admin ? (
    <button
      type="button"
      className={controls.primary}
      onClick={() => {
        setDialog('group');
      }}
    >
      {t('groups.new')}
    </button>
  ) : null;

  return (
    <>
      <div className={styles.top}>
        <h1 className={styles.title}>{t('groups.title')}</h1>
        {createButton !== null && <div className={styles.actions}>{createButton}</div>}
      </div>
      {groups.isPending && <Loading />}
      {groups.isError && (
        <QueryError
          error={groups.error}
          onRetry={() => {
            void groups.refetch();
          }}
        />
      )}
      {groups.isSuccess &&
        (groups.data.items.length === 0 ? (
          <div className={lists.empty}>
            <p>{t('groups.empty')}</p>
            {user.is_admin ? (
              <p className={lists.hint}>{t('groups.emptyHintAdmin')}</p>
            ) : (
              <>
                <p className={lists.hint}>{t('groups.emptyMember')}</p>
                <p className={lists.hint}>{t('groups.emptyHint')}</p>
              </>
            )}
          </div>
        ) : (
          <>
            {groups.data.truncated && (
              <p role="status" className={lists.hint}>
                {t('groups.truncated')}
              </p>
            )}
            <GroupTree
              roots={buildGroupTree(groups.data.items)}
              isAdmin={user.is_admin}
              onNewSubgroup={(next) => {
                setTarget(next);
                setDialog('subgroup');
              }}
              onNewProject={(next) => {
                setTarget(next);
                setDialog('project');
              }}
            />
          </>
        ))}
      {user.is_admin && <CreateGroupDialog open={dialog === 'group'} onClose={close} />}
      {target !== null && (
        <>
          <CreateGroupDialog
            open={dialog === 'subgroup'}
            onClose={close}
            parent={{ groupId: target.id, path: target.path }}
          />
          <CreateProjectDialog
            open={dialog === 'project'}
            onClose={close}
            parent={{ namespaceId: target.namespaceId, path: target.path }}
          />
        </>
      )}
    </>
  );
}
