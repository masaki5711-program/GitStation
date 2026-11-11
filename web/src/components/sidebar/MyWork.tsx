// "My work" (plan 15 §3.1): Home, Projects, Groups, Merge requests. An item
// is added when its screen exists (no placeholders, plan 10): Activity comes
// with plan 15 MR 9. Notifications, settings and admin stay in the header.
// Merge requests carries a badge: the ones that wait on me (mine that need
// action + the ones waiting for my approval). No number while the count is
// loading or failed (Home and the page say why); none when it is 0.
import { useTranslation } from 'react-i18next';

import { isNone, mrCountText, useUserMrCounts } from '../../api/userMergeRequests';
import styles from '../Sidebar.module.css';
import { ItemList, type Item, type SectionProps } from './parts';

const MY_MERGE_REQUESTS_PATH = '/-/merge-requests';

function useWaitingBadge(): Item['badge'] {
  const { t } = useTranslation();
  const counts = useUserMrCounts();
  if (!counts.isSuccess) return undefined;
  const { attention, approvable } = counts.data;
  const total = attention.count + approvable.count;
  // The sum is capped / a lower bound when either part is.
  const sum = {
    count: total,
    capped: attention.capped || approvable.capped,
    truncated: attention.truncated || approvable.truncated,
  };
  if (isNone(sum)) return undefined;
  const shown = mrCountText(sum);
  return { shown, spoken: t('nav.mrBadge', { shown }) };
}

export function MyWork({ pathname, collapsed, onNavigate }: SectionProps & { pathname: string }) {
  const { t } = useTranslation();
  const badge = useWaitingBadge();
  const items: Item[] = [
    { to: '/', icon: 'home', text: t('nav.home'), current: pathname === '/' },
    { to: '/-/projects', icon: 'projects', text: t('nav.projects'), current: pathname === '/-/projects' },
    { to: '/-/groups', icon: 'groups', text: t('nav.groups'), current: pathname === '/-/groups' },
    {
      to: MY_MERGE_REQUESTS_PATH,
      icon: 'pulls',
      text: t('nav.mergeRequests'),
      current: pathname === MY_MERGE_REQUESTS_PATH,
      badge,
    },
  ];
  return (
    <>
      <h2 className={collapsed ? styles.srOnly : styles.heading}>{t('nav.myWork')}</h2>
      <ItemList items={items} collapsed={collapsed} onNavigate={onNavigate} />
    </>
  );
}
