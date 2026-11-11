// The pieces every sidebar section is built from: an item (a link with an
// icon and its name), a list of items, and a section head (the way back to
// "My work", then the section's own name, a placeholder while it loads, or a
// compact error with a retry).
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import { InlineError } from '../InlineError';
import styles from '../Sidebar.module.css';
import { ICONS, type IconKey } from './icons';

export interface Item {
  to: string;
  icon: IconKey;
  text: string;
  current: boolean;
  /** A number next to the name: `shown` is drawn (hidden from assistive
   * tech), `spoken` is appended to the link's name as hidden text. */
  badge?: { shown: string; spoken: string } | undefined;
}

export interface SectionProps {
  collapsed: boolean;
  /** A link to `to` was followed (closes the narrow-screen panel). */
  onNavigate: (to: string) => void;
}

export function Label({ collapsed, text }: { collapsed: boolean; text: string }) {
  return <span className={collapsed ? styles.srOnly : styles.label}>{text}</span>;
}

export function NavItem({
  item,
  collapsed,
  onNavigate,
  className,
}: SectionProps & { item: Item; className?: string | undefined }) {
  return (
    <Link
      to={item.to}
      className={className ?? styles.link ?? ''}
      {...(item.current ? { 'aria-current': 'page' as const } : {})}
      onClick={() => {
        onNavigate(item.to);
      }}
    >
      {ICONS[item.icon]}
      <Label collapsed={collapsed} text={item.text} />
      {item.badge && (
        <>
          <span className={styles.badge} aria-hidden="true">
            {item.badge.shown}
          </span>
          <span className={styles.srOnly}>{item.badge.spoken}</span>
        </>
      )}
    </Link>
  );
}

export function ItemList({ items, collapsed, onNavigate }: SectionProps & { items: Item[] }) {
  return (
    <ul className={styles.list}>
      {items.map((item) => (
        <li key={item.to}>
          <NavItem item={item} collapsed={collapsed} onNavigate={onNavigate} />
        </li>
      ))}
    </ul>
  );
}

/** The section's name as the query for it stands. */
export type TitleState =
  | { status: 'pending' }
  | { status: 'error'; error: unknown; retry: () => void }
  | { status: 'success'; item: Item };

export function SectionHead({ title, collapsed, onNavigate }: SectionProps & { title: TitleState }) {
  const { t } = useTranslation();
  const back: Item = { to: '/', icon: 'back', text: t('nav.myWork'), current: false };
  let name;
  switch (title.status) {
    case 'pending':
      name = (
        <p role="status" className={styles.placeholder}>
          <span className={styles.srOnly}>{t('common.loading')}</span>
        </p>
      );
      break;
    case 'error':
      name = <InlineError error={title.error} onRetry={title.retry} hidden={collapsed} />;
      break;
    case 'success':
      name = <NavItem item={title.item} collapsed={collapsed} onNavigate={onNavigate} />;
      break;
  }
  return (
    <>
      <NavItem item={back} collapsed={collapsed} onNavigate={onNavigate} className={styles.back} />
      <div className={styles.contextTitle}>{name}</div>
    </>
  );
}

/** A query's state as a section title. */
export function titleOf<T>(
  query: { status: 'pending' | 'error' | 'success'; data: T | undefined; error: unknown; refetch: () => unknown },
  item: (data: T) => Item,
): TitleState {
  if (query.status === 'success' && query.data !== undefined) return { status: 'success', item: item(query.data) };
  if (query.status === 'error') {
    return {
      status: 'error',
      error: query.error,
      retry: () => {
        void query.refetch();
      },
    };
  }
  return { status: 'pending' };
}
