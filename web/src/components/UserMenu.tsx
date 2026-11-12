// The user menu: who is signed in and how to sign out. Sign-out is a real
// form POST to `/-/auth/logout` with the CSRF token in the `csrf` field
// (ADR-0021 §8): the server ends the session and answers 303 to the
// identity provider's end-session URL, so the browser itself follows it.
// The theme choice (src/theme.ts) applies the moment a radio is picked.
import { useEffect, useId, useRef, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import type { Session } from '../api/types';
import { type Theme, chooseTheme, storedTheme } from '../theme';
import styles from './UserMenu.module.css';

export function UserMenu({ session }: { session: Session }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const panelId = useId();
  const themeName = useId();
  const [theme, setTheme] = useState<Theme>(storedTheme);
  // String literals so the dictionary gate sees every key.
  const themeLabels: Record<Theme, string> = {
    system: t('theme.system'),
    white: t('theme.white'),
    navy: t('theme.navy'),
    dark: t('theme.dark'),
  };

  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        setOpen(false);
        trigger.current?.focus();
      }
    };
    const onPointer = (e: PointerEvent) => {
      if (root.current !== null && e.target instanceof Node && !root.current.contains(e.target)) {
        setOpen(false);
      }
    };
    document.addEventListener('keydown', onKey);
    document.addEventListener('pointerdown', onPointer);
    return () => {
      document.removeEventListener('keydown', onKey);
      document.removeEventListener('pointerdown', onPointer);
    };
  }, [open]);

  const { user } = session;
  return (
    <div
      className={styles.root}
      ref={root}
      onBlur={(e) => {
        // Focus moved to something outside the menu (a click that focuses
        // nothing has no relatedTarget: the pointer rule above covers it).
        const next = e.relatedTarget;
        if (next instanceof Node && !e.currentTarget.contains(next)) setOpen(false);
      }}
    >
      <button
        type="button"
        ref={trigger}
        className={styles.trigger}
        aria-expanded={open}
        aria-controls={panelId}
        onClick={() => {
          setOpen((v) => !v);
        }}
      >
        {user.display_name}
        <span aria-hidden="true" className={styles.caret} />
      </button>
      <div id={panelId} className={styles.panel} hidden={!open}>
        <p className={styles.who}>
          <span>{t('header.signedInAs')}</span> <code>{user.username}</code>
        </p>
        <ul className={styles.links}>
          <li>
            <Link
              className={styles.link}
              to={`/${encodeURIComponent(user.username)}/-/settings`}
              onClick={() => {
                setOpen(false);
              }}
            >
              {t('header.account')}
            </Link>
          </li>
          {user.is_admin && (
            <li>
              <Link
                className={styles.link}
                to={`/${encodeURIComponent(user.username)}/-/settings?tab=admin`}
                onClick={() => {
                  setOpen(false);
                }}
              >
                {t('header.admin')}
              </Link>
            </li>
          )}
        </ul>
        <fieldset className={styles.themes}>
          <legend className={styles.themesLegend}>{t('theme.legend')}</legend>
          {(Object.keys(themeLabels) as Theme[]).map((value) => (
            <label key={value} className={styles.themeChoice}>
              <input
                type="radio"
                name={themeName}
                value={value}
                checked={theme === value}
                onChange={() => {
                  setTheme(value);
                  chooseTheme(value);
                }}
              />
              <span aria-hidden="true" className={styles.swatch} data-swatch={value} />
              {themeLabels[value]}
            </label>
          ))}
        </fieldset>
        <form method="post" action="/-/auth/logout">
          <input type="hidden" name="csrf" value={session.csrf_token} />
          <button type="submit" className={styles.logout}>
            {t('header.logout')}
          </button>
        </form>
      </div>
    </div>
  );
}
