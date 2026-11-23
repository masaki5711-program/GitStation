#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly

//! Plan 18 S7: the incremental lint answers exactly what the whole lint
//! answers, over random sequences of trees (documents added, deleted,
//! renamed and rewritten, references and blocks changed, type definitions
//! edited), with a cache large enough to keep everything and one so small
//! it drops nearly everything after each call.

use std::collections::BTreeMap;

use proptest::prelude::*;

use super::{KeyedSource, LintCache, MissingBytes};
use crate::lint::{lint, Source};

const T_QP: &str = include_str!("../../../docs/plan/13-examples/.forge/doctypes/qms.procedure.yml");
const TYPE_PATH: &str = ".forge/doctypes/qms.procedure.yml";

const IDS: &[&str] = &[
    "QP-07-001",
    "QP-07-002",
    "QP-07-003",
    "QP-07-004",
    "QP-07-005",
];
const DIRS: &[&str] = &["qms", "qms/old", "other"];

/// One synthetic document (all synthetic data).
#[derive(Debug, Clone)]
struct Doc {
    id: usize,
    /// The file is named after another id (a mismatch the lint reports).
    named: usize,
    dir: usize,
    related: Vec<usize>,
    supersedes: Option<usize>,
    refs: Vec<(usize, Option<u8>)>,
    steps: Vec<u8>,
    typed: bool,
}

impl Doc {
    fn path(&self) -> String {
        format!("{}/{}.md", DIRS[self.dir], IDS[self.named])
    }

    fn text(&self) -> String {
        let mut s = String::from("---\n");
        if self.typed {
            s.push_str("doctype: qms.procedure\n");
        }
        s.push_str(&format!(
            "id: {}\ntitle: T\nowner: qa-team\nclassification: internal\nreview_cycle: 24M\n",
            IDS[self.id]
        ));
        if !self.related.is_empty() {
            let r: Vec<&str> = self.related.iter().map(|i| IDS[*i]).collect();
            s.push_str(&format!("related: [{}]\n", r.join(", ")));
        }
        if let Some(sup) = self.supersedes {
            s.push_str(&format!("supersedes: {}\n", IDS[sup]));
        }
        s.push_str("---\n\n## 目的\n\n");
        for (id, block) in &self.refs {
            match block {
                Some(b) => s.push_str(&format!("See [[{}#S-{b}]].\n", IDS[*id])),
                None => s.push_str(&format!("See [[{}]].\n", IDS[*id])),
            }
        }
        s.push_str("\n## 手順\n\n");
        for step in &self.steps {
            s.push_str(&format!("::: step S-{step} role=\"qa\"\nDo it.\n:::\n\n"));
        }
        s
    }
}

fn doc() -> impl Strategy<Value = Doc> {
    (
        0..IDS.len(),
        prop::bool::weighted(0.85),
        0..IDS.len(),
        0..DIRS.len(),
        prop::collection::vec(0..IDS.len(), 0..3),
        prop::option::of(0..IDS.len()),
        prop::collection::vec((0..IDS.len(), prop::option::of(1u8..4)), 0..4),
        prop::collection::vec(1u8..4, 0..3),
        prop::bool::weighted(0.9),
    )
        .prop_map(
            |(id, same_name, other, dir, related, supersedes, refs, steps, typed)| Doc {
                id,
                named: if same_name { id } else { other },
                dir,
                related,
                supersedes,
                refs,
                steps,
                typed,
            },
        )
}

/// One edit of the tree.
#[derive(Debug, Clone)]
enum Edit {
    Add(Doc),
    Delete(usize),
    /// The file at the index moves to another directory (same bytes).
    Rename(usize, usize),
    Rewrite(usize, Doc),
    /// The type definition is changed (or changed back).
    ToggleType,
}

fn edit() -> impl Strategy<Value = Edit> {
    prop_oneof![
        3 => doc().prop_map(Edit::Add),
        1 => any::<usize>().prop_map(Edit::Delete),
        1 => (any::<usize>(), 0..DIRS.len()).prop_map(|(i, d)| Edit::Rename(i, d)),
        2 => (any::<usize>(), doc()).prop_map(|(i, d)| Edit::Rewrite(i, d)),
        1 => Just(Edit::ToggleType),
    ]
}

/// The tree: path → bytes.
type Tree = BTreeMap<String, String>;

fn apply(tree: &mut Tree, e: &Edit, type_changed: &mut bool) {
    let nth = |tree: &Tree, i: usize| -> Option<String> {
        if tree.is_empty() {
            None
        } else {
            tree.keys().nth(i % tree.len()).cloned()
        }
    };
    match e {
        Edit::Add(d) => {
            tree.insert(d.path(), d.text());
        }
        Edit::Delete(i) => {
            if let Some(p) = nth(tree, *i) {
                tree.remove(&p);
            }
        }
        Edit::Rename(i, dir) => {
            if let Some(p) = nth(tree, *i) {
                let bytes = tree.remove(&p).unwrap();
                let name = p.rsplit('/').next().unwrap().to_owned();
                tree.insert(format!("{}/{name}", DIRS[*dir]), bytes);
            }
        }
        Edit::Rewrite(i, d) => {
            if let Some(p) = nth(tree, *i) {
                tree.insert(p, d.text());
            }
        }
        Edit::ToggleType => *type_changed = !*type_changed,
    }
}

fn type_text(changed: bool) -> String {
    if changed {
        // Steps lose their required attribute: other problems, same ids.
        T_QP.replace(
            "role: { type: text, required: true }",
            "role: { type: text, required: false }",
        )
    } else {
        T_QP.to_owned()
    }
}

/// The incremental answer for `tree`, reading only the bytes it asks for.
fn incremental(
    cache: &mut LintCache,
    tree: &Tree,
    types: &str,
) -> Result<Vec<crate::lint::FileReport>, MissingBytes> {
    // The key is the content itself: content-addressed by construction.
    let missing: Vec<String> = cache
        .missing(tree.values().map(String::as_str))
        .into_iter()
        .map(str::to_owned)
        .collect();
    let docs: Vec<KeyedSource<'_>> = tree
        .iter()
        .map(|(path, bytes)| KeyedSource {
            path,
            key: bytes,
            bytes: missing.contains(bytes).then_some(bytes.as_bytes()),
        })
        .collect();
    let types = [Source {
        path: TYPE_PATH,
        bytes: types.as_bytes(),
    }];
    cache.lint(&types, &docs).map(|(reports, _)| reports)
}

fn full(tree: &Tree, types: &str) -> Vec<crate::lint::FileReport> {
    let docs: Vec<Source<'_>> = tree
        .iter()
        .map(|(path, bytes)| Source {
            path,
            bytes: bytes.as_bytes(),
        })
        .collect();
    let types = [Source {
        path: TYPE_PATH,
        bytes: types.as_bytes(),
    }];
    lint(&types, &docs)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    #[test]
    fn incremental_lint_equals_full_lint(
        start in prop::collection::vec(doc(), 0..6),
        edits in prop::collection::vec(edit(), 1..14),
    ) {
        let mut tree = Tree::new();
        for d in &start {
            tree.insert(d.path(), d.text());
        }
        let mut type_changed = false;
        let mut roomy = LintCache::new(64 * 1024 * 1024);
        let mut tiny = LintCache::new(1);
        for e in std::iter::once(None).chain(edits.iter().map(Some)) {
            if let Some(e) = e {
                apply(&mut tree, e, &mut type_changed);
            }
            let types = type_text(type_changed);
            let want = full(&tree, &types);
            prop_assert_eq!(&incremental(&mut roomy, &tree, &types).unwrap(), &want);
            prop_assert_eq!(&incremental(&mut tiny, &tree, &types).unwrap(), &want);
            prop_assert!(tiny.bytes() <= 1 || tiny.documents() == 0);
        }
    }
}

#[test]
fn an_unchanged_tree_is_neither_parsed_nor_checked_again() {
    let a = Doc {
        id: 0,
        named: 0,
        dir: 0,
        related: vec![1],
        supersedes: None,
        refs: vec![(1, None)],
        steps: vec![1],
        typed: true,
    };
    let b = Doc {
        id: 1,
        named: 1,
        refs: Vec::new(),
        related: Vec::new(),
        ..a.clone()
    };
    let mut tree = Tree::new();
    tree.insert(a.path(), a.text());
    tree.insert(b.path(), b.text());
    let types = type_text(false);
    let mut cache = LintCache::new(1 << 20);
    let keyed = |tree: &Tree, cache: &LintCache| -> Vec<(String, String, bool)> {
        tree.iter()
            .map(|(p, t)| (p.clone(), t.clone(), cache.holds(t)))
            .collect()
    };
    let ty = [Source {
        path: TYPE_PATH,
        bytes: types.as_bytes(),
    }];
    let run = |cache: &mut LintCache, tree: &Tree| {
        let held = keyed(tree, cache);
        let docs: Vec<KeyedSource<'_>> = held
            .iter()
            .map(|(p, t, h)| KeyedSource {
                path: p,
                key: t,
                bytes: (!h).then_some(t.as_bytes()),
            })
            .collect();
        cache.lint(&ty, &docs).unwrap()
    };
    let (_, first) = run(&mut cache, &tree);
    assert_eq!((first.parsed, first.checked, first.reused), (2, 2, 0));
    let (_, again) = run(&mut cache, &tree);
    assert_eq!((again.parsed, again.checked, again.reused), (0, 0, 2));
    // B is deleted: A looked its id up, so only A is checked again.
    tree.remove(&b.path());
    let (reports, after) = run(&mut cache, &tree);
    assert_eq!((after.parsed, after.checked, after.reused), (0, 1, 0));
    assert_eq!(reports, full(&tree, &types));
}

#[test]
fn a_document_without_bytes_that_is_not_held_is_refused() {
    let mut cache = LintCache::new(1 << 20);
    let docs = [KeyedSource {
        path: "qms/QP-07-001.md",
        key: "abc",
        bytes: None,
    }];
    assert_eq!(
        cache.lint(&[], &docs).map(|_| ()),
        Err(MissingBytes {
            path: "qms/QP-07-001.md".to_owned()
        })
    );
}
