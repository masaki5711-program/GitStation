// A router link that prefetches its screen on intent (hover, focus, touch;
// features/prefetch.ts). Otherwise exactly `Link`: same props, same markup.
import type { FocusEvent, MouseEvent, TouchEvent } from 'react';
import { Link, type LinkProps } from 'react-router';

import { usePrefetchIntent } from '../hooks/usePrefetchIntent';

export function PrefetchLink(props: LinkProps & { to: string }) {
  const { onMouseEnter, onMouseLeave, onFocus, onBlur, onTouchStart, ...rest } = props;
  const intent = usePrefetchIntent(props.to);
  return (
    <Link
      {...rest}
      onMouseEnter={(e: MouseEvent<HTMLAnchorElement>) => {
        intent.onMouseEnter();
        onMouseEnter?.(e);
      }}
      onMouseLeave={(e: MouseEvent<HTMLAnchorElement>) => {
        intent.onMouseLeave();
        onMouseLeave?.(e);
      }}
      onFocus={(e: FocusEvent<HTMLAnchorElement>) => {
        intent.onFocus();
        onFocus?.(e);
      }}
      onBlur={(e: FocusEvent<HTMLAnchorElement>) => {
        intent.onBlur();
        onBlur?.(e);
      }}
      onTouchStart={(e: TouchEvent<HTMLAnchorElement>) => {
        intent.onTouchStart();
        onTouchStart?.(e);
      }}
    />
  );
}
