// Cancel (pending / running) and Retry (failed / canceled), for a role that
// may (the caller decides; the server decides again). Cancelling a running
// pipeline stops jobs midway, so it asks once more first; while a request is
// out the button says so and the group is aria-busy. A refusal shows the
// server's user_message and request_id.
import { useEffect, useId, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';

import {
  canCancel,
  canRetry,
  useCancelPipeline,
  useRetryPipeline,
  type Pipeline,
} from '../../../api/pipelines';
import { QueryError } from '../../../components/QueryError';
import controls from '../../../components/controls.module.css';
import styles from './Pipelines.module.css';

export function PipelineActions({ projectId, p }: { projectId: number; p: Pipeline }) {
  const { t } = useTranslation();
  const cancel = useCancelPipeline(projectId, p.id);
  const retry = useRetryPipeline(projectId, p.id);
  const [confirming, setConfirming] = useState(false);
  const confirmButton = useRef<HTMLButtonElement>(null);
  const cancelButton = useRef<HTMLButtonElement>(null);
  const questionId = useId();
  const busy = cancel.isPending || retry.isPending;
  const showCancel = canCancel(p.status);
  const showRetry = canRetry(p.status);
  const asking = confirming && showCancel && !busy;

  useEffect(() => {
    if (asking) confirmButton.current?.focus();
  }, [asking]);

  if (!showCancel && !showRetry && !cancel.isError && !retry.isError) return null;
  const doCancel = () => {
    retry.reset();
    setConfirming(false);
    cancel.mutate();
  };
  return (
    <div data-testid="pipeline-actions" aria-busy={busy}>
      <p className={styles.actions}>
        {showCancel && (
          <button
            ref={cancelButton}
            type="button"
            className={controls.button}
            disabled={busy || asking}
            onClick={() => {
              if (p.status === 'running') setConfirming(true);
              else doCancel();
            }}
          >
            {cancel.isPending ? t('pipelines.canceling') : t('pipelines.cancel')}
          </button>
        )}
        {showRetry && (
          <button
            type="button"
            className={controls.primary}
            disabled={busy}
            onClick={() => {
              cancel.reset();
              retry.mutate();
            }}
          >
            {retry.isPending ? t('pipelines.retrying') : t('pipelines.retry')}
          </button>
        )}
      </p>
      {asking && (
        <div role="group" aria-labelledby={questionId} className={styles.confirm} data-testid="cancel-confirm">
          <p id={questionId}>{t('pipelines.cancelConfirm')}</p>
          <p className={styles.actions}>
            <button ref={confirmButton} type="button" className={controls.primary} onClick={doCancel}>
              {t('pipelines.cancelYes')}
            </button>
            <button
              type="button"
              className={controls.button}
              onClick={() => {
                setConfirming(false);
                window.setTimeout(() => cancelButton.current?.focus(), 0);
              }}
            >
              {t('pipelines.cancelNo')}
            </button>
          </p>
        </div>
      )}
      {cancel.isError && <QueryError error={cancel.error} />}
      {retry.isError && <QueryError error={retry.error} />}
    </div>
  );
}
