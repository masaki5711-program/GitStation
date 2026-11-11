// The group tree of /-/groups (plan 15 §3.3, MR 4), built in the browser from
// the flat `GET /api/v1/groups` list. The listed rows carry no parent id, so
// the parent is the row whose path is this row's path minus its last segment.
// A row whose parent is not in the list (the list was cut at its cap) is shown
// at the top with its full path, never dropped.
//
// TODO(plan 15 MR 3): once `?parent_id=` and group visibility exist, read the
// children of a node when it is opened instead of the whole list.
import type { ListedGroup } from '../api/types';
import { parentPath } from '../api/lists';

export interface GroupNode {
  group: ListedGroup;
  children: GroupNode[];
  /** The parent is not in the list (cut by the cap): shown at the top. */
  orphan: boolean;
}

const byPath = (a: GroupNode, b: GroupNode) => a.group.path.localeCompare(b.group.path);

export function buildGroupTree(items: readonly ListedGroup[]): GroupNode[] {
  const nodes = new Map<string, GroupNode>();
  for (const group of items) {
    nodes.set(group.path, { group, children: [], orphan: false });
  }
  const roots: GroupNode[] = [];
  for (const node of nodes.values()) {
    const parent = parentPath(node.group.path);
    const owner = parent === '' ? undefined : nodes.get(parent);
    if (owner === undefined) {
      node.orphan = parent !== '';
      roots.push(node);
    } else {
      owner.children.push(node);
    }
  }
  const sort = (list: GroupNode[]) => {
    list.sort(byPath);
    for (const n of list) sort(n.children);
  };
  sort(roots);
  return roots;
}

/** Every node that has children (for "expand all"). */
export function parentIds(roots: readonly GroupNode[]): number[] {
  const out: number[] = [];
  const walk = (list: readonly GroupNode[]) => {
    for (const n of list) {
      if (n.children.length > 0) {
        out.push(n.group.id);
        walk(n.children);
      }
    }
  };
  walk(roots);
  return out;
}

const RANK: Record<string, number> = {
  guest: 1,
  reporter: 2,
  developer: 3,
  maintainer: 4,
  owner: 5,
};

const OWNER = 5;
const MAINTAINER = 4;

const rank = (role: string | null) => (role === null ? 0 : (RANK[role] ?? 0));

/** `SubgroupCreate`: the group's Owner (effective role) or an administrator.
 * The server decides again; this only hides a button that would be refused. */
export function mayCreateSubgroup(role: string | null, isAdmin: boolean): boolean {
  return isAdmin || rank(role) >= OWNER;
}

/** `ProjectCreate` in a group: Maintainer or above, or an administrator. */
export function mayCreateProject(role: string | null, isAdmin: boolean): boolean {
  return isAdmin || rank(role) >= MAINTAINER;
}
