//! Native engine tests against real colocated jj repositories. They run `git` and the pinned `jj`
//! CLI only to build fixtures.

use std::path::{Path, PathBuf};

use crate::native::{self, Native, block_on};
use crate::run;

fn git(dir: &Path, args: &[&str]) -> String {
    run::output(dir, "git", args).unwrap()
}

fn jj(dir: &Path, args: &[&str]) -> String {
    run::output(dir, "jj", args).unwrap()
}

fn commit(dir: &Path, file: &str, message: &str) {
    std::fs::write(dir.join(file), format!("{message}\n")).unwrap();
    git(dir, &["add", "."]);
    git(
        dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@e",
            "commit",
            "-qm",
            message,
        ],
    );
}

struct Fixture {
    dir: tempfile::TempDir,
    work: PathBuf,
}

impl Fixture {
    /// A bare `origin` whose `main` has one commit, plus `branches` each one commit ahead of
    /// main, and a colocated jj clone with an identity in its repo config.
    fn new(branches: &[&str]) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir(&src).unwrap();
        git(&src, &["init", "-q", "-b", "main"]);
        commit(&src, "base", "base");
        for branch in branches {
            git(&src, &["checkout", "-q", "-b", branch, "main"]);
            commit(&src, &branch.replace('/', "_"), branch);
        }
        git(&src, &["checkout", "-q", "main"]);
        git(dir.path(), &["clone", "-q", "--bare", "src", "remote.git"]);
        git(dir.path(), &["clone", "-q", "remote.git", "work"]);
        let work = dir.path().join("work");
        jj(&work, &["git", "init", "--colocate"]);
        let fixture = Fixture { dir, work };
        fixture.add_repo_config("[user]\nname = \"t\"\nemail = \"t@e\"\n");
        fixture
    }

    fn remote(&self) -> PathBuf {
        self.dir.path().join("remote.git")
    }

    fn remote_refs(&self) -> String {
        git(&self.remote(), &["for-each-ref"])
    }

    /// Appends TOML to the repository's jj config, wherever jj keeps it.
    fn add_repo_config(&self, toml: &str) {
        let path = PathBuf::from(jj(&self.work, &["config", "path", "--repo"]));
        let mut text = std::fs::read_to_string(&path).unwrap_or_default();
        text.push_str(toml);
        std::fs::write(&path, text).unwrap();
    }

    /// A Git executable that records its arguments in `name.log`, then runs Git.
    fn git_wrapper(&self, name: &str) -> (PathBuf, PathBuf) {
        let script = self.dir.path().join(name);
        let log = self.dir.path().join(format!("{name}.log"));
        let real = String::from_utf8(
            std::process::Command::new("sh")
                .args(["-c", "command -v git"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap();
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\nexec '{}' \"$@\"\n",
                log.display(),
                real.trim()
            ),
        )
        .unwrap();
        run::output(self.dir.path(), "chmod", &["+x", &script.to_string_lossy()]).unwrap();
        (script, log)
    }

    /// A new local bookmark `name` on a new commit described `message` on top of main.
    fn local_series(&self, name: &str, message: &str) -> jj_lib::backend::CommitId {
        jj(&self.work, &["new", "--quiet", "main", "-m", message]);
        std::fs::write(self.work.join(name.replace('/', "_")), "local\n").unwrap();
        jj(
            &self.work,
            &["bookmark", "create", "--quiet", name, "-r", "@"],
        );
        let hex = jj(
            &self.work,
            &["log", "--no-graph", "-r", "@", "-T", "commit_id"],
        );
        native::parse_id(&hex).unwrap()
    }
}

fn update(name: &str, after: jj_lib::backend::CommitId) -> native::PushUpdate {
    native::PushUpdate {
        name: name.into(),
        before: None,
        after,
    }
}

/// The no-silent-drop guard distinguishes a deliberate deletion (tracked on the remote, deleted
/// locally) from an untracked remote series this clone never adopted, and judges a series by
/// its local bookmark, which is what gets pushed.
#[test]
fn guard_keeps_deliberate_deletions_and_refuses_untracked_or_unmerged_series() {
    let f = Fixture::new(&[
        "patch/merged",
        "patch/deleted",
        "patch/untracked",
        "patch/ahead",
    ]);
    for name in ["patch/merged", "patch/deleted", "patch/ahead"] {
        jj(&f.work, &["bookmark", "track", name, "--remote", "origin"]);
    }
    jj(&f.work, &["bookmark", "delete", "patch/deleted"]);
    // patch/ahead's local bookmark moves past what the merge contains.
    jj(&f.work, &["new", "--quiet", "patch/ahead", "-m", "ahead"]);
    std::fs::write(f.work.join("ahead-local"), "x\n").unwrap();
    jj(
        &f.work,
        &["bookmark", "set", "--quiet", "patch/ahead", "-r", "@"],
    );
    // The candidate merges patch/merged and the remote's patch/ahead, nothing else.
    jj(
        &f.work,
        &[
            "new",
            "--quiet",
            "patch/merged",
            "patch/ahead@origin",
            "-m",
            "merge",
        ],
    );
    let merge = native::parse_id(&jj(
        &f.work,
        &["log", "--no-graph", "-r", "@", "-T", "commit_id"],
    ))
    .unwrap();
    let native = Native::load(&f.work).unwrap();
    let prefixes = vec!["patch/".to_string()];
    let dropped =
        native::unmerged_remote_bookmarks(native.repo.as_ref(), "origin", &prefixes, &merge)
            .unwrap();
    assert_eq!(dropped, vec!["patch/ahead", "patch/untracked"]);

    // Once the untracked series is adopted and merged, only the unmerged local work remains.
    jj(
        &f.work,
        &["bookmark", "track", "patch/untracked", "--remote", "origin"],
    );
    jj(
        &f.work,
        &[
            "new",
            "--quiet",
            "patch/merged",
            "patch/ahead",
            "patch/untracked",
            "-m",
            "all",
        ],
    );
    let all = native::parse_id(&jj(
        &f.work,
        &["log", "--no-graph", "-r", "@", "-T", "commit_id"],
    ))
    .unwrap();
    let native = Native::load(&f.work).unwrap();
    assert!(
        native::unmerged_remote_bookmarks(native.repo.as_ref(), "origin", &prefixes, &all)
            .unwrap()
            .is_empty()
    );
}

/// Push policy is jj's configuration as resolved for `git push`: a push-scoped
/// `git.private-commits` refuses a private commit before any network use, while the planning
/// context does not see that scope; a push-scoped `git.executable-path` is used for pushes only.
#[test]
fn push_honors_git_push_scoped_private_commits_and_executable() {
    let f = Fixture::new(&[]);
    let (push_git, push_log) = f.git_wrapper("push-git");
    f.add_repo_config(&format!(
        "[[--scope]]\n--when.commands = [\"git push\"]\n[--scope.git]\nprivate-commits = \"description(glob:'private*')\"\nexecutable-path = '{}'\n",
        push_git.display()
    ));
    let private = f.local_series("patch/private", "private change");
    let public = f.local_series("patch/public", "public change");
    let mut native = Native::load(&f.work).unwrap();
    let settings = native.effective_settings();
    assert_eq!(settings.fork.private_commits, "\"none()\"");
    assert!(settings.push.private_commits.contains("private"));
    assert_eq!(settings.push.git_executable, push_git);
    assert_ne!(settings.fetch.git_executable, push_git);

    let remote_before = f.remote_refs();
    let heads_before = native.op_heads().to_vec();
    let err = native
        .push_explicit("origin", &[update("patch/private", private.clone())])
        .unwrap_err();
    assert!(err.to_string().contains("is private"), "{err:#}");
    assert!(
        !push_log.exists(),
        "a refused push must not reach the network"
    );
    assert_eq!(f.remote_refs(), remote_before);
    let heads = block_on(native.repo.loader().op_heads_store().get_op_heads()).unwrap();
    assert_eq!(heads, heads_before, "a refused push records nothing");
    assert_eq!(
        native::bookmark(native.repo.as_ref(), "patch/private").unwrap(),
        Some(private)
    );

    let report = native
        .push_explicit("origin", &[update("patch/public", public.clone())])
        .unwrap();
    assert!(report.all_accepted(), "{:?}", report.lines());
    assert!(std::fs::read_to_string(&push_log).unwrap().contains("push"));
    assert_eq!(
        git(&f.remote(), &["rev-parse", "patch/public"]),
        public.to_string()
    );

    // Fetching resolves as `git fetch`, so the push-only executable is not used.
    let pushes = std::fs::read_to_string(&push_log).unwrap();
    native.fetch_remote("origin").unwrap();
    assert_eq!(std::fs::read_to_string(&push_log).unwrap(), pushes);
}

/// `git.sign-on-push` would rewrite the checked commit while pushing, so an unsigned commit is
/// refused before any network use and nothing changes locally or on the remote. The same
/// setting outside the `git push` scope does not apply to pushes.
#[test]
fn push_refuses_unsigned_commits_under_push_scoped_sign_on_push() {
    let f = Fixture::new(&[]);
    let (push_git, push_log) = f.git_wrapper("push-git");
    f.add_repo_config(&format!(
        "[git]\nexecutable-path = '{}'\n[[--scope]]\n--when.commands = [\"git push\"]\n[--scope.git]\nsign-on-push = true\n",
        push_git.display()
    ));
    let checked = f.local_series("patch/signed", "checked change");
    let mut native = Native::load(&f.work).unwrap();
    assert!(native.effective_settings().push.sign_on_push);
    assert!(!native.effective_settings().fork.sign_on_push);
    let remote_before = f.remote_refs();
    let err = native
        .push_explicit("origin", &[update("patch/signed", checked.clone())])
        .unwrap_err();
    assert!(err.to_string().contains("sign-on-push"), "{err:#}");
    assert!(
        !push_log.exists(),
        "a refused push must not reach the network"
    );
    assert_eq!(f.remote_refs(), remote_before);
    assert_eq!(
        native::bookmark(native.repo.as_ref(), "patch/signed").unwrap(),
        Some(checked.clone())
    );

    // Scoped to another command, the same setting leaves pushes alone.
    let g = Fixture::new(&[]);
    g.add_repo_config(
        "[[--scope]]\n--when.commands = [\"git fetch\"]\n[--scope.git]\nsign-on-push = true\n",
    );
    let checked = g.local_series("patch/plain", "checked change");
    let mut native = Native::load(&g.work).unwrap();
    assert!(!native.effective_settings().push.sign_on_push);
    let report = native
        .push_explicit("origin", &[update("patch/plain", checked.clone())])
        .unwrap();
    assert!(report.all_accepted(), "{:?}", report.lines());
    assert_eq!(
        git(&g.remote(), &["rev-parse", "patch/plain"]),
        checked.to_string()
    );
}
