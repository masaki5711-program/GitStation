// The signed-in person's notification settings (N1 preferences API), a tab of
// the account settings: mail on or off, the language of the mail (the
// instance default when empty) and the kinds to stop. The kinds are the ones
// the API lists; a kind the API no longer lists is dropped from the request
// (the server refuses an unknown one). Saved with the CSRF header; the result
// is announced in a live region and a refusal shows its reason and request id.
import { useState, type SyntheticEvent } from 'react';
import type { TFunction } from 'i18next';
import { useTranslation } from 'react-i18next';

import {
  useNotificationPreferences,
  usePutNotificationPreferences,
  type NotificationPreferences,
} from '../../api/webhooks';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import { Denied, Section, useRefreshSessionOn403 } from './common';
import styles from './Settings.module.css';


/** A translated name for a kind; one the screen does not know is its code. */
function kindLabel(t: TFunction, kind: string): string {
  switch (kind) {
    case 'mr_approved':
      return t('notifications.kinds.mr_approved');
    case 'mr_merged':
      return t('notifications.kinds.mr_merged');
    case 'mr_commented':
      return t('notifications.kinds.mr_commented');
    default:
      return kind;
  }
}

export function Notifications() {
  const { t } = useTranslation();
  const prefs = useNotificationPreferences();
  return (
    <Section id="notifications-title" title={t('notifications.title')}>
      <p className={styles.hint}>{t('notifications.explain')}</p>
      {prefs.isPending && <Loading />}
      {prefs.isError && (
        <QueryError
          error={prefs.error}
          onRetry={() => {
            void prefs.refetch();
          }}
        />
      )}
      {prefs.isSuccess && <PreferencesForm stored={prefs.data} />}
    </Section>
  );
}

function PreferencesForm({ stored }: { stored: NotificationPreferences }) {
  const { t } = useTranslation();
  const refresh = useRefreshSessionOn403();
  const save = usePutNotificationPreferences();
  const [mail, setMail] = useState(stored.email_enabled);
  const [language, setLanguage] = useState(stored.language ?? '');
  // A stored language this screen does not know is kept as it is (offered as
  // its own option), never sent as null or as another value by accident.
  const known = stored.language === null || ['ja', 'en', 'id'].includes(stored.language);
  const [muted, setMuted] = useState<string[]>(stored.muted_kinds);
  const [message, setMessage] = useState('');
  const submit = (e: SyntheticEvent) => {
    e.preventDefault();
    if (save.isPending) return;
    setMessage('');
    save.mutate(
      {
        email_enabled: mail,
        language: language === '' ? null : language,
        muted_kinds: stored.kinds.filter((k) => muted.includes(k)),
      },
      {
        onSuccess: () => {
          setMessage(t('notifications.saved'));
        },
        onError: refresh,
      },
    );
  };
  return (
    <form onSubmit={submit} noValidate className={styles.narrow}>
      <div className={controls.field}>
        <label className={styles.check}>
          <input
            type="checkbox"
            aria-describedby="notification-mail-help"
            checked={mail}
            onChange={(e) => {
              setMail(e.target.checked);
            }}
          />
          <span>{t('notifications.mail')}</span>
        </label>
        <p id="notification-mail-help" className={controls.help}>
          {t('notifications.mailHelp')}
        </p>
      </div>
      <div className={controls.field}>
        <label htmlFor="notification-language" className={controls.label}>
          {t('notifications.language')}
        </label>
        <select
          id="notification-language"
          className={controls.input}
          value={language}
          onChange={(e) => {
            setLanguage(e.target.value);
          }}
        >
          <option value="">{t('notifications.languageDefault')}</option>
          <option value="ja">{t('language.ja')}</option>
          <option value="en">{t('language.en')}</option>
          <option value="id">{t('language.id')}</option>
          {!known && stored.language !== null && <option value={stored.language}>{stored.language}</option>}
        </select>
      </div>
      <fieldset className={controls.field} aria-describedby="notification-muted-help">
        <legend className={controls.label}>{t('notifications.muted')}</legend>
        <p id="notification-muted-help" className={controls.help}>
          {t('notifications.mutedHelp')}
        </p>
        <div className={styles.checks}>
          {stored.kinds.map((kind) => (
            <label key={kind} className={styles.check}>
              <input
                type="checkbox"
                checked={muted.includes(kind)}
                onChange={(e) => {
                  setMuted(e.target.checked ? [...muted, kind] : muted.filter((k) => k !== kind));
                }}
              />
              <span>{kindLabel(t, kind)}</span>
              <code className={styles.muted}>{kind}</code>
            </label>
          ))}
        </div>
      </fieldset>
      {save.isError && <Denied error={save.error} />}
      <button type="submit" className={controls.primary} aria-disabled={save.isPending ? 'true' : undefined}>
        {save.isPending ? t('settings.saving') : t('notifications.save')}
      </button>
      <p role="status" className={styles.muted}>
        {message}
      </p>
    </form>
  );
}
