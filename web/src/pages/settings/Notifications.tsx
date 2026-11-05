// The signed-in person's notification settings (N1 preferences API), a tab of
// the account settings: mail on or off, the language of the mail (the
// instance default when empty) and the kinds to stop. The kinds are the ones
// the API lists; a kind the API no longer lists is dropped from the request
// (the server refuses an unknown one). Saved with the CSRF header; the result
// is announced in a live region and a refusal shows its reason and request id.
import { useState, type SyntheticEvent } from 'react';
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
            checked={mail}
            onChange={(e) => {
              setMail(e.target.checked);
            }}
          />
          <span>{t('notifications.mail')}</span>
        </label>
        <p className={controls.help}>{t('notifications.mailHelp')}</p>
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
        </select>
      </div>
      <fieldset className={controls.field}>
        <legend className={controls.label}>{t('notifications.muted')}</legend>
        <p className={controls.help}>{t('notifications.mutedHelp')}</p>
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
              <code>{kind}</code>
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
