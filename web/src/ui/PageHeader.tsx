// A page's head: the h1 (Layout moves the focus to it after a navigation),
// an optional one-line subtitle, and the page's primary action on the right
// (it wraps under the title on a narrow screen). The list pages share it, so
// they share one rhythm.
import type { ReactNode, Ref } from 'react';

import styles from './PageHeader.module.css';

export function PageHeader({
  title,
  subtitle,
  actions,
  titleId,
  titleRef,
}: {
  title: string;
  /** One line under the title: what the page is for (text, or a sentence with a link). */
  subtitle?: ReactNode;
  /** The page's primary action (a button or two). */
  actions?: ReactNode;
  titleId?: string;
  /** For a page that moves the focus back to its heading itself. */
  titleRef?: Ref<HTMLHeadingElement>;
}) {
  return (
    <div className={styles.root}>
      <div className={styles.text}>
        <h1
          className={styles.title}
          {...(titleId === undefined ? {} : { id: titleId })}
          {...(titleRef === undefined ? {} : { ref: titleRef, tabIndex: -1 })}
        >
          {title}
        </h1>
        {subtitle !== undefined && <p className={styles.subtitle}>{subtitle}</p>}
      </div>
      {actions !== undefined && actions !== null && <div className={styles.actions}>{actions}</div>}
    </div>
  );
}
