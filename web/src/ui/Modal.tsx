// The modal: the HTML <dialog> element with showModal() (ADR-0022 §3a). The
// browser traps the focus, closes on Esc and makes the page behind inert;
// stopping the page scroll is CSS (global.css). Nothing here injects a
// <style>. Children are mounted only while open, so a form starts fresh.
//
// - Focus: after showModal() the element marked `data-initial-focus` gets it
//   (else the first form control), not the Close button.
// - `busy`: a save is under way; Close and Esc do nothing until it ends, so a
//   result never arrives at a form that is gone. Chromium closes a dialog on a
//   second Esc without a cancelable `cancel` event; the `close` event then
//   shows it again instead of telling the page it closed.
import { useEffect, useId, useRef, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';

import controls from '../components/controls.module.css';
import styles from './Modal.module.css';

export function Modal({
  open,
  onClose,
  title,
  busy = false,
  children,
}: {
  open: boolean;
  onClose: () => void;
  title: string;
  busy?: boolean;
  children: ReactNode;
}) {
  const { t } = useTranslation();
  const ref = useRef<HTMLDialogElement>(null);
  const titleId = useId();

  const handleClose = () => {
    if (busy && open) {
      const dialog = ref.current;
      if (dialog !== null && !dialog.open && typeof dialog.showModal === 'function') {
        dialog.showModal();
      }
      return;
    }
    onClose();
  };

  useEffect(() => {
    const dialog = ref.current;
    if (dialog === null) return;
    if (open && !dialog.open) {
      // jsdom has no showModal(); real browsers always do.
      if (typeof dialog.showModal === 'function') dialog.showModal();
      else dialog.setAttribute('open', '');
      const target =
        dialog.querySelector<HTMLElement>('[data-initial-focus]') ??
        dialog.querySelector<HTMLElement>('input, select, textarea');
      target?.focus();
    } else if (!open && dialog.open) {
      if (typeof dialog.close === 'function') dialog.close();
      else dialog.removeAttribute('open');
    }
  }, [open]);

  return (
    <dialog
      ref={ref}
      className={styles.dialog}
      aria-labelledby={titleId}
      onClose={handleClose}
      onCancel={(e) => {
        if (busy) e.preventDefault();
      }}
    >
      {open && (
        <div className={styles.body}>
          <div className={styles.head}>
            <h2 id={titleId} className={styles.title}>
              {title}
            </h2>
            <button type="button" className={controls.button} disabled={busy} onClick={onClose}>
              {t('common.close')}
            </button>
          </div>
          {children}
        </div>
      )}
    </dialog>
  );
}
