// A render error in a page (a bug, or a screen chunk that did not load):
// shown as the generic error, logged once with the original error. Reset by
// the route (the Layout keys it by path), or by `onRetry` when given (a lazy
// screen loads its chunk again).
import { Component, type ReactNode } from 'react';

import { ErrorNotice } from './ErrorNotice';

export class ErrorBoundary extends Component<{ children: ReactNode; onRetry?: () => void }, { failed: boolean }> {
  override state = { failed: false };

  static getDerivedStateFromError(): { failed: boolean } {
    return { failed: true };
  }

  override componentDidCatch(error: unknown): void {
    console.error('gitstation: a page failed to render', error);
  }

  override render(): ReactNode {
    if (this.state.failed) {
      const retry = this.props.onRetry;
      return (
        <ErrorNotice
          failure={{ kind: 'unexpected', status: null, requestId: null }}
          {...(retry === undefined
            ? {}
            : {
                onRetry: () => {
                  retry();
                  this.setState({ failed: false });
                },
              })}
        />
      );
    }
    return this.props.children;
  }
}
