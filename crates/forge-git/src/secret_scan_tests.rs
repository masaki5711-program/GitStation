// Every sample secret is assembled at run time (`join`), so this file itself
// never trips the push checks of the repository that hosts it.

use super::*;

fn j(parts: &[&str]) -> Vec<u8> {
    parts.concat().into_bytes()
}

/// 36 / 43 / 64 random-looking characters (synthetic).
const R36: &str = "q8Zr2LmX7vB4nT1kW9pY3sD6fH0jG5cA2eRu";
const R43: &str = "Zx3Qv9Lm2Tb7Nc4Wk8Rp1Ys6Dh0Jf5Ga3Eu7Io2Pq9X";
const HEX64: &str = "3fa91c07d2b84e6f5a0c19e7b3d4f28a6c5e0b1d97f3a2c4e8b6d0f1a3c5e7b9";

#[test]
fn private_key_headers_are_found() {
    for label in ["RSA ", "", "EC ", "OPENSSH ", "ENCRYPTED ", "DSA "] {
        let content = j(&["x\n-----BEGIN ", label, "PRIVATE", " KEY-----\nMIIE\n"]);
        assert_eq!(scan(&content), SecretScan::Found("private_key"), "{label}");
    }
    let pgp = j(&["-----BEGIN PGP PRIVATE", " KEY BLOCK-----\n"]);
    assert_eq!(scan(&pgp), SecretScan::Found("private_key"));
}

#[test]
fn public_keys_and_certificates_are_clean() {
    for label in ["PUBLIC KEY", "CERTIFICATE", "RSA PUBLIC KEY"] {
        let content = j(&["-----BEGIN ", label, "-----\nMIIB\n"]);
        assert_eq!(scan(&content), SecretScan::Clean, "{label}");
    }
    // The words without the PEM frame.
    assert_eq!(scan(b"never commit a private key\n"), SecretScan::Clean);
}

#[test]
fn token_formats_are_found() {
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (
            j(&["key = AKIA", "Q7ZR2LMX7VB4NT1K\n"]),
            "aws_access_key_id",
        ),
        (j(&["token: \"gh", "p_", R36, "\"\n"]), "github_token"),
        (
            j(&["github_", "pat_", R36, R36, "_aB3dE5gH7\n"]),
            "github_token",
        ),
        (j(&["gl", "pat-", "Zx3Qv9Lm2Tb7Nc4Wk8Rp\n"]), "gitlab_token"),
        (j(&["xo", "xb-", "1234567890-Zx3Qv9Lm2T\n"]), "slack_token"),
        (j(&["AI", "za", &R43[..35], "\n"]), "google_api_key"),
        (
            j(&["fg", "p_", "0123456789abcdef", "_", HEX64, "\n"]),
            "gitstation_token",
        ),
        (
            j(&["gs", "s_", "0123456789abcdef", "_", HEX64]),
            "gitstation_token",
        ),
        (j(&["wh", "sec_", R43, "\n"]), "gitstation_webhook_secret"),
    ];
    for (content, rule) in cases {
        assert_eq!(
            scan(&content),
            SecretScan::Found(rule),
            "{}",
            String::from_utf8_lossy(&content)
        );
    }
}

#[test]
fn near_misses_are_clean() {
    let cases: Vec<Vec<u8>> = vec![
        // Too short, too long, inside a word.
        j(&["AKIA", "Q7ZR2LMX7VB4NT1\n"]),
        j(&["AKIA", "Q7ZR2LMX7VB4NT1KX\n"]),
        j(&["XAKIA", "Q7ZR2LMX7VB4NT1K\n"]),
        j(&["gh", "p_", &R36[..35], "\n"]),
        // AWS's documented example key.
        j(&["AKIA", "IOSFODNN7EXAMPLE\n"]),
        // Placeholders: the synthetic tokens of forge's own tests.
        j(&["fg", "p_", "00000000000000a1_", &"1".repeat(64), "\n"]),
        j(&["fg", "p_", "0123456789abcdef_", &HEX64[..63], "\n"]),
        j(&["fg", "p_", "0123456789ABCDEF_", HEX64, "\n"]),
        j(&["wh", "sec_", "short\n"]),
    ];
    for content in cases {
        assert_eq!(
            scan(&content),
            SecretScan::Clean,
            "{}",
            String::from_utf8_lossy(&content)
        );
    }
}

#[test]
fn the_allow_marker_anywhere_in_the_file_allows_it() {
    let content = j(&[
        "# synthetic fixture, forge:allow",
        "-secret\n-----BEGIN RSA PRIVATE",
        " KEY-----\n",
    ]);
    assert_eq!(scan(&content), SecretScan::Allowed("private_key"));
}

#[test]
fn binary_content_is_not_scanned() {
    let mut content = j(&["-----BEGIN RSA PRIVATE", " KEY-----\n"]);
    content.insert(0, 0);
    assert_eq!(scan(&content), SecretScan::Binary);
}

/// Review X7: the risk check scans binary content too (a NUL byte must not
/// hide a token); the push check keeps skipping it.
#[test]
fn scan_bytes_reads_content_the_push_check_calls_binary() {
    let mut content = b"\0key = AKIAQWERTYUIOPASDFGH\n".to_vec();
    assert_eq!(scan(&content), SecretScan::Binary);
    assert_eq!(scan_bytes(&content), SecretScan::Found("aws_access_key_id"));
    content.extend_from_slice(b"forge:allow-secret\n");
    assert_eq!(
        scan_bytes(&content),
        SecretScan::Allowed("aws_access_key_id")
    );
    assert_eq!(scan_bytes(b"\0\0plain\n"), SecretScan::Clean);
}
