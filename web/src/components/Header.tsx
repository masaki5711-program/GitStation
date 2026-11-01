// The common header (ADR-0022 §5): the v1 logo from docs/brand/ (copied to
// src/assets/brand/, checked by tests/brand.test.ts). The header is navy, so
// it shows the dark version (docs/brand/README.md: never the light version
// on a dark background); its own navy rectangle matches the header.
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import logo from '../assets/brand/gitstation-v1-logo-dark.svg';
import styles from './Header.module.css';

export function Header() {
  const { t } = useTranslation();
  return (
    <header className={styles.header}>
      <a className={styles.skip} href="#main">
        {t('app.skipToContent')}
      </a>
      <Link to="/" className={styles.home}>
        <img src={logo} alt={t('app.logoAlt')} className={styles.logo} width={115} height={40} />
      </Link>
    </header>
  );
}
