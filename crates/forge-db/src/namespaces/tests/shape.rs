//! Property test: a move preserves the shape of the subtree. Random group
//! trees with projects at random groups (a deterministic proptest runner)
//! are built through the commands, the root is moved below another group,
//! and the subtree read relative to its root — (relative path, kind,
//! relative depth) — is the same before and after; every node has one
//! `moved` tombstone at its old path and the whole tree stays consistent.

use proptest::strategy::{Strategy, ValueTree};
use proptest::test_runner::TestRunner;

use super::*;

/// The subtree below `root` relative to it, sorted.
async fn relative(su: &mut PgConnection, root: NamespaceId) -> Vec<(String, String, i32)> {
    sqlx::query_as(
        "WITH RECURSIVE sub (id) AS (SELECT $1::bigint UNION ALL \
           SELECT n.id FROM forge.namespaces n JOIN sub s ON n.parent_id = s.id) \
         SELECT substr(n.full_path, length(r.full_path) + 1), n.kind, (n.depth - r.depth)::int \
         FROM sub JOIN forge.namespaces n ON n.id = sub.id \
         CROSS JOIN forge.namespaces r WHERE r.id = $1 ORDER BY 1",
    )
    .bind(root.get())
    .fetch_all(su)
    .await
    .expect("subtree")
}

#[tokio::test]
async fn a_move_preserves_the_subtree_shape() {
    let t = FreshDb::migrated().await;
    let alice = user(&t, "alice").await;
    let home = group(&t, "home", None, alice.user_id).await;
    let mut runner = TestRunner::deterministic();
    // Up to 7 groups below the root (parent index per group) and up to 6
    // projects (the group index they go into).
    let shapes = (
        proptest::collection::vec(0usize..8, 0..7),
        proptest::collection::vec(0usize..8, 0..6),
    );
    for case in 0..6 {
        let (parents, projects_at) = shapes.new_tree(&mut runner).expect("shape").current();
        let root = group(&t, &format!("r{case}"), None, alice.user_id).await;
        let mut groups_ns = vec![root.namespace_id];
        for (i, p) in parents.iter().enumerate() {
            let parent = groups_ns[p % groups_ns.len()];
            groups_ns.push(
                group(&t, &format!("g{i}"), Some(parent), alice.user_id)
                    .await
                    .namespace_id,
            );
        }
        for (i, at) in projects_at.iter().enumerate() {
            project(
                &t,
                &format!("p{i}"),
                groups_ns[at % groups_ns.len()],
                alice.user_id,
            )
            .await;
        }
        let mut su = t.superuser().await;
        let before = relative(&mut su, root.namespace_id).await;
        let (moved, _) = move_group(
            &t,
            root.group_id,
            alice.user_id,
            to(Some(home.namespace_id), &format!("m{case}")),
        )
        .await
        .unwrap_or_else(|e| panic!("case {case}: {e}"));
        assert_eq!(moved.new_path, format!("home/m{case}"));
        assert_eq!(
            moved.summary.nodes,
            i64::try_from(before.len()).expect("len")
        );
        let after = relative(&mut su, root.namespace_id).await;
        assert_eq!(before, after, "case {case}: the shape changed");
        let stones: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM forge.namespace_tombstones \
             WHERE old_path = $1 OR old_path LIKE $1 || '/%'",
        )
        .bind(format!("r{case}"))
        .fetch_one(&mut su)
        .await
        .expect("tombstones");
        assert_eq!(
            stones, moved.summary.nodes,
            "case {case}: one tombstone per node"
        );
        assert_tree_consistent(&mut su).await;
    }
    t.drop_now().await;
}
