import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { BrowserRouter } from 'react-router';

import './styles/tokens.css';
import './styles/global.css';
import { createApiClient } from './api/client';
import { AppProviders, AppRoutes, createQueryClient } from './App';
import { createI18n } from './i18n';
import { applyTheme, storedTheme } from './theme';

// Before anything renders, so the first painted screen is already in the
// chosen theme (src/theme.ts says why this cannot be an inline script).
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

void start();
