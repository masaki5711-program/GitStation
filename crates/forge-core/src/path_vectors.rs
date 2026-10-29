//! Shared accept / reject vectors for namespace path segments (ADR-0015),
//! used by the parser tests (forge-core) and the database CHECK test
//! (forge-db, feature `test-support`). Reserved top-level words are in
//! neither list: they are legal segments, refused only at the top level.

/// Every one of these is a valid segment.
pub fn accepted() -> Vec<String> {
    let mut v: Vec<String> = [
        "a",
        "0",
        "7z",
        "acme",
        "forge-web",
        "john.doe",
        "a_b",
        "a.b.c",
        "my--name",
        "x1",
        "git",
        "wiki",
        "atom",
        "foo.gitx",
        "a.wikis",
        "com0",
        "com10",
        "console",
        "aux-x",
        "lpt",
        "nullable",
        "1.0",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    v.push("a".repeat(64));
    v.push(format!("a{}b", "-".repeat(62)));
    v
}

/// Every one of these is refused.
pub fn rejected() -> Vec<String> {
    let mut v: Vec<String> = [
        "",
        "A",
        "Acme",
        "ACME",
        "-",
        ".",
        "..",
        "-a",
        "a-",
        ".a",
        "a.",
        "_a",
        "a_",
        "a b",
        " a",
        "a ",
        "a/b",
        "/a",
        "a\\b",
        "a%2fb",
        "a%41",
        "a:b",
        "a@b",
        "a..b",
        "a...b",
        "x.git",
        "x.atom",
        "x.wiki",
        "a.b.git",
        "con",
        "prn",
        "aux",
        "nul",
        "com1",
        "com9",
        "lpt1",
        "lpt9",
        "con.txt",
        "nul.tar.gz",
        "com1.x.y",
        "aux.git",
        "a\u{0}b",
        "a\nb",
        "a\tb",
        "\u{7f}",
        "日本",
        "aé",
        "ａ",
        "a\u{212a}",
        "İ",
        "a\n",
        "\na",
        "lpt9.x",
        "com1.git",
        "\u{0661}",
        "a\u{0663}",
        "\u{ff12}",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    v.push("a".repeat(65));
    v.push("a".repeat(200));
    v
}
