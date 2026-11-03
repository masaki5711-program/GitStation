// A comment box: a new general thread, or a reply in one. Markdown is
// written here and drawn by the thread (never as raw HTML). An empty comment
// is refused before sending; a refusal of the server is shown as its
// user_message + request_id and the text is kept. The text is cleared only
// after the server has taken it.
import { useEffect, useId, useRef, useState, type SyntheticEvent } from 'react';
import { useTranslation } from 'react-i18next';

import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import styles from './Mr.module.css';

export interface CommentFormProps {
  label: string;
  submitLabel: string;
  /** Send the text; call `done` once the server has taken it. */
  onSend: (body: string, done: () => void) => void;
  pending: boolean;
  /** The last failed send (null: none). */
  error: unknown;
  /** Put the focus in the box when it appears (a reply box opened by a button). */
  autoFocus?: boolean;
  onCancel?: () => void;
}

export function CommentForm({
  label,
  submitLabel,
  onSend,
  pending,
  error,
  autoFocus,
  onCancel,
}: CommentFormProps) {
  const { t } = useTranslation();
  const id = useId();
  const box = useRef<HTMLTextAreaElement>(null);
  const [body, setBody] = useState('');
  const [empty, setEmpty] = useState(false);
  const [refused, setRefused] = useState(0);
  // After the render that describes the box with the error: then the focus
  // reads it.
  useEffect(() => {
    if (refused > 0) box.current?.focus();
  }, [refused]);
  useEffect(() => {
    if (autoFocus === true) box.current?.focus();
  }, [autoFocus]);

  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    // aria-disabled (not disabled) keeps the focus on the button while sending.
    if (pending) return;
    const text = body.trim();
    if (text === '') {
      setEmpty(true);
      setRefused((n) => n + 1);
      return;
    }
    setEmpty(false);
    onSend(text, () => {
      setBody('');
    });
  };

  const errorId = `${id}-error`;
  const helpId = `${id}-help`;
  return (
    <form onSubmit={submit} noValidate className={styles.commentForm}>
      <label htmlFor={id} className={controls.label}>
        {label}
      </label>
      <textarea
        ref={box}
        id={id}
        className={controls.input}
        rows={4}
        value={body}
        aria-invalid={empty ? true : undefined}
        aria-describedby={empty ? `${helpId} ${errorId}` : helpId}
        onChange={(e) => {
          setBody(e.target.value);
          if (empty && e.target.value.trim() !== '') setEmpty(false);
        }}
      />
      <p id={helpId} className={controls.help}>
        {t('mr.comment.help')}
      </p>
      {empty && (
        // No role=alert: the box takes the focus and reads this as its
        // description, so the error is said once.
        <p id={errorId} className={controls.fieldError}>
          {t('mr.comment.required')}
        </p>
      )}
      {error !== null && <QueryError error={error} />}
      <div className={styles.formActions}>
        <button
          type="submit"
          className={controls.primary}
          aria-disabled={pending ? 'true' : undefined}
        >
          {pending ? t('mr.comment.sending') : submitLabel}
        </button>
        {onCancel !== undefined && (
          <button type="button" className={controls.button} onClick={onCancel}>
            {t('common.cancel')}
          </button>
        )}
      </div>
    </form>
  );
}
