import { createContext, useContext, type ReactNode } from 'react';

import type { ApiClient } from './client';

const ApiContext = createContext<ApiClient | null>(null);

export function ApiProvider({ client, children }: { client: ApiClient; children: ReactNode }) {
  return <ApiContext.Provider value={client}>{children}</ApiContext.Provider>;
}

/** The app's API client (a missing provider is a programming error). */
export function useApi(): ApiClient {
  const client = useContext(ApiContext);
  if (client === null) {
    throw new Error('useApi outside <ApiProvider>');
  }
  return client;
}
