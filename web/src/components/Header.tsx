// The common header (ADR-0022 §5): the v1 logo from docs/brand/ (copied to
// src/assets/brand/, checked by tests/brand.test.ts), the language choice
// and, when someone is signed in, the user menu. The header is navy, so it
// shows the dark logo (docs/brand/README.md: never the light version on a
// dark background); its own navy rectangle matches the header. M3 U4: the
// notification bell sits beside the user menu. Plan 15: on narrow screens a
// menu button opens the "My work" sidebar (Layout owns its state).
import { useQueryClient } from '@tanstack/react-query';
import type { RefObject } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import logo from '../assets/brand/gitstation-v1-logo-dark.svg';
import { LANGUAGES, switchLanguage, type Language } from '../i18n';
import { useSessionState } from '../session/session';
import styles from './Header.module.css';
import { NotificationBell } from './NotificationBell';
import { UserMenu } from './UserMenu';

export interface NavToggle {
  open: boolean;
  /** The sidebar's id (aria-controls). */
  controls: string;
  buttonRef: RefObject<HTMLButtonElement | null>;
  onToggle: () => void;
}

export function Header({ nav }: { nav?: NavToggle }) {
  const { t, i18n } = useTranslation();
  const queryClient = useQueryClient();
  const state = useSessionState();
  const current = (LANGUAGES as readonly string[]).includes(i18n.language) ? i18n.language : 'ja';

  const choose = (lng: Language) => {
    void switchLanguage(i18n, lng).then((switched) => {
      // Server messages come back in the new language.
      if (switched) void queryClient.invalidateQueries();
    });
  };

  return (
    <header className={styles.header}>
      <a className={styles.skip} href="#main">
        {t('app.skipToContent')}
      </a>
      {nav && (
        <button
          type="button"
          ref={nav.buttonRef}
          className={styles.menu}
          aria-expanded={nav.open}
          aria-controls={nav.controls}
          onClick={nav.onToggle}
        >
          <svg viewBox="0 0 24 24" width="20" height="20" aria-hidden="true" focusable="false">
            <path fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" d="M4 6h16M4 12h16M4 18h16" />
          </svg>
          <span className={styles.srOnly}>{t('nav.menu')}</span>
        </button>
      )}
      <Link to="/" className={styles.home}>
        <img src={logo} alt={t('app.logoAlt')} className={styles.logo} width={115} height={40} />
      </Link>
      <div className={styles.spacer} />
      <label className={styles.language}>
        <span className={styles.srOnly}>{t('header.language')}</span>
        <select
          className={styles.select}
          value={current}
          onChange={(e) => {
            const value = e.target.value;
            const lng = LANGUAGES.find((l) => l === value);
            if (lng !== undefined) choose(lng);
          }}
        >
          <option value="ja">{t('language.ja')}</option>
          <option value="en">{t('language.en')}</option>
          <option value="id">{t('language.id')}</option>
        </select>
      </label>
      {state.status === 'authed' && <NotificationBell />}
      {state.status === 'authed' && <UserMenu session={state.session} />}
    </header>
  );
}
