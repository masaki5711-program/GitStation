// The page frame: header, main and the session gate. Without a session the
// sign-in screen replaces the page (same address, `next` = this page).
import { useEffect, useRef } from 'react';
import { Outlet, useLocation } from 'react-router';

import { Login } from '../pages/Login';
import { Loading } from './Loading';
import { useSessionState } from '../session/session';
import styles from '../App.module.css';
import { ErrorBoundary } from './ErrorBoundary';
import { ErrorNotice } from './ErrorNotice';
import { Header } from './Header';

export function Layout() {
  const state = useSessionState();
  const location = useLocation();
  const main = useRef<HTMLElement>(null);
  const first = useRef(true);

  // After a navigation the focus moves to the new page's heading (WCAG 2.4.3;
  // not on first load).
  useEffect(() => {
    if (first.current) {
      first.current = false;
      return;
    }
    const root = main.current;
    if (root === null) return;
    const focusHeading = (): boolean => {
      const h1 = root.querySelector('h1');
      if (h1 === null) return false;
      h1.tabIndex = -1;
      h1.focus();
      return true;
    };
    if (focusHeading()) return;
    // The page is still loading: focus its heading when it appears; if none
    // does (an error screen), the main area takes the focus.
    const observer = new MutationObserver(() => {
      if (focusHeading()) observer.disconnect();
    });
    observer.observe(root, { childList: true, subtree: true });
    const timer = window.setTimeout(() => {
      observer.disconnect();
      root.focus();
    }, 3000);
    return () => {
      observer.disconnect();
      window.clearTimeout(timer);
    };
  }, [location.pathname]);

  let content;
  switch (state.status) {
    case 'pending':
      content = <Loading />;
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
