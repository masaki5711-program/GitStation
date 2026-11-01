// The page frame: header, main and the session gate. Without a session the
// sign-in screen replaces the page (same address, `next` = this page).
import { useEffect, useRef } from 'react';
import { useTranslation } from 'react-i18next';
import { Outlet, useLocation } from 'react-router';

import { Login } from '../pages/Login';
import { useSessionState } from '../session/session';
import styles from '../App.module.css';
import { ErrorBoundary } from './ErrorBoundary';
import { ErrorNotice } from './ErrorNotice';
import { Header } from './Header';

export function Layout() {
  const { t } = useTranslation();
  const state = useSessionState();
  const location = useLocation();
  const main = useRef<HTMLElement>(null);
  const first = useRef(true);

  // After a navigation the focus moves to the new page (not on first load).
  useEffect(() => {
    if (first.current) {
      first.current = false;
      return;
    }
    main.current?.focus();
  }, [location.pathname]);

  let content;
  switch (state.status) {
    case 'pending':
      content = <p role="status">{t('common.loading')}</p>;
      break;
    case 'anonymous':
      content = <Login next={location.pathname + location.search} />;
      break;
    case 'error':
      content = <ErrorNotice failure={state.failure} onRetry={state.retry} />;
      break;
    case 'authed':
      content = <Outlet />;
      break;
  }

  return (
    <div className={styles.app}>
      <Header />
      <main id="main" tabIndex={-1} ref={main} className={styles.main}>
        <ErrorBoundary key={location.pathname}>{content}</ErrorBoundary>
      </main>
    </div>
  );
}
