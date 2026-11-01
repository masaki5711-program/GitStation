use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use forge_core::RepoId;
use forge_errors::ErrorCode;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::pkt_line::{decode_all, Packet, PktMode};
use crate::GitConfig;

struct Fixture {
    _home: tempfile::TempDir,
    _root_dir: tempfile::TempDir,
    git: Git,
    root: StorageRoot,
}

async fn fixture() -> Fixture {
    let home = tempfile::tempdir().expect("home");
    let root_dir = tempfile::tempdir().expect("root");
    let git = Git::new(GitConfig::new("git", home.path()));
    let root = StorageRoot::open(root_dir.path()).expect("root");
    Fixture {
        _home: home,
        _root_dir: root_dir,
        git,
        root,
    }
}

impl Fixture {
    async fn repo(&self, id: i64) -> (RepoId, PathBuf) {
        let id = RepoId::new(id);
        let path = self
            .root
            .init_bare(&self.git, id, uuid::Uuid::new_v4(), None)
            .await
            .expect("init");
        (id, path)
    }

    fn hooks(&self) -> String {
        self.git
            .config()
            .empty_hooks_dir()
            .to_string_lossy()
            .into_owned()
    }
}

async fn captured(git: &Git, cmd: GitCommand) -> String {
    let _ = git;
    cmd.output()
        .await
        .expect("git")
        .stdout_str()
        .trim()
        .to_string()
}

/// One commit with one file on `refs/heads/main` (synthetic content).
async fn commit(git: &Git, repo: &Path, content: &[u8]) -> String {
    let blob = captured(
        git,
        git.command("hash-object")
            .repo(repo)
            .arg("-w")
            .arg("--stdin")
            .stdin(content.to_vec()),
    )
    .await;
    let tree = captured(
        git,
        git.command("mktree")
            .repo(repo)
            .stdin(format!("100644 blob {blob}\tREADME\n").into_bytes()),
    )
    .await;
    let commit = captured(
        git,
        git.command("commit-tree")
            .config_override("user.name", "forge-test")
            .config_override("user.email", "forge-test@example.invalid")
            .repo(repo)
            .arg("-m")
            .arg("synthetic")
            .arg_owned(&tree),
    )
    .await;
    git.command("update-ref")
        .repo(repo)
        .arg("refs/heads/main")
        .arg_owned(&commit)
        .output()
        .await
        .expect("update-ref");
    commit
}

/// Run a stream to the end with `input` on stdin.
async fn run(cmd: GitCommand, input: Vec<u8>) -> (Vec<u8>, Result<crate::StreamStats, ForgeError>) {
    let mut stream = cmd.spawn_stream("user:1").await.expect("spawn");
    let mut stdin = stream.take_stdin().expect("stdin");
    let mut stdout = stream.take_stdout().expect("stdout");
    let writer = tokio::spawn(async move {
        let _ = stdin.write_all(&input).await; // git may stop reading early
        let _ = stdin.shutdown().await;
    });
    let mut out = Vec::new();
    stdout.read_to_end(&mut out).await.expect("read");
    writer.await.expect("join");
    (out, stream.finish().await)
}

fn nz(n: u64) -> std::num::NonZeroU64 {
    std::num::NonZeroU64::new(n).expect("non-zero")
}

fn strings(argv: &[std::ffi::OsString]) -> Vec<String> {
    argv.iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

#[tokio::test]
async fn upload_pack_argv_is_the_fixed_set() {
    let f = fixture().await;
    let (id, path) = f.repo(1).await;
    let path = path.to_string_lossy().into_owned();
    let hooks = format!("core.hooksPath={}", f.hooks());
    let advertise = f
        .git
        .upload_pack(&f.root, id, Phase::Advertise, None)
        .expect("cmd");
    assert_eq!(
        strings(&advertise.argv()),
        [
            "-c",
            "uploadpack.allowFilter=true",
            "-c",
            "uploadpack.keepAlive=5",
            "-c",
            "uploadpack.allowAnySHA1InWant=false",
            "-c",
            hooks.as_str(),
            "upload-pack",
            "--strict",
            "--stateless-rpc",
            "--advertise-refs",
            "--",
            path.as_str(),
        ]
    );
    let rpc = f
        .git
        .upload_pack(&f.root, id, Phase::Rpc, None)
        .expect("cmd");
    let mut want = strings(&advertise.argv());
    want.retain(|a| a != "--advertise-refs");
    assert_eq!(strings(&rpc.argv()), want);
}

#[tokio::test]
async fn receive_pack_argv_is_the_fixed_set() {
    let f = fixture().await;
    let (id, path) = f.repo(2).await;
    let path = path.to_string_lossy().into_owned();
    let hooks = format!("core.hooksPath={}", f.hooks());
    let cmd = f
        .git
        .receive_pack(&f.root, id, Phase::Rpc, None, nz(2 * 1024 * 1024 * 1024))
        .expect("cmd");
    assert_eq!(
        strings(&cmd.argv()),
        [
            "-c",
            "receive.fsckObjects=true",
            "-c",
            "receive.keepAlive=5",
            "-c",
            "receive.advertisePushOptions=false",
            "-c",
            "receive.maxInputSize=2147483648",
            "-c",
            hooks.as_str(),
            "receive-pack",
            "--stateless-rpc",
            "--",
            path.as_str(),
        ]
    );
}

/// The `Git-Protocol` header is the only request value that reaches git, and
/// only as a sanitised `GIT_PROTOCOL`, never in argv.
#[tokio::test]
async fn request_values_never_reach_argv() {
    let f = fixture().await;
    let (id, _) = f.repo(3).await;
    let plain = f
        .git
        .upload_pack(&f.root, id, Phase::Rpc, None)
        .expect("cmd")
        .argv();
    for header in [
        "version=2",
        "version=2 --upload-pack=touch /tmp/x",
        "version=2\nGIT_DIR=/etc",
        "-c core.sshCommand=x",
    ] {
        let cmd = f
            .git
            .upload_pack(&f.root, id, Phase::Rpc, Some(header))
            .expect("cmd");
        assert_eq!(cmd.argv(), plain, "{header:?}");
        let env = cmd.env_value("GIT_PROTOCOL");
        if header == "version=2" {
            assert_eq!(env, Some(OsStr::new("version=2")));
        } else {
            assert_eq!(env, None, "an invalid header is dropped: {header:?}");
        }
    }
}

#[test]
fn protocol_version_follows_what_git_will_see() {
    use ProtocolVersion::{V0, V1, V2};
    for (header, want) in [
        (None, V0),
        (Some("version=2"), V2),
        (Some("version=1"), V1),
        (Some("version=1:version=2"), V2),
        (Some("object-format=sha1:version=2"), V2),
        (Some("version=3"), V0),
        // Dropped by the sanitiser, so git runs v0: no v2 framing either.
        (Some("version=2 x"), V0),
    ] {
        assert_eq!(
            ProtocolVersion::from_git_protocol(header),
            want,
            "{header:?}"
        );
    }
    assert_eq!(
        Service::from_name("git-upload-pack"),
        Some(Service::UploadPack)
    );
    assert_eq!(
        Service::from_name("git-receive-pack"),
        Some(Service::ReceivePack)
    );
    assert_eq!(Service::from_name("git-upload-archive"), None);
}

#[test]
fn advertisement_prefix_bytes() {
    assert_eq!(
        advertisement_prefix(Service::UploadPack, ProtocolVersion::V0),
        b"001e# service=git-upload-pack\n0000"
    );
    assert_eq!(
        advertisement_prefix(Service::ReceivePack, ProtocolVersion::V2),
        b"001f# service=git-receive-pack\n0000"
    );
    assert!(advertisement_prefix(Service::UploadPack, ProtocolVersion::V2).is_empty());
}

#[tokio::test]
async fn v2_advertisement_has_no_service_prefix() {
    let f = fixture().await;
    let (id, path) = f.repo(4).await;
    commit(&f.git, &path, b"synthetic\n").await;
    let header = Some("version=2");
    let cmd = f
        .git
        .upload_pack(&f.root, id, Phase::Advertise, header)
        .expect("cmd");
    let (out, res) = run(cmd, Vec::new()).await;
    res.expect("exit 0");
    let mut body = advertisement_prefix(
        Service::UploadPack,
        ProtocolVersion::from_git_protocol(header),
    );
    body.extend_from_slice(&out);
    assert!(
        body.starts_with(b"000eversion 2\n"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    let packets = decode_all(&body, PktMode::V2).expect("pkt-lines");
    assert_eq!(packets.last(), Some(&Packet::Flush));
    assert!(packets
        .iter()
        .any(|p| matches!(p, Packet::Data(d) if d.starts_with(b"fetch="))));
}

#[tokio::test]
async fn v0_advertisement_has_the_service_prefix() {
    let f = fixture().await;
    let (id, path) = f.repo(5).await;
    let sha = commit(&f.git, &path, b"synthetic\n").await;
    let cmd = f
        .git
        .upload_pack(&f.root, id, Phase::Advertise, None)
        .expect("cmd");
    let (out, res) = run(cmd, Vec::new()).await;
    res.expect("exit 0");
    let mut body = advertisement_prefix(Service::UploadPack, ProtocolVersion::V0);
    body.extend_from_slice(&out);
    let packets = decode_all(&body, PktMode::V0).expect("pkt-lines");
    assert_eq!(
        packets.first(),
        Some(&Packet::Data(b"# service=git-upload-pack\n".to_vec()))
    );
    assert_eq!(packets.get(1), Some(&Packet::Flush));
    let Some(Packet::Data(first_ref)) = packets.get(2) else {
        panic!("a ref line follows: {packets:?}")
    };
    assert!(first_ref.starts_with(sha.as_bytes()), "HEAD is advertised");
    let caps = String::from_utf8_lossy(first_ref);
    assert!(caps.contains("filter"), "allowFilter took effect: {caps}");
    assert_eq!(packets.last(), Some(&Packet::Flush));
}

#[tokio::test]
async fn receive_pack_advertisement_keeps_the_prefix_for_a_v2_client() {
    let f = fixture().await;
    let (id, path) = f.repo(6).await;
    commit(&f.git, &path, b"synthetic\n").await;
    let header = Some("version=2");
    let cmd = f
        .git
        .receive_pack(&f.root, id, Phase::Advertise, header, nz(1 << 20))
        .expect("cmd");
    let (out, res) = run(cmd, Vec::new()).await;
    res.expect("exit 0");
    let mut body = advertisement_prefix(
        Service::ReceivePack,
        ProtocolVersion::from_git_protocol(header),
    );
    body.extend_from_slice(&out);
    let packets = decode_all(&body, PktMode::V0).expect("v0 framing");
    assert_eq!(
        packets.first(),
        Some(&Packet::Data(b"# service=git-receive-pack\n".to_vec()))
    );
    let Some(Packet::Data(first_ref)) = packets.get(2) else {
        panic!("refs follow: {packets:?}")
    };
    let caps = String::from_utf8_lossy(first_ref);
    assert!(caps.contains("report-status"), "{caps}");
    assert!(
        !caps.contains("push-options"),
        "push options are not advertised: {caps}"
    );
}

#[tokio::test]
async fn empty_hooks_dir_is_created_and_must_stay_empty() {
    let f = fixture().await;
    let (id, _) = f.repo(7).await;
    let dir = f.git.config().empty_hooks_dir();
    assert!(!dir.exists());
    f.git
        .upload_pack(&f.root, id, Phase::Rpc, None)
        .expect("created on first use");
    assert!(dir.is_dir());

    std::fs::write(dir.join("pre-receive"), "#!/bin/sh\nexit 0\n").expect("plant a hook");
    let err = f
        .git
        .receive_pack(&f.root, id, Phase::Rpc, None, nz(1 << 20))
        .expect_err("a hook in the empty dir");
    assert_eq!(err.code(), ErrorCode::GitUnavailable);
    let text = err.to_string();
    assert!(text.contains("reason=hooks_dir"), "{text}");
    assert!(text.contains("detail=not_empty"), "{text}");

    std::fs::remove_file(dir.join("pre-receive")).expect("rm");
    std::fs::remove_dir(&dir).expect("rmdir");
    std::fs::write(&dir, "not a dir").expect("file");
    let err = f
        .git
        .upload_pack(&f.root, id, Phase::Rpc, None)
        .expect_err("a file");
    assert!(err.to_string().contains("detail=not_a_directory"), "{err}");
}

/// The hooks dir is created private and refused when group / others could
/// plant a hook in it.
#[cfg(unix)]
#[tokio::test]
async fn empty_hooks_dir_must_not_be_writable_by_others() {
    use std::os::unix::fs::PermissionsExt as _;
    let f = fixture().await;
    let (id, _) = f.repo(12).await;
    let dir = f.git.config().empty_hooks_dir();
    f.git
        .upload_pack(&f.root, id, Phase::Rpc, None)
        .expect("created");
    let mode = std::fs::metadata(&dir).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "created private: {mode:o}");
    for bad in [0o777, 0o720, 0o702] {
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(bad)).expect("chmod");
        let err = f
            .git
            .upload_pack(&f.root, id, Phase::Rpc, None)
            .expect_err("writable by others");
        assert_eq!(err.code(), ErrorCode::GitUnavailable);
        let text = err.to_string();
        assert!(text.contains("detail=insecure_permissions"), "{text}");
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    f.git
        .upload_pack(&f.root, id, Phase::Rpc, None)
        .expect("0755 is fine: only forge writes");
}

#[tokio::test]
async fn a_missing_repository_is_storage_missing() {
    let f = fixture().await;
    let err = f
        .git
        .upload_pack(&f.root, RepoId::new(404), Phase::Rpc, None)
        .expect_err("no repo");
    assert_eq!(err.code(), ErrorCode::StorageMissing);
}

fn want_request_v0(oid: &str) -> Vec<u8> {
    let mut body = Vec::new();
    crate::pkt_line::encode_data(&mut body, format!("want {oid} side-band-64k\n").as_bytes())
        .expect("want");
    body.extend_from_slice(crate::pkt_line::FLUSH);
    crate::pkt_line::encode_data(&mut body, b"done\n").expect("done");
    body
}

fn want_request_v2(oid: &str) -> Vec<u8> {
    let mut body = Vec::new();
    crate::pkt_line::encode_data(&mut body, b"command=fetch\n").expect("cmd");
    body.extend_from_slice(crate::pkt_line::DELIM);
    crate::pkt_line::encode_data(&mut body, format!("want {oid}\n").as_bytes()).expect("want");
    crate::pkt_line::encode_data(&mut body, b"done\n").expect("done");
    body.extend_from_slice(crate::pkt_line::FLUSH);
    body
}

fn has_pack(out: &[u8]) -> bool {
    out.windows(4).any(|w| w == b"PACK")
}

#[tokio::test]
async fn upload_pack_rpc_serves_an_advertised_want() {
    let f = fixture().await;
    let (id, path) = f.repo(8).await;
    let sha = commit(&f.git, &path, b"synthetic\n").await;
    let cmd = f
        .git
        .upload_pack(&f.root, id, Phase::Rpc, None)
        .expect("cmd");
    let (out, res) = run(cmd, want_request_v0(&sha)).await;
    let stats = res.expect("exit 0");
    assert!(has_pack(&out), "a pack is sent");
    assert_eq!(stats.bytes_out, u64::try_from(out.len()).expect("len"));
}

/// Characterisation (ADR-0007 amendment, M1 phase 9): git's protocol v2
/// `fetch` serves any object in the repository, referenced or not, and
/// `uploadpack.allowAnySHA1InWant=false` does not change that; v0 refuses an
/// unreferenced commit. If a git upgrade changes this, this test says so.
#[tokio::test]
async fn v2_fetch_serves_unreferenced_objects_despite_allow_any_sha1_false() {
    let f = fixture().await;
    let (id, path) = f.repo(9).await;
    commit(&f.git, &path, b"visible\n").await;
    // A commit no ref points at (what a force push leaves behind).
    let old = commit(&f.git, &path, b"rewritten away\n").await;
    commit(&f.git, &path, b"visible again\n").await;

    let v0 = f
        .git
        .upload_pack(&f.root, id, Phase::Rpc, None)
        .expect("cmd");
    let (out, _) = run(v0, want_request_v0(&old)).await;
    assert!(!has_pack(&out), "v0 refuses an unreferenced commit");
    assert!(String::from_utf8_lossy(&out).contains("not our ref"));

    let v2 = f
        .git
        .upload_pack(&f.root, id, Phase::Rpc, Some("version=2"))
        .expect("cmd");
    let (out, res) = run(v2, want_request_v2(&old)).await;
    res.expect("exit 0");
    assert!(
        has_pack(&out),
        "v2 serves the unreferenced commit (open item for MR3: forge must check wants)"
    );

    // An unreferenced blob: served by v0 too (git's reachability check in
    // stateless-rpc only walks commits).
    let blob = captured(
        &f.git,
        f.git
            .command("hash-object")
            .repo(&path)
            .arg("-w")
            .arg("--stdin")
            .stdin(b"synthetic dangling blob\n".to_vec()),
    )
    .await;
    let v0 = f
        .git
        .upload_pack(&f.root, id, Phase::Rpc, None)
        .expect("cmd");
    let (out, _) = run(v0, want_request_v0(&blob)).await;
    assert!(has_pack(&out), "v0 serves an unreferenced blob");
}

/// A pack with one incompressible 64 KiB blob, and the push request that
/// creates `refs/heads/main` with it (report-status).
async fn push_request(f: &Fixture) -> (Vec<u8>, String) {
    let (_, src) = f.repo(100).await;
    let mut noise = Vec::with_capacity(64 * 1024);
    let mut x: u32 = 0x1234_5678;
    for _ in 0..64 * 1024 {
        // xorshift: synthetic incompressible bytes
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        noise.push(x.to_le_bytes()[0]);
    }
    let sha = commit(&f.git, &src, &noise).await;
    let pack = f
        .git
        .command("pack-objects")
        .repo(&src)
        .arg("--stdout")
        .arg("--revs")
        .arg("--quiet")
        .stdin(format!("{sha}\n").into_bytes())
        .output()
        .await
        .expect("pack")
        .stdout;
    let mut body = Vec::new();
    let zero = "0".repeat(40);
    crate::pkt_line::encode_data(
        &mut body,
        format!("{zero} {sha} refs/heads/main\0report-status\n").as_bytes(),
    )
    .expect("command");
    body.extend_from_slice(crate::pkt_line::FLUSH);
    body.extend_from_slice(&pack);
    (body, sha)
}

fn report(out: &[u8]) -> String {
    decode_all(out, PktMode::V0)
        .expect("report-status")
        .into_iter()
        .filter_map(|p| match p {
            Packet::Data(d) => Some(String::from_utf8_lossy(&d).into_owned()),
            Packet::Flush | Packet::Delim | Packet::ResponseEnd => None,
        })
        .collect()
}

#[tokio::test]
async fn receive_pack_applies_a_push_and_enforces_max_input_size() {
    let f = fixture().await;
    let (body, sha) = push_request(&f).await;

    let (small_id, small_path) = f.repo(10).await;
    let cmd = f
        .git
        .receive_pack(&f.root, small_id, Phase::Rpc, None, nz(1024))
        .expect("cmd");
    let (out, _) = run(cmd, body.clone()).await;
    let text = report(&out);
    assert!(text.starts_with("unpack "), "{text}");
    assert!(!text.starts_with("unpack ok"), "over maxInputSize: {text}");
    let missing = f
        .git
        .command("rev-parse")
        .repo(&small_path)
        .arg("--verify")
        .arg("--quiet")
        .arg("refs/heads/main")
        .output()
        .await;
    assert!(missing.is_err(), "no ref was created");

    let (id, path) = f.repo(11).await;
    let cmd = f
        .git
        .receive_pack(&f.root, id, Phase::Rpc, None, nz(10 << 20))
        .expect("cmd");
    let (out, res) = run(cmd, body).await;
    res.expect("exit 0");
    let text = report(&out);
    assert!(text.starts_with("unpack ok"), "{text}");
    assert!(text.contains("ok refs/heads/main"), "{text}");
    let head = captured(
        &f.git,
        f.git
            .command("rev-parse")
            .repo(&path)
            .arg("--verify")
            .arg("refs/heads/main"),
    )
    .await;
    assert_eq!(head, sha);
}
