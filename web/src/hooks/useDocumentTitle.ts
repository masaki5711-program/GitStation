import { useEffect } from 'react';

const APP = 'GitStation';

/** The tab title names the page (WCAG 2.4.2); the app name closes it. */
export function useDocumentTitle(title: string | null): void {
  useEffect(() => {
    document.title = title === null || title === '' ? APP : `${title} · ${APP}`;
  }, [title]);
}
