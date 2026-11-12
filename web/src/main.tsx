import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { BrowserRouter } from 'react-router';

import './styles/tokens.css';
import './styles/global.css';
import { createApiClient } from './api/client';
import { AppProviders, AppRoutes, createQueryClient } from './App';
import { createI18n } from './i18n';
import { applyTheme, storedTheme } from './theme';

// Before anything renders (public/theme-init-*.js already did so before the
// first paint; this also covers a page built without it, e.g. tests).
applyTheme(storedTheme());

async function start(): Promise<void> {
  const root = document.getElementById('app');
  if (root === null) {
    throw new Error('the shell has no #app element');
  }
  const i18n = await createI18n();
  document.documentElement.lang = i18n.language;
  document.title = i18n.t('app.name');
  i18n.on('languageChanged', (lng) => {
    document.documentElement.lang = lng;
  });
  const api = createApiClient({ language: () => i18n.language });
  createRoot(root).render(
    <StrictMode>
      <AppProviders i18n={i18n} api={api} queryClient={createQueryClient()}>
        <BrowserRouter>
          <AppRoutes />
        </BrowserRouter>
      </AppProviders>
    </StrictMode>,
  );
}

start().catch((e: unknown) => {
  // Nothing rendered (no #app, or a failure before the first render): the
  // console is the only place left to say so.
  console.error('gitstation: the app failed to start', e);
});
