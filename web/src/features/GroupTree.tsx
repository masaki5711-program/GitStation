// The nested group list of /-/groups (plan 15 §3.3, MR 4). A nested list with
// disclosure buttons (aria-expanded / aria-controls), not role="tree": every
// control is a native button or link, so Tab and Enter/Space operate it with
// no custom key handling. The create buttons are shown only where the role
// the list reports allows them; the server decides again.
import { useId, useState } from 'react';
import { useTranslation } from 'react-i18next';
import { Link } from 'react-router';

import controls from '../components/controls.module.css';
import { encodePath } from './repoUrls';
import { roleLabel } from './labels';
import { mayCreateProject, mayCreateSubgroup, parentIds, type GroupNode } from './groupTreeModel';
import styles from './GroupTree.module.css';

export interface CreateTarget {
  id: number;
  namespaceId: number;
  path: string;
}

interface Actions {
  isAdmin: boolean;
  onNewSubgroup: (target: CreateTarget) => void;
  onNewProject: (target: CreateTarget) => void;
}

export function GroupTree({ roots, ...actions }: { roots: GroupNode[] } & Actions) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState<ReadonlySet<number>>(() => new Set());
  const expandable = parentIds(roots);
  const toggle = (id: number) => {
    setExpanded((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  };
  return (
    <>
      {expandable.length > 0 && (
        <div className={styles.toolbar}>
          <button
            type="button"
            className={controls.button}
            onClick={() => {
              setExpanded(new Set(expandable));
            }}
          >
            {t('groups.expandAll')}
          </button>
          <button
            type="button"
            className={controls.button}
            onClick={() => {
              setExpanded(new Set());
            }}
          >
            {t('groups.collapseAll')}
          </button>
        </div>
      )}
      <ul className={styles.tree}>
        {roots.map((n) => (
          <Node key={n.group.id} node={n} expanded={expanded} toggle={toggle} actions={actions} />
        ))}
      </ul>
    </>
  );
}

function Node({
  node,
  expanded,
  toggle,
  actions,
}: {
  node: GroupNode;
  expanded: ReadonlySet<number>;
  toggle: (id: number) => void;
  actions: Actions;
}) {
  const { t } = useTranslation();
  const childrenId = useId();
  const g = node.group;
  const open = expanded.has(g.id);
  const hasChildren = node.children.length > 0;
  const role = roleLabel(t, g.role);
  const target: CreateTarget = { id: g.id, namespaceId: g.namespace_id, path: g.path };
  const subgroup = mayCreateSubgroup(g.role, actions.isAdmin);
  const project = mayCreateProject(g.role, actions.isAdmin);
  return (
    <li className={styles.node}>
      <div className={styles.row}>
        {hasChildren ? (
          <button
            type="button"
            className={styles.toggle}
            aria-expanded={open}
            aria-controls={childrenId}
            aria-label={t('groups.toggle', { name: g.display_name })}
            onClick={() => {
              toggle(g.id);
            }}
          >
            <span className={styles.chevron} aria-hidden="true" />
          </button>
        ) : (
          <span className={styles.spacer} aria-hidden="true" />
        )}
        <span className={styles.label}>
          <Link to={`/${encodePath(g.path)}`} className={styles.name}>
            {g.display_name}
          </Link>
          <code className={styles.path}>{g.path}</code>
        </span>
        <span className={styles.badges}>
          {hasChildren && (
            <span className={controls.badge}>
              {t('groups.subgroupCount', { count: node.children.length })}
            </span>
          )}
          {role !== null && <span className={controls.badge}>{role}</span>}
        </span>
        {(subgroup || project) && (
          <span className={styles.actions}>
            {subgroup && (
              <button
                type="button"
                className={controls.button}
                aria-label={t('groups.newSubgroupIn', { name: g.display_name })}
                onClick={() => {
                  actions.onNewSubgroup(target);
                }}
              >
                {t('namespace.newSubgroup')}
              </button>
            )}
            {project && (
              <button
                type="button"
                className={controls.button}
                aria-label={t('groups.newProjectIn', { name: g.display_name })}
                onClick={() => {
                  actions.onNewProject(target);
                }}
              >
                {t('namespace.newProject')}
              </button>
            )}
          </span>
        )}
      </div>
      {hasChildren && (
        <ul id={childrenId} className={styles.children} hidden={!open}>
          {node.children.map((c) => (
            <Node key={c.group.id} node={c} expanded={expanded} toggle={toggle} actions={actions} />
          ))}
        </ul>
      )}
    </li>
  );
}
