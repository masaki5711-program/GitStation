// Copy a text to the clipboard. The clipboard can be missing (plain http is
// not a secure context) or refuse: say so; the text stays on screen.
import { useState } from 'react';
import { useTranslation } from 'react-i18next';

import styles from './controls.module.css';

export function CopyButton({ text, label }: { text: string; label: string }) {
  const { t } = useTranslation();
  const [state, setState] = useState<'idle' | 'copied' | 'failed'>('idle');
  const copy = () => {
    // Empty first: the same message twice in a row is not announced again.
    setState('idle');
    const clipboard = (navigator as Partial<Navigator>).clipboard;
    if (clipboard === undefined) {
      setState('failed');
      return;
    }
    clipboard.writeText(text).then(
      () => {
        setState('copied');
      },
      () => {
        setState('failed');
      },
    );
  };
  const status =
    state === 'copied' ? t('common.copied') : state === 'failed' ? t('common.copyFailed') : '';
  return (
    <>
      <button type="button" className={styles.button} onClick={copy}>
        {label}
      </button>
      <span role="status">{status}</span>
    </>
  );
}
