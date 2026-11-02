// "Recently viewed" lives in this browser only (plan 10 §4.2). Storage can
// be missing or throw (private window, policy): then the list is simply empty.
export interface RecentProject {
  path: string;
  name: string;
}

const KEY = 'gitstation.recent';
const MAX = 8;

function isRecent(v: unknown): v is RecentProject {
  return (
    typeof v === 'object' &&
    v !== null &&
    'path' in v &&
    typeof v.path === 'string' &&
    'name' in v &&
    typeof v.name === 'string'
  );
}

export function readRecent(): RecentProject[] {
  try {
    const raw = window.localStorage.getItem(KEY);
    if (raw === null) return [];
    const parsed: unknown = JSON.parse(raw);
    return Array.isArray(parsed) ? parsed.filter(isRecent).slice(0, MAX) : [];
  } catch {
    return [];
  }
}

export function rememberProject(item: RecentProject): void {
  try {
    const rest = readRecent().filter((r) => r.path !== item.path);
    window.localStorage.setItem(KEY, JSON.stringify([item, ...rest].slice(0, MAX)));
  } catch {
    // Not remembered; nothing depends on it.
  }
}
