// Token scopes a screen may offer. `admin` is never in these lists: a personal
// token gets it only for an administrator (Tokens.tsx adds it itself), and the
// server refuses it for a service account. `act_as` (acting for a person,
// ADR-0025 §2) is a service account's only, never on an agent's token: the
// server refuses it otherwise (`act_as_requires_service_account`,
// `act_as_agent_token`).
export const PERSONAL_SCOPES = ['read_repository', 'write_repository', 'read_api', 'api'] as const;

export const SERVICE_ACCOUNT_SCOPES = [
  ...PERSONAL_SCOPES,
  'merge_execute',
  'merge_approve',
  'user_email',
  'user_employee_number',
  'act_as',
] as const;

/** The scope acting for a person (ADR-0025 §2). */
export const ACT_AS_SCOPE = 'act_as';

/** The most scopes one token may carry (forge-web `scopes()`: `too_many`). */
export const MAX_TOKEN_SCOPES = 8;
