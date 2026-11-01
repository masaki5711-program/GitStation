// The modal: the HTML <dialog> element with showModal() (ADR-0022 §3a). The
// browser traps the focus, closes on Esc and makes the page behind inert;
// stopping the page scroll is CSS (global.css). Nothing here injects a
// <style>. Children are mounted only while open, so a form starts fresh.
import { useEffect, useId, useRef, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';

import controls from '../components/controls.module.css';
import styles from './Modal.module.css';

export function Modal({
  open,
  onClose,
  title,
  children,
}: {
  open: boolean;
  onClose: () => void;
  title: string;
  children: ReactNode;
}) {
  const { t } = useTranslation();
  const ref = useRef<HTMLDialogElement>(null);
  const titleId = useId();

  useEffect(() => {
    const dialog = ref.current;
    if (dialog === null) return;
    if (open && !dialog.open) {
      // jsdom has no showModal(); real browsers always do.
      if (typeof dialog.showModal === 'function') dialog.showModal();
      else dialog.setAttribute('open', '');
    } else if (!open && dialog.open) {
      if (typeof dialog.close === 'function') dialog.close();
      else dialog.removeAttribute('open');
    }
  }, [open]);

  return (
    <dialog ref={ref} className={styles.dialog} aria-labelledby={titleId} onClose={onClose}>
      {open && (
        <div className={styles.body}>
          <div className={styles.head}>
            <h2 id={titleId} className={styles.title}>
              {title}
            </h2>
            <button type="button" className={controls.button} onClick={onClose}>
              {t('common.close')}
            </button>
          </div>
          {children}
        </div>
      )}
    </dialog>
  );
}
