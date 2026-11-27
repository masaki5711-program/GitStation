// The marks of an agent part (plan 16 §6): its capabilities, its channel and
// its kind. Every mark is a word on screen (and so read aloud); the colour
// and the icon only repeat it (WCAG 1.4.1). A capability list is a list
// named "Capabilities", so a screen reader says how many there are.
import type { TFunction } from 'i18next';
import { useTranslation } from 'react-i18next';

import { orderedCapabilities } from '../../api/marketplace';
import styles from './Marks.module.css';

/** The words of a capability (literal keys: the dictionary gate reads them). */
export function capabilityLabel(t: TFunction, cap: string): string {
  switch (cap) {
    case 'exec':
      return t('market.cap.exec');
    case 'network':
      return t('market.cap.network');
    case 'permissions':
      return t('market.cap.permissions');
    case 'fs-write':
      return t('market.cap.fsWrite');
    default:
      return cap;
  }
}

export function kindLabel(t: TFunction, kind: string): string {
  switch (kind) {
    case 'skill':
      return t('market.kind.skill');
    case 'agent':
      return t('market.kind.agent');
    case 'rule':
      return t('market.kind.rule');
    case 'hook':
      return t('market.kind.hook');
    case 'command':
      return t('market.kind.command');
    case 'mcp':
      return t('market.kind.mcp');
    case 'adapter':
      return t('market.kind.adapter');
    default:
      return kind;
  }
}

const TONE: Record<string, string | undefined> = {
  exec: styles.capExec,
  network: styles.capNetwork,
  permissions: styles.capPermissions,
  'fs-write': styles.capWrite,
};

/** A small icon per capability (decorative: the word beside it is the mark). */
function CapIcon({ cap }: { cap: string }) {
  const d =
    cap === 'exec'
      ? 'M4 6l5 5-5 5M11 18h9'
      : cap === 'network'
        ? 'M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18ZM3 12h18M12 3c3 3 3 15 0 18M12 3c-3 3-3 15 0 18'
        : cap === 'permissions'
          ? 'M12 3l7 3v6c0 4-3 7-7 9-4-2-7-5-7-9V6Z'
          : 'M5 19h14M7 15l9-9 3 3-9 9H7Z';
  return (
    <svg
      className={styles.icon}
      viewBox="0 0 24 24"
      width="14"
      height="14"
      aria-hidden="true"
      focusable="false"
      fill="none"
      stroke="currentColor"
      strokeWidth="2"
      strokeLinecap="round"
      strokeLinejoin="round"
    >
      <path d={d} />
    </svg>
  );
}

/**
 * A part's capabilities as words. `label` names the list (default
 * "Capabilities"); with no capability, one quiet line says so (`none`
 * replaces it, or `null` shows nothing).
 */
export function CapabilityMarks({
  caps,
  label,
  none,
}: {
  caps: readonly string[];
  label?: string;
  none?: string | null;
}) {
  const { t } = useTranslation();
  const ordered = orderedCapabilities(caps);
  if (ordered.length === 0) {
    if (none === null) return null;
    return <p className={styles.none}>{none ?? t('market.cap.none')}</p>;
  }
  return (
    <ul className={styles.marks} aria-label={label ?? t('market.capabilities')}>
      {ordered.map((c) => (
        <li key={c} className={TONE[c] ?? styles.capOther}>
          <CapIcon cap={c} />
          <span>{capabilityLabel(t, c)}</span>
        </li>
      ))}
    </ul>
  );
}

/** `beta` / `stable` as a word on a colour. */
export function ChannelBadge({ channel }: { channel: string }) {
  return (
    <span className={channel === 'stable' ? styles.stable : styles.beta} data-channel={channel} lang="en">
      {channel}
    </span>
  );
}

/** A revoked version: a word, never a colour alone. */
export function RevokedBadge() {
  const { t } = useTranslation();
  return <span className={styles.revoked}>{t('market.revoked')}</span>;
}
