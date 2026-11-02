// The sign-in screen: shown in place of any page when no session exists, so
// the address stays the page the person asked for and `next` brings them back
// to it (plan 10 §4.1). One button; no password field here (ADR-0021).
import { useTranslation } from 'react-i18next';

import { SignInPanel } from '../features/SignInPanel';
import { useDocumentTitle } from '../hooks/useDocumentTitle';
import styles from './Login.module.css';

export function Login({ next }: { next: string }) {
  const { t } = useTranslation();
  useDocumentTitle(t('login.title'));
  return (
    <section className={styles.card} aria-labelledby="login-title">
      <h1 id="login-title" className={styles.title}>
        {t('login.title')}
      </h1>
      <p className={styles.lead}>{t('login.lead')}</p>
      <SignInPanel next={next} />
    </section>
  );
}
