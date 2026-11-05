// The event types of a webhook as checkboxes grouped by class (docs/api/webhooks.md).
//
// - `*` ("all events") names every integration-class event, so while it is
//   on those boxes show as covered and cannot be toggled; the maintainer and
//   deployment classes are chosen by exact name only.
// - A class the sign-in may not subscribe to is disabled, with the reason
//   tied to its group by `aria-describedby`.
// - A stored entry the list does not name (a family such as `merge_request.*`)
//   is shown as its own box, so an edit does not drop it.
import { useId } from 'react';
import type { TFunction } from 'i18next';
import { useTranslation } from 'react-i18next';

import { useWebhookEventTypes } from '../../api/webhooks';
import { Loading } from '../../components/Loading';
import { QueryError } from '../../components/QueryError';
import controls from '../../components/controls.module.css';
import styles from '../settings/Settings.module.css';

export const ALL_EVENTS = '*';
const CLASSES = ['integration', 'maintainer', 'deployment'] as const;
type EventClass = (typeof CLASSES)[number];

// Literal keys, so the i18n gate sees them used.
const classLabel = (t: TFunction, c: EventClass) =>
  c === 'integration'
    ? { name: t('webhooks.class.integration'), help: t('webhooks.class.integrationHelp') }
    : c === 'maintainer'
      ? { name: t('webhooks.class.maintainer'), help: t('webhooks.class.maintainerHelp') }
      : { name: t('webhooks.class.deployment'), help: t('webhooks.class.deploymentHelp') };

const isFamily = (entry: string) => entry.endsWith('.*') && entry !== ALL_EVENTS;

export function EventTypePicker({
  projectId,
  value,
  onChange,
  error,
}: {
  projectId: number;
  value: string[];
  onChange: (next: string[]) => void;
  /** Why the choice cannot be sent; shown under the group. */
  error?: string | null;
}) {
  const { t } = useTranslation();
  const id = useId();
  const catalog = useWebhookEventTypes(projectId);
  if (catalog.isPending) return <Loading />;
  if (catalog.isError) {
    return (
      <QueryError
        error={catalog.error}
        onRetry={() => {
          void catalog.refetch();
        }}
      />
    );
  }
  const subscribable = (c: EventClass) => catalog.data.classes.find((x) => x.class === c)?.subscribable ?? false;
  const known = new Set(catalog.data.items.map((i) => i.name));
  const all = value.includes(ALL_EVENTS);
  const extras = value.filter((v) => v !== ALL_EVENTS && !known.has(v));
  const toggle = (name: string, on: boolean) => {
    onChange(on ? [...new Set([...value, name])].sort() : value.filter((v) => v !== name));
  };
  const toggleAll = (on: boolean) => {
    if (!on) {
      onChange(value.filter((v) => v !== ALL_EVENTS));
      return;
    }
    // `*` covers the integration class: drop the entries it makes redundant.
    const covered = new Set(catalog.data.items.filter((i) => i.class === 'integration').map((i) => i.name));
    onChange([ALL_EVENTS, ...value.filter((v) => v !== ALL_EVENTS && !covered.has(v) && !isFamily(v))]);
  };
  const helpId = `${id}-help`;
  const errorId = `${id}-error`;
  const allLocked = !subscribable('integration');
  return (
    <fieldset
      className={controls.field}
      aria-describedby={[helpId, error ? errorId : null].filter(Boolean).join(' ')}
    >
      <legend className={controls.label}>{t('webhooks.eventTypes')}</legend>
      <p id={helpId} className={controls.help}>
        {t('webhooks.eventTypesHelp')}
      </p>
      <label className={styles.check}>
        <input
          type="checkbox"
          checked={all}
          disabled={allLocked}
          aria-describedby={`${id}-all-help`}
          onChange={(e) => {
            toggleAll(e.target.checked);
          }}
        />
        <span>{t('webhooks.allEvents')}</span>
      </label>
      <p id={`${id}-all-help`} className={controls.choiceHelp}>
        {allLocked ? t('webhooks.classLocked') : t('webhooks.allEventsHelp')}
      </p>
      {CLASSES.map((c) => {
        const items = catalog.data.items.filter((i) => i.class === c);
        if (items.length === 0) return null;
        const locked = !subscribable(c);
        const covered = all && c === 'integration';
        const label = classLabel(t, c);
        return (
          <fieldset
            key={c}
            className={controls.field}
            aria-describedby={`${id}-${c}-help${locked ? ` ${id}-${c}-locked` : ''}`}
          >
            <legend className={controls.label}>{label.name}</legend>
            <p id={`${id}-${c}-help`} className={controls.help}>
              {label.help}
            </p>
            {locked && (
              <p id={`${id}-${c}-locked`} className={controls.help}>
                {t('webhooks.classLocked')}
              </p>
            )}
            <div className={styles.checks}>
              {items.map((i) => (
                <label key={i.name} className={styles.check}>
                  <input
                    type="checkbox"
                    checked={covered || value.includes(i.name)}
                    disabled={locked || covered}
                    onChange={(e) => {
                      toggle(i.name, e.target.checked);
                    }}
                  />
                  <span>{i.name}</span>
                </label>
              ))}
            </div>
          </fieldset>
        );
      })}
      {extras.length > 0 && (
        <fieldset className={controls.field} aria-describedby={`${id}-extra-help`}>
          <legend className={controls.label}>{t('webhooks.class.other')}</legend>
          <p id={`${id}-extra-help`} className={controls.help}>
            {t('webhooks.class.otherHelp')}
          </p>
          <div className={styles.checks}>
            {extras.map((name) => (
              <label key={name} className={styles.check}>
                <input
                  type="checkbox"
                  checked
                  disabled={all && isFamily(name)}
                  onChange={(e) => {
                    toggle(name, e.target.checked);
                  }}
                />
                <span>{name}</span>
              </label>
            ))}
          </div>
        </fieldset>
      )}
      {error ? (
        <p id={errorId} className={controls.fieldError}>
          {error}
        </p>
      ) : null}
    </fieldset>
  );
}
