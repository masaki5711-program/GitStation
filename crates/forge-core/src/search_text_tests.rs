use proptest::prelude::*;

use super::*;

fn query_terms(raw: &str) -> Vec<String> {
    SearchQuery::parse(raw)
        .unwrap()
        .terms()
        .iter()
        .map(|t| t.text.clone())
        .collect()
}

/// Does `query` match a text, judged as PostgreSQL would on the vector
/// (every term present; a prefix term matches a stored term it starts).
fn matches(text: &str, query: &str) -> bool {
    let stored = index_terms(text);
    SearchQuery::parse(query).unwrap().terms().iter().all(|t| {
        stored.iter().any(|s| {
            if t.prefix {
                s.starts_with(&t.text)
            } else {
                *s == t.text
            }
        })
    })
}

#[test]
fn normalize_is_nfkc_and_lower_case() {
    assert_eq!(normalize("ＡＢＣ１２３"), "abc123");
    assert_eq!(normalize("ｶﾞｲﾄﾞ"), "ガイド");
    assert_eq!(normalize("Ⅻ"), "xii");
}

#[test]
fn japanese_runs_become_two_grams_with_the_last_character() {
    assert_eq!(index_terms("設備仕様"), vec!["設備", "備仕", "仕様", "様"]);
    assert_eq!(index_terms("設"), vec!["設"]);
}

#[test]
fn two_char_japanese_word_found() {
    assert!(matches("この設備の承認の手順", "設備"));
    assert!(matches("この設備の承認の手順", "承認"));
    assert!(!matches("この設備の手順", "承認"));
    assert_eq!(query_terms("承認"), vec!["承認"]);
}

#[test]
fn fullwidth_and_halfwidth_match() {
    assert!(matches("ＰＬＣ ﾃﾞｰﾀ 収集", "plc データ"));
    assert!(matches("PLC データ 収集", "ＰＬＣ　ﾃﾞｰﾀ"));
}

#[test]
fn indonesian_words_split_on_spaces() {
    assert_eq!(
        index_terms("Pemeliharaan mesin produksi"),
        vec!["pemeliharaan", "mesin", "produksi"]
    );
    assert!(matches("Pemeliharaan mesin produksi", "mesin"));
    // The last word is a prefix: typing is searching.
    assert!(matches("Pemeliharaan mesin produksi", "pemelih"));
    assert!(!matches("Pemeliharaan mesin produksi", "mes produksi"));
}

#[test]
fn mixed_scripts_split_between_runs() {
    assert_eq!(index_terms("PLC制御v2"), vec!["plc", "制御", "御", "v2"]);
    assert!(matches("PLC制御v2の説明", "制御"));
    assert!(matches("REQ-0012 の要件", "req-0012"));
}

#[test]
fn one_character_cjk_piece_is_a_prefix() {
    let q = SearchQuery::parse("A 設").unwrap();
    assert_eq!(
        q.terms(),
        &[
            QueryTerm {
                text: "a".into(),
                prefix: true
            },
            QueryTerm {
                text: "設".into(),
                prefix: true
            },
        ]
    );
    // The last character of a run is stored alone, so it is found too.
    assert!(matches("a 装置設", "a 設"));
    assert!(matches("a 設備", "a 設"));
}

#[test]
fn query_rejections() {
    assert_eq!(SearchQuery::parse(""), Err(QueryRejection::Empty));
    assert_eq!(SearchQuery::parse(" ・、!"), Err(QueryRejection::Empty));
    assert_eq!(SearchQuery::parse("a"), Err(QueryRejection::TooShort));
    assert_eq!(SearchQuery::parse("設"), Err(QueryRejection::TooShort));
    assert!(SearchQuery::parse("ab").is_ok());
    let long = "a".repeat(MAX_QUERY_CHARS + 1);
    assert_eq!(SearchQuery::parse(&long), Err(QueryRejection::TooLong));
    assert!(SearchQuery::parse(&"a".repeat(MAX_QUERY_CHARS)).is_ok());
    assert_eq!(QueryRejection::TooShort.as_str(), "too_short");
}

#[test]
fn tsquery_literal_quotes_every_term() {
    let q = SearchQuery::parse("設備 maint").unwrap();
    assert_eq!(q.tsquery_literal(), "'設備' & 'maint':*");
    let mut out = String::new();
    quote("a'b\\c", &mut out);
    assert_eq!(out, "'a''b\\\\c'");
}

#[test]
fn duplicate_query_terms_are_dropped() {
    assert_eq!(query_terms("設備 設備"), vec!["設備"]);
}

#[test]
fn vector_literal_has_weights_and_sorted_lexemes() {
    let v = IndexText::new()
        .with(Weight::A, "Pump")
        .with(Weight::B, "plant/pump")
        .with(Weight::C, "設備")
        .build();
    assert_eq!(v.literal, "'plant':2B 'pump':1A,3B '備':5C '設備':4C");
    assert_eq!(v.terms, 4);
    assert!(!v.truncated);
}

#[test]
fn too_many_terms_mark_the_vector_truncated() {
    let text: String = (0..(MAX_INDEX_TERMS + 10))
        .map(|i| format!("w{i} "))
        .collect();
    let v = IndexText::new().with(Weight::C, &text).build();
    assert!(v.truncated);
    assert_eq!(v.terms, MAX_INDEX_TERMS);
}

#[test]
fn long_words_are_cut_the_same_way_on_both_sides() {
    let word = "x".repeat(MAX_WORD_CHARS + 20);
    assert_eq!(index_terms(&word), vec!["x".repeat(MAX_WORD_CHARS)]);
    assert!(matches(&word, &word[..MAX_QUERY_CHARS.min(word.len())]));
}

#[test]
fn snippet_highlights_in_original_characters() {
    let q = SearchQuery::parse("ﾃﾞｰﾀ").unwrap();
    let s = snippet("PLC のデータを集める", &q, 100);
    assert_eq!(s.text, "PLC のデータを集める");
    let chars: Vec<char> = s.text.chars().collect();
    assert_eq!(s.highlights, vec![(5, 8)]);
    assert_eq!(chars[5..8].iter().collect::<String>(), "データ");
    assert!(!s.cut_before && !s.cut_after);
}

#[test]
fn snippet_windows_around_the_first_match() {
    let text = format!("{}設備の承認{}", "あ".repeat(300), "い".repeat(300));
    let q = SearchQuery::parse("承認").unwrap();
    let s = snippet(&text, &q, 40);
    assert_eq!(s.text.chars().count(), 40);
    assert!(s.cut_before && s.cut_after);
    let (a, b) = s.highlights[0];
    let chars: Vec<char> = s.text.chars().collect();
    assert_eq!(chars[a..b].iter().collect::<String>(), "承認");
}

#[test]
fn snippet_without_a_match_is_the_start() {
    let q = SearchQuery::parse("zz").unwrap();
    let s = snippet("abc", &q, 2);
    assert_eq!(s.text, "ab");
    assert!(s.highlights.is_empty());
    assert!(s.cut_after && !s.cut_before);
}

#[test]
fn fingerprint_is_stable_and_hides_the_words() {
    let a = SearchQuery::parse("設備 Pump").unwrap();
    let b = SearchQuery::parse("設備　ｐｕｍｐ").unwrap();
    assert_eq!(a.fingerprint(), b.fingerprint());
    assert_eq!(a.fingerprint().len(), 32);
    assert!(!a.fingerprint().contains("pump"));
}

proptest! {
    #[test]
    fn tokenize_is_deterministic(text in "\\PC{0,80}") {
        prop_assert_eq!(index_terms(&text), index_terms(&text));
        let a = IndexText::new().with(Weight::C, &text).build();
        let b = IndexText::new().with(Weight::C, &text).build();
        prop_assert_eq!(a, b);
    }

    /// A Japanese query that is a substring of a text finds it: its terms
    /// are a subset of the text's.
    #[test]
    fn query_tokens_subset_of_text_tokens_when_substring(
        text in "[あ-んア-ン一-龥]{2,40}",
        start in 0usize..40,
        len in 2usize..10,
    ) {
        let chars: Vec<char> = text.chars().collect();
        let start = start.min(chars.len() - 2);
        let end = (start + len).min(chars.len());
        let query: String = chars[start..end].iter().collect();
        prop_assert!(matches(&text, &query), "{query} in {text}");
    }

    /// Whole words of a Latin text are found, in any case.
    #[test]
    fn latin_words_are_found(words in prop::collection::vec("[a-zA-Z0-9]{2,12}", 1..8), pick in 0usize..8) {
        let text = words.join(" ");
        let word = &words[pick % words.len()];
        prop_assert!(matches(&text, &word.to_uppercase()));
    }
}
