#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly

//! Properties over generated definitions: a DAG is accepted and its plan
//! respects every `needs` and every stage barrier; a back edge is always a
//! cycle naming both ends.

use proptest::prelude::*;

use super::*;

const D: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Job `i` is in stage `stages[i]` (non-decreasing) and needs the jobs in
/// `needs[i]` (all `< i`, in a stage ≤ its own).
fn render(stages: &[usize], needs: &[Vec<usize>]) -> String {
    let count = stages.iter().max().map_or(1, |m| m + 1);
    let names: Vec<String> = (0..count).map(|s| format!("s{s}")).collect();
    let mut text = format!("version: 1\nstages: [{}]\njobs:\n", names.join(", "));
    for (i, (stage, need)) in stages.iter().zip(needs).enumerate() {
        let need: Vec<String> = need.iter().map(|n| format!("j{n}")).collect();
        text.push_str(&format!(
            "  j{i}:\n    stage: s{stage}\n    needs: [{}]\n    image: r/i@sha256:{D}\n    timeout: 1m\n    script: [run]\n",
            need.join(", ")
        ));
    }
    text
}

fn dag() -> impl Strategy<Value = (Vec<usize>, Vec<Vec<usize>>)> {
    (2usize..14).prop_flat_map(|n| {
        let steps = proptest::collection::vec(0usize..2, n);
        let picks = proptest::collection::vec(
            proptest::collection::vec(any::<prop::sample::Index>(), 0..4),
            n,
        );
        (steps, picks).prop_map(|(steps, picks)| {
            let mut stages = Vec::with_capacity(steps.len());
            let mut s = 0;
            for (i, step) in steps.iter().enumerate() {
                if i > 0 {
                    s += step;
                }
                stages.push(s);
            }
            let needs = picks
                .iter()
                .enumerate()
                .map(|(i, ps)| {
                    let mut v: Vec<usize> = if i == 0 {
                        Vec::new()
                    } else {
                        ps.iter().map(|p| p.index(i)).collect()
                    };
                    v.sort_unstable();
                    v.dedup();
                    v
                })
                .collect();
            (stages, needs)
        })
    })
}

proptest! {
    #[test]
    fn a_dag_is_accepted_and_its_plan_respects_needs_and_stages((stages, needs) in dag()) {
        let text = render(&stages, &needs);
        let def = check(text.as_bytes(), &LintContext::with_protected_branches(vec!["main".to_owned()]))
            .unwrap_or_else(|p| panic!("{p:?}\n{text}"));
        let p = plan(&def, &Trigger { event: Event::Push, branch: "main".into(), protected: true }).expect("plan");
        prop_assert_eq!(p.jobs.len(), stages.len());
        let wave = |i: usize| p.jobs.iter().find(|j| j.name == format!("j{i}")).map(|j| j.wave).unwrap();
        let pos = |i: usize| p.jobs.iter().position(|j| j.name == format!("j{i}")).unwrap();
        for (i, need) in needs.iter().enumerate() {
            for n in need {
                prop_assert!(wave(*n) < wave(i));
                prop_assert!(pos(*n) < pos(i));
            }
            for (k, sk) in stages.iter().enumerate() {
                if *sk < stages[i] {
                    prop_assert!(wave(k) < wave(i), "barrier j{} < j{}", k, i);
                }
            }
        }
    }

    #[test]
    fn a_back_edge_is_always_a_cycle((_stages, mut needs) in dag(), a in any::<prop::sample::Index>(), b in any::<prop::sample::Index>()) {
        let n = needs.len();
        let (x, y) = (a.index(n), b.index(n));
        let (lo, hi) = (x.min(y), x.max(y));
        // One stage, so no barrier gets in the way: hi needs lo, lo needs hi.
        let stages = vec![0; n];
        if !needs[hi].contains(&lo) { needs[hi].push(lo); }
        needs[lo].push(hi);
        let text = render(&stages, &needs);
        let found = lint(text.as_bytes(), &LintContext::with_protected_branches(vec!["main".to_owned()]));
        let cyc: Vec<String> = found.iter().filter(|p| p.reason == "needs_cycle").filter_map(|p| p.subject.clone()).collect();
        prop_assert!(cyc.contains(&format!("j{lo}")), "{:?}", found);
        prop_assert!(cyc.contains(&format!("j{hi}")), "{:?}", found);
    }
}
