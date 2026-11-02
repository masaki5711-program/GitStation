#![allow(clippy::expect_used, clippy::unwrap_used)] // tests may fail loudly

use super::*;

fn owners_of(c: &Codeowners, path: &str) -> Option<Vec<String>> {
    c.rule_for(path)
        .map(|r| r.owners.iter().map(Owner::display).collect())
}

fn err(text: &str) -> CodeownersError {
    parse(text.as_bytes()).expect_err("invalid")
}

/// Every grammar form of ADR-0024 §9 / plan 11 Q5.
#[test]
fn each_grammar_form_matches_as_specified() {
    let c = parse(
        b"# a comment\n\
          \n\
          *            @all\n\
          *.rs         @rust   # trailing comment\n\
          /README.md   @docs\n\
          docs/        @writers @org/writers\n\
          /build/      @release\n\
          src/**/gen/  @gen\n\
          ?.txt        @short\n\
          my\\ file    @spaces\n\
          /tag\\#1     @hash\n",
    )
    .expect("valid");
    assert_eq!(c.rules.len(), 9);
    // `*` everywhere, unless a later rule matches.
    assert_eq!(owners_of(&c, "Makefile"), Some(vec!["@all".to_owned()]));
    // No leading slash: any depth.
    assert_eq!(
        owners_of(&c, "src/deep/a.rs"),
        Some(vec!["@rust".to_owned()])
    );
    assert_eq!(owners_of(&c, "a.rs"), Some(vec!["@rust".to_owned()]));
    // A leading slash: the root only.
    assert_eq!(owners_of(&c, "README.md"), Some(vec!["@docs".to_owned()]));
    assert_eq!(
        owners_of(&c, "sub/README.md"),
        Some(vec!["@all".to_owned()])
    );
    // A trailing slash: everything below, at any depth; groups by path.
    assert_eq!(
        owners_of(&c, "docs/guide/intro.md"),
        Some(vec!["@writers".to_owned(), "@org/writers".to_owned()])
    );
    assert_eq!(
        owners_of(&c, "pkg/docs/x.md"),
        Some(vec!["@writers".to_owned(), "@org/writers".to_owned()])
    );
    // Anchored directory.
    assert_eq!(
        owners_of(&c, "build/out.bin"),
        Some(vec!["@release".to_owned()])
    );
    assert_eq!(
        owners_of(&c, "x/build/out.bin"),
        Some(vec!["@all".to_owned()])
    );
    // `**` as a whole segment: any number of directories.
    assert_eq!(owners_of(&c, "src/gen/a.c"), Some(vec!["@gen".to_owned()]));
    assert_eq!(
        owners_of(&c, "src/a/b/gen/a.c"),
        Some(vec!["@gen".to_owned()])
    );
    // `?`: exactly one character.
    assert_eq!(owners_of(&c, "a.txt"), Some(vec!["@short".to_owned()]));
    assert_eq!(owners_of(&c, "ab.txt"), Some(vec!["@all".to_owned()]));
    // Escapes.
    assert_eq!(owners_of(&c, "my file"), Some(vec!["@spaces".to_owned()]));
    assert_eq!(owners_of(&c, "tag#1"), Some(vec!["@hash".to_owned()]));
    assert_eq!(c.rules[3].owners[1], Owner::Group("org/writers".to_owned()));
    assert_eq!(c.rules[0].owners[0], Owner::Name("all".to_owned()));
}

/// The last matching line wins, not the most specific one.
#[test]
fn the_last_matching_rule_wins() {
    let c = parse(b"src/lib.rs @first\n*.rs @second\n").expect("valid");
    assert_eq!(
        owners_of(&c, "src/lib.rs"),
        Some(vec!["@second".to_owned()])
    );
    let c = parse(b"*.rs @second\nsrc/lib.rs @first\n").expect("valid");
    assert_eq!(owners_of(&c, "src/lib.rs"), Some(vec!["@first".to_owned()]));
    assert_eq!(owners_of(&c, "README"), None, "no rule: no owner");
}

#[test]
fn an_unanchored_name_matches_a_file_or_a_directory_at_any_depth() {
    let c = parse(b"config @ops\n/root-only @root\n/ @everyone\n").expect("valid");
    assert_eq!(owners_of(&c, "config"), Some(vec!["@everyone".to_owned()]));
    let c = parse(b"/ @everyone\nconfig @ops\n").expect("valid");
    assert_eq!(owners_of(&c, "a/config"), Some(vec!["@ops".to_owned()]));
    assert_eq!(
        owners_of(&c, "a/config/x.yml"),
        Some(vec!["@ops".to_owned()])
    );
    assert_eq!(
        owners_of(&c, "a/configs"),
        Some(vec!["@everyone".to_owned()])
    );
}

/// Malformed files fail closed with their line number and reason.
#[test]
fn malformed_files_report_the_line_and_the_reason() {
    assert_eq!(
        err("* @a\nsrc/\n"),
        CodeownersError {
            line: 2,
            reason: "syntax"
        }
    );
    assert_eq!(
        err("* @a\n\n[Section]\n* @b\n"),
        CodeownersError {
            line: 3,
            reason: "unsupported"
        }
    );
    assert_eq!(
        err("^[Optional]\n"),
        CodeownersError {
            line: 1,
            reason: "unsupported"
        }
    );
    assert_eq!(
        err("* user@example.test\n"),
        CodeownersError {
            line: 1,
            reason: "unsupported"
        }
    );
    assert_eq!(
        err("!*.md @a\n"),
        CodeownersError {
            line: 1,
            reason: "unsupported"
        }
    );
    assert_eq!(
        err("* owner\n"),
        CodeownersError {
            line: 1,
            reason: "owner_syntax"
        }
    );
    assert_eq!(
        err("* @bad!name\n"),
        CodeownersError {
            line: 1,
            reason: "owner_syntax"
        }
    );
    assert_eq!(
        err("* @a//b\n"),
        CodeownersError {
            line: 1,
            reason: "owner_syntax"
        }
    );
    assert_eq!(
        err("*.[ch] @c\n"),
        CodeownersError {
            line: 1,
            reason: "pattern_invalid"
        }
    );
    assert_eq!(
        err("a/***/b @c\n"),
        CodeownersError {
            line: 1,
            reason: "pattern_invalid"
        }
    );
    assert_eq!(
        err("a**b @c\n"),
        CodeownersError {
            line: 1,
            reason: "pattern_invalid"
        }
    );
    assert_eq!(
        err("a/../b @c\n"),
        CodeownersError {
            line: 1,
            reason: "pattern_invalid"
        }
    );
    assert_eq!(
        err("a\\tb @c\n"),
        CodeownersError {
            line: 1,
            reason: "pattern_invalid"
        }
    );
}

/// The limits decided in R5 (ADR-0024 未決): 64 KiB, 1 000 lines, 4 KiB a
/// line, 50 owners a rule; not UTF-8.
#[test]
fn the_size_and_line_limits_fail_closed() {
    let big = vec![b'#'; MAX_BYTES + 1];
    assert_eq!(
        parse(&big).expect_err("big"),
        CodeownersError {
            line: 0,
            reason: "too_large"
        }
    );
    let many = "# c\n".repeat(MAX_LINES + 1);
    assert_eq!(
        err(&many),
        CodeownersError {
            line: u32::try_from(MAX_LINES + 1).expect("small"),
            reason: "too_many_lines"
        }
    );
    let long = format!("{} @a\n", "a".repeat(MAX_LINE_BYTES));
    assert_eq!(
        err(&long),
        CodeownersError {
            line: 1,
            reason: "line_too_long"
        }
    );
    let owners: Vec<String> = (0..=MAX_OWNERS_PER_RULE)
        .map(|n| format!("@u{n}"))
        .collect();
    assert_eq!(
        err(&format!("* {}\n", owners.join(" "))),
        CodeownersError {
            line: 1,
            reason: "too_many_owners"
        }
    );
    assert_eq!(
        parse(b"* @a\n\xff\n").expect_err("latin-1"),
        CodeownersError {
            line: 0,
            reason: "not_utf8"
        }
    );
}

#[test]
fn the_owners_are_listed_once() {
    let c = parse(b"* @a @g/x\n*.rs @a\n").expect("valid");
    assert_eq!(
        c.owners(),
        vec![Owner::Name("a".to_owned()), Owner::Group("g/x".to_owned())]
    );
}

/// R5 review (HIGH): a hostile pattern against a deep path finishes at
/// once (no exponential backtracking); too many wildcards are refused.
#[test]
fn an_adversarial_pattern_matches_in_bounded_time() {
    let pattern = format!("{}b @a\n", "a*".repeat(8));
    let c = parse(pattern.as_bytes()).expect("valid");
    let deep = vec!["a".repeat(200); 60].join("/");
    let started = std::time::Instant::now();
    assert_eq!(owners_of(&c, &deep), None);
    let many = parse(b"**/**/**/**/**/**/**/**/x @a\n").expect("collapsed");
    assert_eq!(owners_of(&many, &deep), None);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    let wild = format!("{} @a\n", "?".repeat(MAX_WILDCARDS + 1));
    assert_eq!(
        err(&wild),
        CodeownersError {
            line: 1,
            reason: "pattern_invalid"
        }
    );
}

/// R5 review (HIGH): a BOM is dropped; invisible format characters and
/// non-ASCII blanks outside a comment are refused with their line.
#[test]
fn invisible_characters_fail_closed_and_a_bom_is_dropped() {
    let c = parse("\u{FEFF}*.rs @a\n".as_bytes()).expect("bom");
    assert_eq!(owners_of(&c, "x.rs"), Some(vec!["@a".to_owned()]));
    assert_eq!(
        err("*.rs\u{200B} @a\n"),
        CodeownersError {
            line: 1,
            reason: "pattern_invalid"
        }
    );
    assert_eq!(
        err("# ok\n*.rs @a\u{202E}\n"),
        CodeownersError {
            line: 2,
            reason: "pattern_invalid"
        }
    );
    assert_eq!(
        err("*.rs\u{3000}@a\n"),
        CodeownersError {
            line: 1,
            reason: "pattern_invalid"
        }
    );
    // In a comment they are only text.
    parse("*.rs @a # 全角\u{3000}スペース\n".as_bytes()).expect("comment");
}

/// R5 review (MEDIUM): matching ignores ASCII case.
#[test]
fn matching_ignores_ascii_case() {
    let c = parse(b"/src/ @a\n*.RS @b\n").expect("valid");
    assert_eq!(owners_of(&c, "SRC/auth.rs"), Some(vec!["@b".to_owned()]));
    let c = parse(b"/src/ @a\n").expect("valid");
    assert_eq!(owners_of(&c, "SRC/auth.txt"), Some(vec!["@a".to_owned()]));
}

/// R5 review round 2 (MEDIUM): control characters (VT, DEL), combining
/// marks, Hangul fillers, the Braille blank and variation selectors are
/// refused outside a comment; a tab still separates.
#[test]
fn other_invisible_characters_are_refused() {
    for bad in [
        "*.rs\u{000B} @a\n",
        "*.rs\u{007F} @a\n",
        "*.rs\u{0301} @a\n",
        "*.rs\u{20DD} @a\n",
        "*.rs\u{3164} @a\n",
        "*.rs\u{115F} @a\n",
        "*.rs\u{1160} @a\n",
        "*.rs\u{FFA0} @a\n",
        "*.rs\u{2800} @a\n",
        "*.rs\u{FE0F} @a\n",
        "*.rs @a\u{E0101}\n",
    ] {
        assert_eq!(
            err(bad),
            CodeownersError {
                line: 1,
                reason: "pattern_invalid"
            },
            "{bad:?}"
        );
    }
    let c = parse(b"*.rs\t@a\n").expect("a tab separates");
    assert_eq!(owners_of(&c, "x.rs"), Some(vec!["@a".to_owned()]));
}

/// R5 review round 2 (LOW): a doubled slash at either end is refused; a
/// pattern with a slash before its end is anchored at the root (gitignore,
/// ADR-0024 §9: only a pattern without `/` matches at any depth).
#[test]
fn patterns_with_a_slash_are_anchored_and_doubled_slashes_refused() {
    assert_eq!(
        err("//src @a\n"),
        CodeownersError {
            line: 1,
            reason: "pattern_invalid"
        }
    );
    assert_eq!(
        err("src// @a\n"),
        CodeownersError {
            line: 1,
            reason: "pattern_invalid"
        }
    );
    let c = parse(b"src/*.rs @a\ndocs/ @b\n").expect("valid");
    assert_eq!(owners_of(&c, "src/x.rs"), Some(vec!["@a".to_owned()]));
    assert_eq!(
        owners_of(&c, "lib/src/x.rs"),
        None,
        "anchored: not at depth"
    );
    assert_eq!(
        owners_of(&c, "pkg/docs/a.md"),
        Some(vec!["@b".to_owned()]),
        "trailing slash only"
    );
}

/// R5 review round 2 (HIGH): matching stops at its deadline inside the
/// rule loop too, not only between paths.
#[test]
fn matching_stops_at_the_deadline_inside_the_rules() {
    let text: String = (0..1000).map(|n| format!("/d{n}/**/x*y*z @a\n")).collect();
    let c = parse(text.as_bytes()).expect("valid");
    let past = std::time::Instant::now();
    assert_eq!(c.owned_lines(&["a/b".to_owned()], Some(past)), None);
}
