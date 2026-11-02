// Pieces the settings screens share: the confirmation dialog for a
// destructive request (native <dialog>, the safe button has the focus), the
// failure of a request as the one-line reason + request id (ErrorNotice via
// QueryError), and the stale-CSRF refresh on a 403.
import { useQueryClient } from '@tanstack/react-query';
import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react';
import { useTranslation } from 'react-i18next';

import { ApiFailure } from '../../api/client';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { SESSION_KEY } from '../../session/session';
import { Modal } from '../../ui/Modal';
import styles from './Settings.module.css';

/** A 403 on a request is most often a stale CSRF token: read the session
 * again (the reason is shown all the same). */
export function useRefreshSessionOn403() {
  const queryClient = useQueryClient();
  return (error: unknown) => {
    if (error instanceof ApiFailure && error.failure.kind === 'api' && error.failure.status === 403) {
      void queryClient.invalidateQueries({ queryKey: SESSION_KEY });
    }
  };
}

/** The reason a request was refused, in the one place a person looks. */
export function Denied({ error }: { error: unknown }) {
  return <QueryError error={error} />;
}

/** A section with its own heading (the heading takes the focus after a
 * destructive change removed the control that had it). */
export function Section({
  id,
  title,
  danger = false,
  children,
}: {
  id: string;
  title: string;
  /** The heading of a danger zone is in the danger text colour. */
  danger?: boolean;
  children: ReactNode;
}) {
  return (
    <section className={styles.section} aria-labelledby={id}>
      <h2 id={id} className={danger ? `${styles.sectionTitle} ${styles.dangerTitle}` : styles.sectionTitle}
        tabIndex={-1}>
        {title}
      </h2>
      {children}
    </section>
  );
}

/** Focus a heading once no modal dialog is open any more: a confirmation
 * closes in an effect after the request succeeded, and focus moved while the
 * dialog is still modal is lost (the page behind is inert). The polling stops
 * when the screen goes away, and a heading that is no longer in the page is
 * skipped. */
export function useFocusHeading() {
  const timer = useRef<number | null>(null);
  useEffect(
    () => () => {
      if (timer.current !== null) window.clearTimeout(timer.current);
    },
    [],
  );
  return useCallback((id: string) => {
    if (timer.current !== null) window.clearTimeout(timer.current);
    let tries = 0;
    const attempt = () => {
      tries += 1;
      if (document.querySelector('dialog[open]') !== null && tries < 50) {
        timer.current = window.setTimeout(attempt, 20);
        return;
      }
      timer.current = null;
      const heading = document.getElementById(id);
      if (heading?.isConnected === true) heading.focus();
    };
    timer.current = window.setTimeout(attempt, 0);
  }, []);
}

/** Ask before a request that cannot be taken back. The cancel button has the
 * focus; Esc and Cancel close; while the request runs nothing closes it. A
 * refusal stays in the dialog (reason + request id) so it can be read there. */
export function ConfirmDialog({
  open,
  title,
  message,
  confirmLabel,
  busy,
  error,
  onConfirm,
  onCancel,
}: {
  open: boolean;
  title: string;
  message: ReactNode;
  confirmLabel: string;
  busy: boolean;
  error: unknown;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const { t } = useTranslation();
  return (
    <Modal open={open} onClose={onCancel} title={title} busy={busy}>
      <div data-testid="confirm">
        <div className={styles.confirmText}>{message}</div>
        {error !== null && error !== undefined && <Denied error={error} />}
        <div className={styles.row}>
          <button
            type="button"
            className={controls.button}
            data-initial-focus=""
            disabled={busy}
            onClick={onCancel}
          >
            {t('common.cancel')}
          </button>
          <button
            type="button"
            className={styles.danger}
            aria-disabled={busy ? 'true' : undefined}
            onClick={() => {
              if (!busy) onConfirm();
            }}
          >
            {confirmLabel}
          </button>
        </div>
      </div>
    </Modal>
  );
}

/** State for one confirmation: what is being confirmed, kept until closed. */
export function useConfirm<T>() {
  const [target, setTarget] = useState<T | null>(null);
  return { target, ask: setTarget, close: () => { setTarget(null); } };
}
