// The common header (ADR-0022 §5): the v1 logo from docs/brand/ (copied to
// src/assets/brand/, checked by tests/brand.test.ts), the language choice
// and, when someone is signed in, the user menu. The header is navy, so it
// shows the dark logo (docs/brand/README.md: never the light version on a
// dark background); its own navy rectangle matches the header.
import { useQueryClient } from '@tanstack/react-query';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import logo from '../assets/brand/gitstation-v1-logo-dark.svg';
import { LANGUAGES, rememberLanguage, type Language } from '../i18n';
import { useSessionState } from '../session/session';
import styles from './Header.module.css';
import { UserMenu } from './UserMenu';

export function Header() {
  const { t, i18n } = useTranslation();
  const queryClient = useQueryClient();
  const state = useSessionState();
  const current = (LANGUAGES as readonly string[]).includes(i18n.language) ? i18n.language : 'ja';

  const choose = (lng: Language) => {
    rememberLanguage(lng);
    void i18n.changeLanguage(lng).then(() => {
      // Server messages come back in the new language.
      void queryClient.invalidateQueries();
    });
  };

  return (
    <header className={styles.header}>
      <a className={styles.skip} href="#main">
        {t('app.skipToContent')}
      </a>
      <Link to="/" className={styles.home}>
        <img src={logo} alt={t('app.logoAlt')} className={styles.logo} width={115} height={40} />
      </Link>
      <div className={styles.spacer} />
      <label className={styles.language}>
        <span className={styles.srOnly}>{t('header.language')}</span>
        <select
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
      {state.status === 'authed' && <UserMenu session={state.session} />}
    </header>
  );
}
