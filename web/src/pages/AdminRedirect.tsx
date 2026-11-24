// `/-/admin` (ADR-0021 amendment 2026-11-24): the portal menu's
// administrator link. It sends the signed-in person on to their own
// administrator settings; the settings page shows its tokens tab instead
// when the person is not an administrator (the server decides every call).
import { Navigate } from 'react-router';

import { useSession } from '../session/session';

export function AdminRedirect() {
  const session = useSession();
  return <Navigate replace to={`/${encodeURIComponent(session.user.username)}/-/settings?tab=admin`} />;
}
