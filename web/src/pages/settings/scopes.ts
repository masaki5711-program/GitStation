// Token scopes a screen may offer. `admin` is never in these lists: a personal
// token gets it only for an administrator (Tokens.tsx adds it itself), and the
// server refuses it for a service account.
export const PERSONAL_SCOPES = ['read_repository', 'write_repository', 'read_api', 'api'] as const;

export const SERVICE_ACCOUNT_SCOPES = [
  ...PERSONAL_SCOPES,
  'merge_execute',
  'merge_approve',
  'user_email',
  'user_employee_number',
] as const;
