//! Integration tests for the Git LFS fetch path.
//!
//! Uses the `lfs-sample` fixture under `tests/fixtures/`: a tiny working tree
//! with `*.bin filter=lfs` in `.gitattributes` (shipped as `dot-gitattributes`
//! so the outer repo doesn't apply LFS rules to it) and one binary file.
//!
//! All tests are guarded by `require_git_lfs()` so they skip cleanly on hosts
//! without `git-lfs` installed.

use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Command,
};

use rattler_git::{
    GitLfs, GitUrl, sha::GitSha, source::GitSource,
};
use rattler_networking::LazyClient;
use reqwest_middleware::ClientWithMiddleware;
use tempfile::TempDir;

/// File name a fixture uses in place of `.gitattributes` so the outer
/// (rattler) repo doesn't interpret the LFS filter rules. Renamed back to
/// `.gitattributes` when copied into the fixture's working repo.
const GITATTRIBUTES_PLACEHOLDER: &str = "dot-gitattributes";

/// True if `git lfs version` succeeds on this host.
fn git_lfs_available() -> bool {
    Command::new("git")
        .args(["lfs", "version"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Skip the test when `git-lfs` doesn't work on this host.
fn require_git_lfs(test: &str) -> bool {
    let ok = git_lfs_available();
    if !ok {
        eprintln!("skipping {test}: git-lfs is not installed");
    }
    ok
}

/// Returns whether `path` looks like a git-lfs pointer file.
fn is_lfs_pointer(path: &Path) -> bool {
    let contents = fs_err::read_to_string(path).unwrap_or_default();
    contents.starts_with("version https://git-lfs.github.com/spec/")
}

/// `LazyClient` whose initializer panics if HTTP is touched. file:// URLs
/// (and GitHub fast-path skipping for non-github hosts) never trigger it.
fn panic_client() -> LazyClient {
    LazyClient::new(|| -> ClientWithMiddleware {
        panic!("network should not be used in LFS tests")
    })
}

/// A temporary git repository created from a fixture directory.
///
/// Builds one commit per top-level subdirectory in sorted order (so
/// `001_x`, `002_y` etc. produce commits in that order). The commit
/// message is the part after the `_` prefix; if it starts with `v`,
/// a matching git tag is created. Stripped down from pixi's
/// `pixi_test_utils::GitRepoFixture`.
struct GitRepoFixture {
    _tempdir: TempDir,
    repo_path: PathBuf,
    base_url: url::Url,
    commits: Vec<String>,
    #[allow(dead_code)]
    tags: HashMap<String, String>,
    #[allow(dead_code)]
    uses_lfs: bool,
}

impl GitRepoFixture {
    fn new(fixture_name: &str) -> Self {
        let fixture_base = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(fixture_name);
        Self::from_path(&fixture_base, fixture_name)
    }

    fn from_path(fixture_base: &Path, repo_name: &str) -> Self {
        let tempdir = TempDir::new().expect("failed to create temp dir");
        let repo_path = tempdir.path().join(repo_name);
        fs_err::create_dir_all(&repo_path).expect("failed to create repo dir");

        git_in(&repo_path, &["init", "-b", "main"]);
        git_in(&repo_path, &["config", "user.email", "test@test.com"]);
        git_in(&repo_path, &["config", "user.name", "Test"]);
        // Defeat any global commit/tag signing config so the fixture is
        // self-contained on hosts that mandate signed commits.
        git_in(&repo_path, &["config", "commit.gpgsign", "false"]);
        git_in(&repo_path, &["config", "tag.gpgsign", "false"]);

        // Determine commit directories. If `fixture_base` has numbered
        // subdirectories, use them; otherwise treat the whole fixture dir
        // as a single commit named `v0.1.0`.
        let mut commit_dirs: Vec<_> = fs_err::read_dir(fixture_base)
            .expect("failed to read fixture dir")
            .filter_map(Result::ok)
            .filter(|e| e.path().is_dir())
            .collect();
        commit_dirs.sort_by_key(fs_err::DirEntry::file_name);

        let single_commit = commit_dirs.is_empty();
        let uses_lfs = if single_commit {
            dir_uses_lfs(fixture_base)
        } else {
            commit_dirs.iter().any(|d| dir_uses_lfs(&d.path()))
        };

        if uses_lfs {
            assert!(
                git_lfs_available(),
                "git-lfs is required for fixture '{repo_name}'"
            );
            git_in(&repo_path, &["lfs", "install", "--local"]);
        }

        let mut commits = Vec::new();
        let mut tags = HashMap::new();

        let scripted_commits: Vec<(PathBuf, String)> = if single_commit {
            vec![(fixture_base.to_path_buf(), "v0.1.0".to_string())]
        } else {
            commit_dirs
                .into_iter()
                .map(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy().into_owned();
                    let msg = name
                        .split_once('_')
                        .map_or_else(|| name.clone(), |(_, msg)| msg.to_string());
                    (entry.path(), msg)
                })
                .collect()
        };

        for (src, commit_msg) in scripted_commits {
            copy_dir_contents(&src, &repo_path);
            git_in(&repo_path, &["add", "."]);
            git_in(&repo_path, &["commit", "--message", &commit_msg]);

            let hash = git_stdout(&repo_path, &["rev-parse", "HEAD"]);

            if commit_msg.starts_with('v') {
                git_in(&repo_path, &["tag", &commit_msg]);
                tags.insert(commit_msg.clone(), hash.clone());
            }
            commits.push(hash);
        }

        let base_url =
            url::Url::from_directory_path(&repo_path).expect("failed to create URL from repo path");

        Self {
            _tempdir: tempdir,
            repo_path,
            base_url,
            commits,
            tags,
            uses_lfs,
        }
    }

    fn latest_commit(&self) -> &str {
        self.commits.last().expect("no commits in fixture")
    }

    fn git(&self, args: &[&str]) -> String {
        git_stdout(&self.repo_path, args)
    }
}

/// Recursively copy contents from `src` to `dst`, renaming
/// `dot-gitattributes` to `.gitattributes` along the way.
fn copy_dir_contents(src: &Path, dst: &Path) {
    for entry in fs_err::read_dir(src).expect("failed to read fixture dir") {
        let entry = entry.expect("failed to read dir entry");
        let src_path = entry.path();
        let dst_path = dst.join(dest_name(&entry.file_name()));

        if src_path.is_dir() {
            fs_err::create_dir_all(&dst_path).expect("failed to create dir");
            copy_dir_contents(&src_path, &dst_path);
        } else {
            fs_err::copy(&src_path, &dst_path).expect("failed to copy file");
        }
    }
}

fn dest_name(name: &OsStr) -> OsString {
    if name == OsStr::new(GITATTRIBUTES_PLACEHOLDER) {
        OsString::from(".gitattributes")
    } else {
        name.to_owned()
    }
}

/// True if any gitattributes file directly under `dir` configures the lfs filter.
fn dir_uses_lfs(dir: &Path) -> bool {
    let Ok(entries) = fs_err::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if dir_uses_lfs(&path) {
                return true;
            }
        } else {
            let name = path.file_name();
            let is_attrs = name == Some(OsStr::new(".gitattributes"))
                || name == Some(OsStr::new(GITATTRIBUTES_PLACEHOLDER));
            if is_attrs && gitattributes_uses_lfs(&path) {
                return true;
            }
        }
    }
    false
}

fn gitattributes_uses_lfs(path: &Path) -> bool {
    let Ok(contents) = fs_err::read_to_string(path) else {
        return false;
    };
    contents.lines().any(|line| {
        let line = line.trim();
        !line.starts_with('#') && line.contains("filter=lfs")
    })
}

fn git_in(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|err| panic!("failed to spawn `git {}`: {err}", args.join(" ")));
    assert!(
        output.status.success(),
        "`git {}` failed: stdout={:?} stderr={:?}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn git_stdout(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|err| panic!("failed to spawn `git {}`: {err}", args.join(" ")));
    assert!(
        output.status.success(),
        "`git {}` failed: stdout={:?} stderr={:?}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout)
        .expect("git output must be utf-8")
        .trim()
        .to_string()
}

/// Sanity: the fixture builds, LFS gets installed, and `data.bin` is stored
/// as an LFS pointer with the blob present under `.git/lfs/objects/`.
#[test]
fn fixture_builds_with_lfs() {
    if !require_git_lfs("fixture_builds_with_lfs") {
        return;
    }
    let repo = GitRepoFixture::new("lfs-sample");
    assert!(repo.uses_lfs, "fixture should auto-detect LFS");

    let pointer = repo.git(&["show", "HEAD:data.bin"]);
    assert!(
        pointer.starts_with("version https://git-lfs.github.com/spec/"),
        "HEAD:data.bin should be an LFS pointer, got: {pointer:?}"
    );

    let objects = repo.repo_path.join(".git/lfs/objects");
    assert!(
        objects.is_dir() && fs_err::read_dir(&objects).unwrap().next().is_some(),
        "expected LFS objects under {}",
        objects.display()
    );
}

/// `GitLfs::Disabled` (the default) keeps `GIT_LFS_SKIP_SMUDGE=1` set on the
/// `git reset --hard`, so pointer files survive into the checkout.
#[test]
fn fetch_without_lfs_leaves_pointer() {
    if !require_git_lfs("fetch_without_lfs_leaves_pointer") {
        return;
    }
    let repo = GitRepoFixture::new("lfs-sample");
    let cache = TempDir::new().unwrap();

    let git_url = GitUrl::try_from(repo.base_url.clone())
        .unwrap()
        .with_lfs(GitLfs::Disabled);
    let fetch = GitSource::new(git_url, panic_client(), cache.path())
        .fetch()
        .expect("fetch should succeed");

    assert!(!fetch.lfs_ready(), "LFS was not requested");
    let data = fetch.path().join("data.bin");
    assert!(data.is_file(), "data.bin missing from checkout");
    assert!(
        is_lfs_pointer(&data),
        "data.bin should still be a pointer when LFS is disabled"
    );
}

/// `GitLfs::Enabled` runs `git lfs fetch`, validates with `git lfs fsck`,
/// and materialises the real blob in the checkout.
#[test]
fn fetch_with_lfs_materialises_blob() {
    if !require_git_lfs("fetch_with_lfs_materialises_blob") {
        return;
    }
    let repo = GitRepoFixture::new("lfs-sample");
    let original = fs_err::read(repo.repo_path.join("data.bin")).unwrap();
    let cache = TempDir::new().unwrap();

    let git_url = GitUrl::try_from(repo.base_url.clone())
        .unwrap()
        .with_lfs(GitLfs::Enabled);
    let fetch = GitSource::new(git_url, panic_client(), cache.path())
        .fetch()
        .expect("fetch should succeed");

    assert!(
        fetch.lfs_ready(),
        "fsck should pass for a healthy LFS fixture"
    );
    let data = fetch.path().join("data.bin");
    assert!(data.is_file());
    assert!(
        !is_lfs_pointer(&data),
        "data.bin should be the real blob, not a pointer"
    );
    let got = fs_err::read(&data).unwrap();
    assert_eq!(
        got, original,
        "checked-out data.bin should match fixture source"
    );
}

/// Second fetch against a warm cache (same `cache` dir, same precise rev)
/// hits the cached-DB branch in `GitSource::fetch`. With LFS requested, the
/// branch requires `db.contains_lfs_artifacts(rev)`; the first fetch
/// populated `.git/lfs/objects/`, so the second fetch returns
/// `lfs_ready == true` without touching the remote.
#[test]
fn cached_fetch_with_lfs_artifacts_is_ready() {
    if !require_git_lfs("cached_fetch_with_lfs_artifacts_is_ready") {
        return;
    }
    let repo = GitRepoFixture::new("lfs-sample");
    let cache = TempDir::new().unwrap();
    let head: GitSha = repo.latest_commit().parse().unwrap();

    let make_source = || {
        let url = GitUrl::try_from(repo.base_url.clone())
            .unwrap()
            .with_lfs(GitLfs::Enabled)
            .with_precise(head);
        GitSource::new(url, panic_client(), cache.path())
    };

    // Warm the cache.
    let first = make_source().fetch().expect("first fetch should succeed");
    assert!(first.lfs_ready());

    // Second fetch reuses the DB and skips the network entirely while
    // still reporting LFS as ready.
    let second = make_source().fetch().expect("cached fetch should succeed");
    assert!(second.lfs_ready());
    assert_eq!(second.commit(), first.commit());
}
