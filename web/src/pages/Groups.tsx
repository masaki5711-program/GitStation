// /-/groups (plan 15 §3.3, MR 1): every group I may see (`GET /api/v1/groups`),
// flat for now — the tree, subgroup and visibility actions are plan 15 MR 4.
// Only an administrator may create a top-level group (plan 15 §6-1), so the
// create button is shown to administrators only; the server decides again.
import { useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { useTranslation } from 'react-i18next';

import { useApi } from '../api/context';
import { groupsQuery } from '../api/lists';
import controls from '../components/controls.module.css';
import { Loading } from '../components/Loading';
import { QueryError } from '../components/QueryError';
import { CreateGroupDialog } from '../features/CreateDialogs';
import { GroupList } from '../features/Lists';
import lists from '../features/Lists.module.css';
import { useDocumentTitle } from '../hooks/useDocumentTitle';
import { useSession } from '../session/session';
import styles from './Home.module.css';

export function GroupsPage() {
  const { t } = useTranslation();
  const api = useApi();
  const { user } = useSession();
  useDocumentTitle(t('groupsPage.title'));
  const [creating, setCreating] = useState(false);
  const groups = useQuery(groupsQuery(api));

  const createButton = user.is_admin ? (
    <button
      type="button"
      className={controls.primary}
      onClick={() => {
        setCreating(true);
      }}
    >
      {t('home.newGroup')}
    </button>
  ) : null;

  return (
    <>
      <div className={styles.top}>
        <h1 className={styles.title}>{t('groupsPage.title')}</h1>
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
            <p>{t('groupsPage.empty')}</p>
            <p className={lists.hint}>
              {user.is_admin ? t('groupsPage.emptyHintAdmin') : t('groupsPage.emptyHint')}
            </p>
          </div>
        ) : (
          <>
            <GroupList items={groups.data.items} />
            {groups.data.truncated && <p className={lists.hint}>{t('common.truncated')}</p>}
          </>
        ))}
      {user.is_admin && (
        <CreateGroupDialog
          open={creating}
          onClose={() => {
            setCreating(false);
          }}
        />
      )}
    </>
  );
}
