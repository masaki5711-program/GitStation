// Pieces the settings screens share: the confirmation dialog for a
// destructive request (native <dialog>, the safe button has the focus), the
// failure of a request as the one-line reason + request id (ErrorNotice via
// QueryError), and the stale-CSRF refresh on a 403.
import { useQueryClient } from '@tanstack/react-query';
import { useState, type ReactNode } from 'react';
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
  children,
}: {
  id: string;
  title: string;
  children: ReactNode;
}) {
  return (
    <section className={styles.section} aria-labelledby={id}>
      <h2 id={id} className={styles.sectionTitle} tabIndex={-1}>
        {title}
      </h2>
      {children}
    </section>
  );
}

export function focusHeading(id: string) {
  document.getElementById(id)?.focus();
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
