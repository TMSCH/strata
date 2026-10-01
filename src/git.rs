//! Verify one immutable tree captured from Git's index, without locking a daemon.
use crate::{
    layout::{self, Snapshot},
    store::Verification,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    io::BufReader,
    path::Path,
    process::{Command, Stdio},
};

#[derive(Debug, Serialize)]
pub struct StagedVerification {
    #[serde(flatten)]
    pub verification: Verification,
    /// Commit exactly this tree, or keep exclusive control of the unchanged index.
    pub tree: String,
    pub baseline_commit: Option<String>,
}

fn command(repo: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo);
    cmd
}

fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = command(repo).args(args).output().context("run Git")?;
    ensure!(
        output.status.success(),
        "git {}: {}",
        args[0],
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(output.stdout)
}

fn oid(repo: &Path, args: &[&str]) -> Result<String> {
    let value = String::from_utf8(git(repo, args)?)?.trim().to_owned();
    ensure!(
        matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit()),
        "unexpected Git object ID"
    );
    Ok(value)
}

fn read_tree(repo: &Path, tree: &str, path: &str) -> Result<Snapshot> {
    let listing = git(repo, &["ls-tree", "-r", "-z", "--full-tree", tree])?;
    let prefix = format!("{path}/");
    let mut snapshot = Snapshot::default();
    for entry in listing.split(|b| *b == 0).filter(|b| !b.is_empty()) {
        let tab = entry
            .iter()
            .position(|b| *b == b'\t')
            .context("invalid Git tree entry")?;
        let name = &entry[tab + 1..];
        ensure!(name != path.as_bytes(), "store path is a file or submodule");
        let Some(relative) = name.strip_prefix(prefix.as_bytes()) else {
            continue;
        };
        let relative = std::str::from_utf8(relative).context("non-UTF-8 store path")?;
        let header = std::str::from_utf8(&entry[..tab])?;
        let parts: Vec<_> = header.split(' ').collect();
        ensure!(
            parts.len() == 3 && parts[0] == "100644" && parts[1] == "blob",
            "expected ordinary non-executable file: {relative}"
        );
        if layout::auxiliary(relative) {
            continue;
        }
        layout::location(relative).with_context(|| {
            format!("unexpected tracked store file: {relative}; exclude local lock/temporary files")
        })?;
        let mut child = command(repo)
            .args(["cat-file", "blob", parts[2]])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let result = snapshot.add(relative, BufReader::new(child.stdout.take().unwrap()));
        if result.is_err() {
            let _ = child.kill();
        }
        let status = child.wait()?;
        result?;
        ensure!(status.success(), "cannot read Git blob for {relative}");
    }
    snapshot.validate()?;
    Ok(snapshot)
}

/// Verify staged events and preservation of a trusted commit (HEAD by default).
/// `initial` explicitly permits the first commit in an unborn repository.
/// The caller owns index/ref coordination and must commit the returned tree.
pub fn verify_staged(
    repo: &Path,
    path: &str,
    baseline_ref: Option<&str>,
    initial: bool,
) -> Result<StagedVerification> {
    ensure!(
        !path.is_empty()
            && path
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".." && part != ".git")
            && !path.contains('\\'),
        "--path must be a normalized repository-relative directory"
    );
    ensure!(
        !initial || baseline_ref.is_none(),
        "--initial conflicts with --baseline-ref"
    );
    let baseline_commit = if initial {
        ensure!(
            !command(repo)
                .args(["rev-parse", "--verify", "HEAD"])
                .output()?
                .status
                .success(),
            "--initial is only allowed before the first commit"
        );
        None
    } else {
        let reference = format!("{}^{{commit}}", baseline_ref.unwrap_or("HEAD"));
        Some(
            oid(
                repo,
                &["rev-parse", "--verify", "--end-of-options", &reference],
            )
            .context("resolve trusted baseline; use --initial only before the first commit")?,
        )
    };
    // Captures the entire index once. Subsequent staging cannot change this tree.
    let tree = oid(repo, &["write-tree"])?;
    let current = read_tree(repo, &tree, path)?;
    let mut verification = current.status();
    if let Some(commit) = &baseline_commit {
        let baseline = read_tree(repo, commit, path)?;
        current.preserves(&baseline)?;
        verification.baseline_events = Some(baseline.records.len() as u64);
    }
    Ok(StagedVerification {
        verification,
        tree,
        baseline_commit,
    })
}

/// Cleanup must not mistake Git failure for absence of a repository. Discover
/// filesystem markers ourselves, including linked worktrees/submodules (.git files).
/// Ignore inherited Git routing variables so they cannot select another repository.
pub(crate) fn archives_committed(dir: &Path, archives: &[String]) -> Result<bool> {
    let dir = dir.canonicalize()?;
    let mut root = None;
    for ancestor in dir.ancestors() {
        match std::fs::symlink_metadata(ancestor.join(".git")) {
            Ok(_) => {
                root = Some(ancestor);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        // A bare repository is not an ordinary non-Git directory for cleanup.
        if ancestor.join("HEAD").exists()
            && ancestor.join("objects").is_dir()
            && ancestor.join("refs").is_dir()
        {
            return Ok(false);
        }
    }
    let Some(root) = root else {
        return Ok(true);
    };
    let run = |args: &[&std::ffi::OsStr]| -> Result<Vec<u8>> {
        let mut cmd = command(root);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                cmd.env_remove(key);
            }
        }
        let output = cmd
            .args(args)
            .output()
            .context("check committed archives")?;
        ensure!(
            output.status.success(),
            "Git could not confirm committed archives; retaining loose events"
        );
        Ok(output.stdout)
    };
    use std::ffi::OsStr;
    let head = run(&[
        OsStr::new("rev-parse"),
        OsStr::new("--verify"),
        OsStr::new("HEAD^{commit}"),
    ])?;
    let head = std::str::from_utf8(&head)?.trim();
    for archive in archives {
        let path = dir.join(archive);
        let relative = path.strip_prefix(root)?;
        // Literal pathspec handles spaces, brackets, and colon-prefixed folders.
        let mut spec = std::ffi::OsString::from(":(literal)");
        spec.push(relative);
        let entry = run(&[
            OsStr::new("ls-tree"),
            OsStr::new("-z"),
            OsStr::new("--full-tree"),
            OsStr::new(head),
            OsStr::new("--"),
            &spec,
        ])?;
        let Some(tab) = entry.iter().position(|b| *b == b'\t') else {
            return Ok(false);
        };
        let header = std::str::from_utf8(&entry[..tab])?;
        let Some(blob) = header.strip_prefix("100644 blob ") else {
            return Ok(false);
        };
        let actual = run(&[
            OsStr::new("hash-object"),
            OsStr::new("--no-filters"),
            OsStr::new("--"),
            path.as_os_str(),
        ])?;
        if std::str::from_utf8(&actual)?.trim() != blob {
            return Ok(false);
        }
    }
    // Ordinary forward commits keep immutable archives. Rewinds/checkouts and
    // external edits still require stopping the daemon, as with all store files.
    Ok(true)
}
