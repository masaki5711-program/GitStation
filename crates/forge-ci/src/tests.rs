#![allow(clippy::expect_used, clippy::panic, clippy::unwrap_used)] // tests may fail loudly

//! One test per lint reason (stable codes), the plan, and the error mapping.
//! All data is synthetic.

use super::*;
use forge_errors::ErrorCode;

const D: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn image() -> String {
    format!("registry.internal/rust@sha256:{D}")
}

/// A valid file: build → test (two jobs, one needing the other) → deploy
/// (secrets, main only, no merge requests).
fn valid() -> String {
    format!(
        "version: 1
stages: [build, test, deploy]
jobs:
  build:
    stage: build
    image: {img}
    timeout: 30m
    script:
      - cargo build --release
    tags: [linux]
    artifacts:
      paths: [target/release/app]
  unit:
    stage: test
    image: {img}
    timeout: 1h
    script: [cargo test]
  integration:
    stage: test
    needs: [unit]
    image: {img}
    timeout: 45m
    script: [cargo test -- --ignored]
  deploy:
    stage: deploy
    image: {img}
    timeout: 10m
    script: [./deploy.sh]
    secrets: [DEPLOY_KEY]
    rules:
      branches: [main]
      merge_requests: false
",
        img = image()
    )
}

fn problems(text: &str) -> Vec<Problem> {
    lint(text.as_bytes(), &LintContext::default())
}

/// The reasons found, in order.
fn reasons(text: &str) -> Vec<&'static str> {
    problems(text).iter().map(|p| p.reason).collect()
}

/// A one-job file with `job` as the job's body (indented 4).
fn one_job(job: &str) -> String {
    let body: String = job.lines().map(|l| format!("    {l}\n")).collect();
    format!("version: 1\nstages: [build]\njobs:\n  a:\n{body}")
}

fn base_job() -> String {
    format!(
        "stage: build\nimage: {}\ntimeout: 5m\nscript: [make]\n",
        image()
    )
}

#[test]
fn a_valid_definition_has_no_problems() {
    let def = check(valid().as_bytes(), &LintContext::default()).expect("valid");
    assert_eq!(def.stages, ["build", "test", "deploy"]);
    let names: Vec<&str> = def.jobs.iter().map(|j| j.name.as_str()).collect();
    assert_eq!(names, ["build", "unit", "integration", "deploy"]);
    let deploy = &def.jobs[3];
    assert_eq!(deploy.timeout_secs, 600);
    assert_eq!(deploy.secrets, ["DEPLOY_KEY"]);
    assert!(!deploy.rules.merge_requests);
    assert_eq!(def.jobs[0].artifacts, ["target/release/app"]);
    // The script is kept as written, never interpreted.
    assert_eq!(def.jobs[2].script, ["cargo test -- --ignored"]);
}

#[test]
fn a_pipeline_definition_with_a_cycle_is_ci_config_invalid() {
    let text = valid().replace(
        "  unit:\n    stage: test\n",
        "  unit:\n    stage: test\n    needs: [integration]\n",
    );
    let found = problems(&text);
    assert!(found.iter().any(|p| p.reason == "needs_cycle"), "{found:?}");
    let err = invalid(&found);
    assert_eq!(err.code(), ErrorCode::CiConfigInvalid);
    assert_eq!(err.code().id(), "FG-CI-0001");
    assert_eq!(err.code().http_status(), 422);
    assert!(err
        .context()
        .iter()
        .any(|(k, v)| *k == "reason" && v == "needs_cycle"));
}

#[test]
fn needs_cycle_names_every_job_on_the_cycle_and_not_the_rest() {
    let text = valid().replace(
        "  unit:\n    stage: test\n",
        "  unit:\n    stage: test\n    needs: [integration]\n",
    );
    let on_cycle: Vec<String> = problems(&text)
        .into_iter()
        .filter(|p| p.reason == "needs_cycle")
        .filter_map(|p| p.subject)
        .collect();
    assert_eq!(on_cycle, ["unit", "integration"]); // sorted by line
}

#[test]
fn a_job_needing_itself_is_a_cycle() {
    let text = one_job(&format!("{}needs: [a]\n", base_job()));
    assert_eq!(reasons(&text), ["needs_cycle"]);
}

#[test]
fn needs_on_a_missing_job_is_needs_unknown() {
    let text = valid().replace("needs: [unit]", "needs: [nope]");
    let found = problems(&text);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].reason, "needs_unknown");
    assert_eq!(found[0].subject.as_deref(), Some("nope"));
    assert_eq!(found[0].line, 18); // the job's key `integration:`
}

#[test]
fn needs_on_a_later_stage_is_refused() {
    let text = valid().replace(
        "    stage: build\n    image",
        "    stage: build\n    needs: [unit]\n    image",
    );
    assert_eq!(reasons(&text), ["needs_later_stage"]);
}

#[test]
fn a_need_named_twice_is_needs_duplicate() {
    let text = valid().replace("needs: [unit]", "needs: [unit, unit]");
    assert_eq!(reasons(&text), ["needs_duplicate"]);
}

#[test]
fn too_many_needs_is_refused() {
    let list: Vec<String> = (0..=MAX_NEEDS).map(|i| format!("j{i}")).collect();
    let text = one_job(&format!("{}needs: [{}]\n", base_job(), list.join(", ")));
    assert!(reasons(&text).contains(&"too_many_needs"));
}

#[test]
fn a_missing_timeout_is_timeout_missing() {
    let text = one_job(&base_job().replace("timeout: 5m\n", ""));
    assert_eq!(reasons(&text), ["timeout_missing"]);
}

#[test]
fn a_timeout_over_four_hours_is_timeout_too_long() {
    assert_eq!(
        reasons(&one_job(&base_job().replace("5m", "241m"))),
        ["timeout_too_long"]
    );
    assert!(problems(&one_job(&base_job().replace("5m", "4h"))).is_empty());
}

#[test]
fn a_timeout_in_another_form_is_timeout_invalid() {
    for bad in ["5", "0m", "1d", "1h30m", "-5m", "m", "9999999s"] {
        let text = one_job(&base_job().replace("5m", bad));
        assert_eq!(reasons(&text), ["timeout_invalid"], "{bad}");
    }
}

#[test]
fn an_image_without_a_digest_is_image_not_pinned() {
    for bad in [
        "registry.internal/rust:1.88".to_owned(),
        format!("registry.internal/rust@sha256:{}", &D[..63]),
        format!("registry.internal/rust@sha256:{}", D.to_uppercase()),
        format!("\"@sha256:{D}\""),
        format!("registry.internal/rust:latest@sha512:{D}"),
    ] {
        let text = one_job(&base_job().replace(&image(), &bad));
        assert_eq!(reasons(&text), ["image_not_pinned"], "{bad}");
    }
    // A tag and a digest together is pinned (the digest decides).
    let ok =
        one_job(&base_job().replace(&image(), &format!("registry.internal/rust:1.88@sha256:{D}")));
    assert!(problems(&ok).is_empty());
}

#[test]
fn a_missing_image_is_field_missing() {
    let text = one_job(&base_job().replace(&format!("image: {}\n", image()), ""));
    let found = problems(&text);
    assert_eq!(found.len(), 1);
    assert_eq!(
        (found[0].reason, found[0].subject.as_deref()),
        ("field_missing", Some("image"))
    );
}

#[test]
fn a_secret_on_a_job_that_runs_for_merge_requests_is_refused() {
    let text = valid().replace("      merge_requests: false\n", "");
    let found = problems(&text);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].reason, "secret_on_unprotected_ref");
    assert_eq!(found[0].subject.as_deref(), Some("merge_requests"));
}

#[test]
fn a_secret_on_a_wildcard_branch_is_refused_without_the_protected_list() {
    let text = valid().replace("branches: [main]", "branches: [\"release/*\"]");
    assert_eq!(reasons(&text), ["secret_on_unprotected_ref"]);
    // Default rules (every branch) are refused too.
    let text = valid().replace(
        "    rules:\n      branches: [main]\n      merge_requests: false\n",
        "",
    );
    assert_eq!(
        reasons(&text),
        ["secret_on_unprotected_ref", "secret_on_unprotected_ref"]
    );
}

#[test]
fn a_secret_lint_uses_the_protected_branches_when_known() {
    let ctx = |list: &[&str]| LintContext {
        protected_branches: Some(list.iter().map(|s| (*s).to_owned()).collect()),
    };
    let release = valid().replace("branches: [main]", "branches: [main, \"release/*\"]");
    assert!(lint(release.as_bytes(), &ctx(&["main", "release/*"])).is_empty());
    let found = lint(release.as_bytes(), &ctx(&["main"]));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].subject.as_deref(), Some("release/*"));
    // An exact branch must be protected (directly or by a pattern).
    let feature = valid().replace("branches: [main]", "branches: [feature-x]");
    assert_eq!(lint(feature.as_bytes(), &ctx(&["main"])).len(), 1);
    assert!(lint(feature.as_bytes(), &ctx(&["feature-*"])).is_empty());
}

#[test]
fn duplicate_job_names_are_refused_as_duplicate_key() {
    let text = format!("{}  build:\n    stage: build\n", valid());
    assert_eq!(reasons(&text), ["duplicate_key"]);
}

#[test]
fn an_empty_script_is_script_empty() {
    for script in ["script: []\n", "script:\n", "script: [\"  \"]\n", ""] {
        let text = one_job(&base_job().replace("script: [make]\n", script));
        assert_eq!(reasons(&text), ["script_empty"], "{script:?}");
    }
}

#[test]
fn too_many_script_lines_is_refused() {
    let lines = vec!["make"; MAX_SCRIPT_LINES + 1].join(", ");
    let text = one_job(&base_job().replace("[make]", &format!("[{lines}]")));
    assert_eq!(reasons(&text), ["too_many_script_lines"]);
}

#[test]
fn too_many_jobs_is_refused() {
    let mut text = String::from("version: 1\nstages: [build]\njobs:\n");
    for i in 0..=MAX_JOBS {
        text.push_str(&format!("  j{i}:\n"));
        for l in base_job().lines() {
            text.push_str(&format!("    {l}\n"));
        }
    }
    assert_eq!(reasons(&text), ["too_many_jobs"]);
}

#[test]
fn too_many_stages_is_refused() {
    let stages: Vec<String> = (0..=MAX_STAGES).map(|i| format!("s{i}")).collect();
    let text = one_job(&base_job()).replace("[build]", &format!("[build, {}]", stages.join(", ")));
    assert_eq!(reasons(&text), ["too_many_stages"]);
}

#[test]
fn a_file_over_the_limit_is_too_large() {
    let text = format!("{}#{}\n", valid(), "x".repeat(MAX_BYTES));
    assert_eq!(reasons(&text), ["too_large"]);
}

#[test]
fn nesting_beyond_the_bound_is_too_deep() {
    let deep = format!("{}{}", "[".repeat(40), "]".repeat(40));
    let text = one_job(&base_job()).replace("tags", "x") + &format!("    tags: {deep}\n");
    assert_eq!(reasons(&text), ["too_deep"]);
}

#[test]
fn bytes_that_are_not_utf8_are_not_utf8() {
    let found = lint(b"version: 1\n\xff\n", &LintContext::default());
    assert_eq!(found[0].reason, "not_utf8");
}

#[test]
fn yaml_features_that_change_meaning_are_refused() {
    let anchor = valid().replace("    timeout: 30m", "    timeout: &t 30m");
    assert_eq!(reasons(&anchor), ["yaml_anchor"]);
    assert_eq!(
        reasons("version: 1\n---\nversion: 1\n"),
        ["multiple_documents"]
    );
    assert_eq!(reasons("version: !!str 1\n"), ["yaml_tag"]);
    assert_eq!(reasons("version: [1\n"), ["syntax"]);
}

#[test]
fn an_empty_or_non_mapping_file_is_refused() {
    assert_eq!(reasons(""), ["empty_file"]);
    assert_eq!(reasons("- a\n"), ["not_a_mapping"]);
}

#[test]
fn unknown_keys_are_refused_at_every_level() {
    let top = format!("{}variables: {{}}\n", valid());
    assert_eq!(reasons(&top), ["key_unknown"]);
    let job = one_job(&format!("{}when: always\n", base_job()));
    assert_eq!(problems(&job)[0].subject.as_deref(), Some("when"));
    let rules = one_job(&format!("{}rules:\n  tags: [x]\n", base_job()));
    assert_eq!(reasons(&rules), ["key_unknown"]);
    let artifacts = one_job(&format!("{}artifacts:\n  expire: 1d\n", base_job()));
    assert_eq!(reasons(&artifacts), ["key_unknown"]);
}

#[test]
fn version_stages_and_jobs_are_required() {
    let found = problems("stages: [a]\njobs:\n");
    assert!(found
        .iter()
        .any(|p| p.subject.as_deref() == Some("version")));
    assert!(reasons("version: 2\nstages: [a]\njobs: {}\n").contains(&"version_unsupported"));
    assert!(reasons("version: 1\njobs: {}\n").contains(&"field_missing"));
    assert!(reasons("version: 1\nstages: []\njobs: {}\n").contains(&"stages_empty"));
    assert!(reasons("version: 1\nstages: [a]\njobs: {}\n").contains(&"jobs_empty"));
}

#[test]
fn stage_names_are_checked() {
    assert!(
        reasons(&valid().replace("[build, test, deploy]", "[build, test, deploy, test]"))
            .contains(&"stage_duplicate")
    );
    assert!(
        reasons(&valid().replace("[build, test, deploy]", "[build, test, deploy, Bad]"))
            .contains(&"stage_name_invalid")
    );
    assert_eq!(
        reasons(&one_job(&base_job().replace("stage: build", "stage: nope"))),
        ["stage_unknown"]
    );
    let missing = one_job(&base_job().replace("stage: build\n", ""));
    assert_eq!(problems(&missing)[0].subject.as_deref(), Some("stage"));
}

#[test]
fn names_paths_and_patterns_are_checked() {
    let bad_job = valid()
        .replace("  unit:\n", "  Unit:\n")
        .replace("needs: [unit]", "needs: []");
    assert_eq!(reasons(&bad_job), ["job_name_invalid"]);
    assert_eq!(
        reasons(&one_job(&format!("{}tags: [\"Linux box\"]\n", base_job()))),
        ["tag_invalid"]
    );
    assert_eq!(
        reasons(&one_job(&format!(
            "{}tags: [{}]\n",
            base_job(),
            ["t"; MAX_TAGS + 1].join(", ")
        ))),
        ["too_many_tags"]
    );
    assert_eq!(
        reasons(&valid().replace("DEPLOY_KEY", "deploy-key")),
        ["secret_name_invalid"]
    );
    let many: Vec<String> = (0..=MAX_SECRETS).map(|i| format!("S{i}")).collect();
    assert_eq!(
        reasons(&valid().replace("[DEPLOY_KEY]", &format!("[{}]", many.join(", ")))),
        ["too_many_secrets"]
    );
    for bad in ["../etc", "/abs", "a/./b", "C:/x", "a\\\\b", "a//b"] {
        let text = valid().replace("target/release/app", bad);
        assert_eq!(reasons(&text), ["artifact_path_invalid"], "{bad}");
    }
    let paths = vec!["p"; MAX_ARTIFACT_PATHS + 1].join(", ");
    assert_eq!(
        reasons(&valid().replace("[target/release/app]", &format!("[{paths}]"))),
        ["too_many_artifacts"]
    );
    for bad in ["\"*/x\"", "-x", "a..b", "\"a b\"", "\"r*x*\""] {
        let text = valid().replace("branches: [main]", &format!("branches: [{bad}]"));
        assert_eq!(reasons(&text), ["branch_pattern_invalid"], "{bad}");
    }
    let branches = vec!["main"; MAX_BRANCHES + 1].join(", ");
    assert_eq!(
        reasons(&valid().replace("branches: [main]", &format!("branches: [{branches}]"))),
        ["too_many_branches"]
    );
    assert_eq!(
        reasons(&valid().replace("merge_requests: false", "merge_requests: no")),
        ["field_invalid"]
    );
    assert_eq!(
        reasons(&valid().replace("tags: [linux]", "tags: [linux, linux]")),
        ["field_invalid"]
    );
    assert_eq!(
        reasons(&valid().replace("tags: [linux]", "tags: linux")),
        ["field_invalid"]
    );
}

#[test]
fn several_independent_problems_are_all_reported_in_line_order() {
    let text = valid()
        .replace("timeout: 30m", "timeout: 9h")
        .replace("needs: [unit]", "needs: [nope]");
    // The first is a schema problem: the graph is checked only on a
    // well-formed file, so this reports the timeout alone…
    assert_eq!(reasons(&text), ["timeout_too_long"]);
    // …and two schema problems in two jobs are both reported, by line.
    let text = valid()
        .replace("timeout: 30m", "timeout: 9h")
        .replace("timeout: 1h", "timeout: 0h");
    let found = problems(&text);
    assert_eq!(
        found.iter().map(|p| p.reason).collect::<Vec<_>>(),
        ["timeout_too_long", "timeout_invalid"]
    );
    assert!(found[0].line < found[1].line);
}

#[test]
fn a_problem_displays_line_reason_and_subject() {
    assert_eq!(
        Problem::about(7, "needs_unknown", "x").to_string(),
        "7 needs_unknown — x"
    );
    assert_eq!(Problem::new(0, "too_large").to_string(), "0 too_large");
}

// ── plan ───────────────────────────────────────────────────────────────

fn plan_for(text: &str, event: Event, branch: &str) -> Result<Plan, Vec<Problem>> {
    let def = check(text.as_bytes(), &LintContext::default()).expect("valid");
    plan(
        &def,
        &Trigger {
            event,
            branch: branch.to_owned(),
        },
    )
}

fn order(p: &Plan) -> Vec<(&str, usize)> {
    p.jobs.iter().map(|j| (j.name.as_str(), j.wave)).collect()
}

#[test]
fn a_push_to_main_plans_every_job_with_stage_barriers_and_needs() {
    let p = plan_for(&valid(), Event::Push, "main").expect("plan");
    assert_eq!(
        order(&p),
        [("build", 0), ("unit", 1), ("integration", 2), ("deploy", 3)]
    );
    let integration = &p.jobs[2];
    assert_eq!(integration.after, ["build", "unit"]);
    assert_eq!(p.jobs[3].after, ["integration", "unit"]);
    assert_eq!(p.jobs[3].secrets, ["DEPLOY_KEY"]);
    assert_eq!(p.jobs[0].script_lines, 1);
}

#[test]
fn a_merge_request_plan_leaves_out_jobs_without_merge_requests() {
    let p = plan_for(&valid(), Event::MergeRequest, "feature-x").expect("plan");
    assert_eq!(order(&p), [("build", 0), ("unit", 1), ("integration", 2)]);
    let p = plan_for(&valid(), Event::Push, "feature-x").expect("plan");
    assert!(p.jobs.iter().all(|j| j.name != "deploy"));
}

#[test]
fn jobs_without_needs_in_one_stage_share_a_wave() {
    let text = valid().replace("    needs: [unit]\n", "");
    let p = plan_for(&text, Event::Push, "main").expect("plan");
    assert_eq!(
        order(&p),
        [("build", 0), ("integration", 1), ("unit", 1), ("deploy", 2)]
    );
}

#[test]
fn a_plan_is_refused_when_a_job_needs_one_the_trigger_does_not_run() {
    let text = valid().replace(
        "  unit:\n    stage: test\n",
        "  unit:\n    stage: test\n    rules:\n      branches: [main]\n",
    );
    let err = plan_for(&text, Event::Push, "dev").expect_err("needs a skipped job");
    assert_eq!(err[0].reason, "needs_not_in_pipeline");
    assert_eq!(err[0].subject.as_deref(), Some("unit"));
    assert_eq!(invalid(&err).code(), ErrorCode::CiConfigInvalid);
}

#[test]
fn a_plan_is_the_same_for_the_same_input() {
    let a = plan_for(&valid(), Event::Push, "main").expect("plan");
    let b = plan_for(&valid(), Event::Push, "main").expect("plan");
    assert_eq!(a, b);
}

#[test]
fn events_parse_and_print() {
    assert_eq!(Event::parse("push"), Some(Event::Push));
    assert_eq!(Event::parse("merge_request"), Some(Event::MergeRequest));
    assert_eq!(Event::parse("tag"), None);
    assert_eq!(Event::MergeRequest.to_string(), "merge_request");
}
