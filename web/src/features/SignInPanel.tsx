// The one sign-in button (plan 10 §4.1): "sign in with the IoT portal".
// States: idle, starting, redirecting, "no sign-in configured" (FG-AUTH-0018,
// an operator's matter: say so in one line), failed (user_message +
// request_id).
import { useMutation } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';

import { useApi } from '../api/context';
import type { Failure } from '../api/client';
import { ErrorNotice } from '../components/ErrorNotice';
import { startSignIn, type SignInResult } from './signin';
import styles from './SignInPanel.module.css';

export function SignInPanel({ next }: { next: string }) {
  const { t } = useTranslation();
  const api = useApi();
  const start = useMutation<SignInResult>({ mutationFn: () => startSignIn(api, next) });
  const result = start.data;
  const busy = start.isPending || result?.kind === 'redirecting';

  let failure: Failure | null = null;
  if (start.isError) failure = { kind: 'unexpected', status: null, requestId: null };
  else if (result?.kind === 'failed') failure = result.failure;

  return (
    <div className={styles.panel}>
      <button
        type="button"
        className={styles.primary}
        disabled={busy}
        onClick={() => {
          start.mutate();
        }}
      >
        {busy ? t('login.starting') : t('login.button')}
      </button>
      <p className={styles.hint}>{t('login.hint')}</p>
      {result?.kind === 'notConfigured' && (
        <ErrorNotice
          title={t('login.notConfiguredTitle')}
          failure={{
            kind: 'api',
            status: 503,
            userMessage: t('login.notConfigured'),
            requestId: result.requestId,
          }}
        />
      )}
      {failure !== null && (
        <ErrorNotice
          failure={failure}
          onRetry={() => {
            start.mutate();
          }}
        />
      )}
    </div>
  );
}
