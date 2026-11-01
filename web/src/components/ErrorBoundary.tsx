// A render error in a page (a bug): shown as the generic error, logged once
// with the original error. Reset by the route (the Layout keys it by path).
import { Component, type ReactNode } from 'react';

import { ErrorNotice } from './ErrorNotice';

export class ErrorBoundary extends Component<{ children: ReactNode }, { failed: boolean }> {
  override state = { failed: false };

  static getDerivedStateFromError(): { failed: boolean } {
    return { failed: true };
  }

  override componentDidCatch(error: unknown): void {
    console.error('gitstation: a page failed to render', error);
  }

  override render(): ReactNode {
    if (this.state.failed) {
      return <ErrorNotice failure={{ kind: 'unexpected', status: null, requestId: null }} />;
    }
    return this.props.children;
  }
}
