// The Markdown renderer is a separate chunk (the parser is most of the code
// the file screens add): it is fetched when the first document is shown.
import { lazy, Suspense } from 'react';

import { Loading } from '../components/Loading';
import type { MarkdownProps } from './Markdown';

const Markdown = lazy(() => import('./Markdown').then((m) => ({ default: m.Markdown })));

export function MarkdownView(props: MarkdownProps) {
  return (
    <Suspense fallback={<Loading />}>
      <Markdown {...props} />
    </Suspense>
  );
}
