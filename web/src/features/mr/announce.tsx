// One polite live region per merge request screen: a thread resolved or
// reopened is said once, wherever the thread is drawn (the region stays
// mounted while threads move between lists or tabs).
import { createContext, useCallback, useContext, useEffect, useRef, useState, type ReactNode } from 'react';

import repo from '../../pages/repo/Repo.module.css';

const AnnounceContext = createContext<(text: string) => void>(() => undefined);

export function useAnnounce(): (text: string) => void {
  return useContext(AnnounceContext);
}

export function Announcer({ children }: { children: ReactNode }) {
  const [message, setMessage] = useState('');
  const timer = useRef<number | undefined>(undefined);
  // Emptied first, the words a moment later: the same words twice are still
  // a change that is read out. The region itself stays mounted.
  const announce = useCallback((text: string) => {
    setMessage('');
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => {
      setMessage(text);
    }, 50);
  }, []);
  useEffect(
    () => () => {
      window.clearTimeout(timer.current);
    },
    [],
  );
  return (
    <AnnounceContext.Provider value={announce}>
      {children}
      <div className={repo.srOnly} aria-live="polite" data-testid="mr-announce">
        {message}
      </div>
    </AnnounceContext.Provider>
  );
}
