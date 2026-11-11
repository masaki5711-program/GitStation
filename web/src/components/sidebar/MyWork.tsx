// "My work" (plan 15 §3.1): Home, Projects, Groups. An item is added when its
// screen exists (no placeholders, plan 10): Activity comes with plan 15 MR 9.
// Notifications, settings and admin stay in the header.
import { useTranslation } from 'react-i18next';

import styles from '../Sidebar.module.css';
import { ItemList, type Item, type SectionProps } from './parts';

export function MyWork({ pathname, collapsed, onNavigate }: SectionProps & { pathname: string }) {
  const { t } = useTranslation();
  const items: Item[] = [
    { to: '/', icon: 'home', text: t('nav.home'), current: pathname === '/' },
    { to: '/-/projects', icon: 'projects', text: t('nav.projects'), current: pathname === '/-/projects' },
    { to: '/-/groups', icon: 'groups', text: t('nav.groups'), current: pathname === '/-/groups' },
  ];
  return (
    <>
      <h2 className={collapsed ? styles.srOnly : styles.heading}>{t('nav.myWork')}</h2>
      <ItemList items={items} collapsed={collapsed} onNavigate={onNavigate} />
    </>
  );
}
