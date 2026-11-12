// The page frame: header, the sidebar (My work / project / group) (signed in only), main and
// the session gate. Without a session the sign-in screen replaces the page
// (same address, `next` = this page).
import { useCallback, useEffect, useId, useRef, useState } from 'react';
import { Outlet, useLocation } from 'react-router';

import { Login } from '../pages/Login';
import { Loading } from './Loading';
import { useSessionState } from '../session/session';
import styles from '../App.module.css';
import { ErrorBoundary } from './ErrorBoundary';
import { ErrorNotice } from './ErrorNotice';
import { Header } from './Header';
import { Sidebar } from './Sidebar';
import { SidebarScopeProvider } from './sidebarScope';
import { useNarrowScreen } from '../hooks/useNarrowScreen';

/** `bare`: no header (the act-as confirmation popup has nowhere to go). */
export function Layout({ bare = false }: { bare?: boolean }) {
  const state = useSessionState();
  const location = useLocation();
  const main = useRef<HTMLElement>(null);
  const first = useRef(true);
  const narrow = useNarrowScreen();
  const navId = useId();
  const menuButton = useRef<HTMLButtonElement>(null);
  const [navOpen, setNavOpen] = useState(false);
  const pathname = location.pathname;
  // A followed link closes the narrow-screen panel. The link of the page I am
  // on changes no route, so the focus (on the now hidden link) would fall to
  // the body: give it back to the menu button.
  const closeNav = useCallback(
    (to: string) => {
      setNavOpen(false);
      if (to === pathname) menuButton.current?.focus();
    },
    [pathname],
  );

  // Narrow screens: opening moves the focus to the first item.
  useEffect(() => {
    if (!narrow || !navOpen) return;
    document.getElementById(navId)?.querySelector<HTMLElement>('a[href]')?.focus();
  }, [narrow, navOpen, navId]);

  // Narrow screens: Esc closes the open sidebar and gives the focus back to
  // the menu button (WCAG 2.1.2 / 2.4.3) — only when the focus is in the
  // sidebar or on the button, and not when something else (a dialog) has
  // already handled the key.
  useEffect(() => {
    if (!narrow || !navOpen) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Escape' || e.defaultPrevented) return;
      const active = document.activeElement;
      const inNav = active !== null && document.getElementById(navId)?.contains(active) === true;
      if (!inNav && active !== menuButton.current) return;
      setNavOpen(false);
      menuButton.current?.focus();
    };
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('keydown', onKey);
    };
  }, [narrow, navOpen, navId]);

  // After a navigation the focus moves to the new page's heading (WCAG 2.4.3;
  // not on first load).
  useEffect(() => {
    if (first.current) {
      first.current = false;
      return;
    }
    const root = main.current;
    if (root === null) return;
    // The person may have moved on while the page loaded: only take the focus
    // when it is still on the page body (not in the header, a field, a menu).
    const focusIsFree = () => {
      const active = document.activeElement;
      return active === null || active === document.body || active === root;
    };
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
      if (!focusIsFree()) {
        observer.disconnect();
        return;
      }
      if (focusHeading()) observer.disconnect();
    });
    observer.observe(root, { childList: true, subtree: true });
    const timer = window.setTimeout(() => {
      observer.disconnect();
      if (focusIsFree()) root.focus();
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

  const withNav = !bare && state.status === 'authed';
  const nav = withNav && narrow
    ? {
        open: navOpen,
        controls: navId,
        buttonRef: menuButton,
        onToggle: () => {
          setNavOpen((v) => !v);
        },
      }
    : undefined;
  return (
    <SidebarScopeProvider>
      <div className={styles.app}>
        {!bare && <Header {...(nav ? { nav } : {})} />}
        <div className={styles.frame}>
          {withNav && <Sidebar id={navId} narrow={narrow} open={navOpen} onNavigate={closeNav} />}
          <main id="main" tabIndex={-1} ref={main} className={styles.main}>
            <ErrorBoundary key={location.pathname}>{content}</ErrorBoundary>
          </main>
        </div>
      </div>
    </SidebarScopeProvider>
  );
}
