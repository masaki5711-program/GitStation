// The intent handlers of a link (features/prefetch.ts): hover, focus or a
// touch starts a short timer; leaving (or blur) before it ends cancels it.
import { useQueryClient } from '@tanstack/react-query';
import { useEffect, useMemo, useRef } from 'react';

import { useApi } from '../api/context';
import { INTENT_DELAY_MS, prefetchRoute } from '../features/prefetch';

export interface IntentHandlers {
  onMouseEnter: () => void;
  onMouseLeave: () => void;
  onFocus: () => void;
  onBlur: () => void;
  onTouchStart: () => void;
}

export function usePrefetchIntent(to: string): IntentHandlers {
  const queryClient = useQueryClient();
  const api = useApi();
  const timer = useRef<number | null>(null);
  useEffect(
    () => () => {
      if (timer.current !== null) window.clearTimeout(timer.current);
    },
    [],
  );
  return useMemo(() => {
    const cancel = () => {
      if (timer.current !== null) {
        window.clearTimeout(timer.current);
        timer.current = null;
      }
    };
    const start = () => {
      if (timer.current !== null) return;
      timer.current = window.setTimeout(() => {
        timer.current = null;
        void prefetchRoute(queryClient, api, to);
      }, INTENT_DELAY_MS);
    };
    return { onMouseEnter: start, onMouseLeave: cancel, onFocus: start, onBlur: cancel, onTouchStart: start };
  }, [queryClient, api, to]);
}
