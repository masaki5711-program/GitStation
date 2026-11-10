#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)] // tests may fail loudly

use super::*;

const JOB_TOKEN: &str =
    "fgj_0123456789abcdef_0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const PAT: &str =
    "fgp_fedcba9876543210_ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

fn log(tail: &str) -> FailureLog {
    FailureLog {
        job_name: "test".to_owned(),
        job_error_code: None,
        tail: tail.to_owned(),
        first_line: 1,
        truncated: false,
    }
}

#[test]
fn masking_hides_forge_tokens_auth_headers_url_passwords_and_keys() {
    let text = format!(
        "FORGE_JOB_TOKEN={JOB_TOKEN}\n\
         curl -H 'Authorization: Bearer abc.def' https://forge.internal\n\
         git clone https://ci:s3cret@git.internal/web.git\n\
         token {PAT} done\n\
         -----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\nBBBB\n-----END OPENSSH PRIVATE KEY-----\n\
         session gss_ABCDEFGHIJKLMNOPQRSTUV ok\n"
    );
    let masked = mask("", &text, "");
    for secret in [
        JOB_TOKEN,
        PAT,
        "abc.def",
        "s3cret",
        "AAAA",
        "BBBB",
        "gss_ABCDEFGHIJKLMNOPQRSTUV",
    ] {
        assert!(!masked.contains(secret), "{secret} in {masked}");
    }
    assert!(masked.contains("FORGE_JOB_TOKEN=[MASKED]"));
    assert!(masked.contains("token [MASKED] done"));
    assert!(masked.contains("git.internal/web.git"));
    assert!(masked.contains("session [MASKED] ok"));
    // Ordinary output is left as it is.
    let plain = "test result: ok. 12 passed; 0 failed\nfgp_ is a prefix\n";
    assert_eq!(mask("", plain, ""), plain);
}

/// A secret cut by a page's edge is still masked on both pages: the
/// context finds it, only the page's own part is returned.
#[test]
fn a_secret_across_the_page_edge_is_masked_on_both_sides() {
    let all = format!("env: FORGE_JOB_TOKEN={JOB_TOKEN} end");
    let cut = all.find("_0123456789abcdef_").expect("token") + 10;
    let (page1, page2) = all.split_at(cut);
    let first = mask("", page1, page2);
    let second = mask(page1, page2, "");
    assert_eq!(first, "env: FORGE_JOB_TOKEN=[MASKED]");
    assert_eq!(second, "[MASKED] end");
    for part in [&first, &second] {
        assert!(!part.contains("0123456789abcdef0123"), "{part}");
    }
    // A key block whose END is beyond the page is masked to the page's end.
    let open = "log\n-----BEGIN RSA PRIVATE KEY-----\nMIIE\n";
    assert_eq!(mask("", open, ""), "log\n[MASKED]");
}

#[test]
fn normalizing_removes_what_varies_between_runs() {
    let a = normalize(
        "2026-11-10T10:20:00Z test reload_within_10s ... FAILED (timeout after 10s) \
         at /tmp/cargo-x1y2/target pid 4123 commit 1a2b3c4d5e FG-DB-0003",
    );
    let b = normalize(
        "2026-11-11T08:01:59Z test reload_within_10s ... FAILED (timeout after 12s) \
         at /tmp/cargo-q9/target pid 77 commit 9f8e7d6c5b FG-DB-0003",
    );
    assert_eq!(a, b);
    assert!(a.contains("FG-DB-0003"), "{a}");
    assert!(a.contains("FAILED"), "{a}");
    assert_ne!(
        normalize("test a FAILED"),
        normalize("test b FAILED"),
        "the test's name is kept"
    );
    assert_eq!(normalize("\x1b[31merror\x1b[0m:  x"), "error: x");
}

#[test]
fn the_same_failure_gets_the_same_fingerprint_another_failure_another() {
    let first = digest(&log(
        "Compiling forge v0.1.0\n   Finished in 12.3s\nrunning 4 tests\n\
         test db::reload ... FAILED\n\
         thread 'db::reload' panicked at src/db.rs:41:9: pool timed out FG-DB-0003\n\
         test result: FAILED. 3 passed; 1 failed\n",
    ));
    let again = digest(&log(
        "Compiling forge v0.1.0\n   Finished in 9.8s\nrunning 4 tests\n\
         test db::reload ... FAILED\n\
         thread 'db::reload' panicked at src/db.rs:41:9: pool timed out FG-DB-0003\n\
         test result: FAILED. 3 passed; 1 failed\n",
    ));
    let other = digest(&log("running 4 tests\ntest web::login ... FAILED\n\
         thread 'web::login' panicked at src/web.rs:7:1: assertion failed\n"));
    assert_eq!(first.fingerprint, again.fingerprint);
    assert_ne!(first.fingerprint, other.fingerprint);
    assert!(first.fingerprint.starts_with("fp_") && first.fingerprint.len() == 19);
    assert_eq!(first.error_codes, ["FG-DB-0003"]);
    assert_eq!(first.lines, Some((4, 6)));
    assert!(first.excerpt.starts_with("test db::reload ... FAILED"));
    assert!(!first.excerpt.contains("Compiling"));
}

#[test]
fn a_digest_never_keeps_a_secret_and_stays_within_its_bounds() {
    let mut tail =
        format!("export FORGE_JOB_TOKEN={JOB_TOKEN}\nerror: FORGE_JOB_TOKEN={JOB_TOKEN}\n");
    for i in 0..400 {
        tail.push_str(&format!("error[E0{i:03}]: {}\n", "x".repeat(60)));
    }
    let d = digest(&log(&tail));
    assert!(!d.excerpt.contains(JOB_TOKEN));
    assert!(d.excerpt.len() <= MAX_EXCERPT_BYTES);
    assert!(d.excerpt.lines().count() <= KEY_LINES);
    let many = (0..30)
        .map(|i| format!("FG-CI-{i:04}"))
        .collect::<Vec<_>>()
        .join(" failed\n");
    assert_eq!(digest(&log(&many)).error_codes.len(), MAX_ERROR_CODES);
}

#[test]
fn no_failure_line_keeps_the_last_lines_and_no_log_keys_on_the_job_code() {
    let quiet = digest(&log("step 1\nstep 2\n\nstep 3\n"));
    assert_eq!(quiet.excerpt, "step 1\nstep 2\nstep 3");
    assert_eq!(quiet.lines, Some((1, 4)));
    let mut lost = log("");
    lost.job_error_code = Some("FG-CI-0004".to_owned());
    let a = digest(&lost);
    assert_eq!(a.error_codes, ["FG-CI-0004"]);
    assert_eq!(a.excerpt, "");
    assert_eq!(a.lines, None);
    lost.job_error_code = Some("FG-CI-0003".to_owned());
    assert_ne!(digest(&lost).fingerprint, a.fingerprint);
    // Line numbers follow the tail's place in the log.
    let mut later = log("ok\nerror: boom\n");
    later.first_line = 1000;
    assert_eq!(digest(&later).lines, Some((1001, 1001)));
}
