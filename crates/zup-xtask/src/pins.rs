//! Reading and refreshing `github-actions.lock.json`.
//!
//! Two commands with different failure modes. `check` is a gate: it runs in every CI
//! job, is offline by default, and fails the build, because the failure it prevents
//! is silent — a workflow on `actions/checkout@v4` produces a green build running
//! unmaintained code. `refresh` is an edit: it reaches the network and changes a
//! file, and it must never run inside a build, because a ref that moved under
//! somebody who only regenerated a matrix is a supply-chain change they did not make.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use zup_publish_github::{
    ActionPin, LOCK_PATH, LOCK_SCHEMA, PinLock, generated_actions, infrastructure_actions, lock,
    lock_json, pins,
};

/// What `check` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Actions the lock tracks.
    pub tracked: usize,
    /// Things that are wrong. Empty is a pass.
    pub problems: Vec<Problem>,
    /// Newer major series, when `--online` asked for them.
    pub newer: Vec<Outdated>,
    /// Refs that no longer point where the lock recorded, when `--online` asked.
    pub moved: Vec<Moved>,
    /// A one-line summary for a human.
    pub detail: String,
}

impl Report {
    /// Whether CI should pass.
    pub fn is_clean(&self) -> bool {
        self.problems.is_empty()
    }
}

/// One thing that is wrong with the lock or with the workflows using it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// What the check was looking at: a workflow path, or the lock.
    pub subject: String,
    /// What is wrong, in one sentence a developer can act on.
    pub detail: String,
}

/// A tracked ref that is behind the newest major series upstream publishes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outdated {
    /// `owner/name`.
    pub repository: String,
    /// The ref the workflows use.
    pub tracked: String,
    /// The newer series upstream publishes.
    pub available: String,
}

/// A tracked ref that no longer points at the commit the lock recorded.
///
/// The failure a version ref cannot show on its own, and the reason the lock
/// stores a SHA at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    /// `owner/name`.
    pub repository: String,
    /// The ref, e.g. `v7`.
    pub ref_name: String,
    /// The commit the lock recorded on its `checkedAt` date.
    pub recorded: String,
    /// The commit the ref points at now.
    pub actual: String,
}

/// The upstream a lock is resolved against.
///
/// Deliberately private: every caller passes a repository name so the URL is built
/// in one place. Git answers a doubled URL with `remote: Not Found`, and since
/// `check --online` treats an unreachable repository as nothing to report, that
/// failure is indistinguishable from a clean result.
fn upstream(repository: &str) -> String {
    format!("https://github.com/{repository}.git")
}

/// Check the lock, and every committed workflow that uses it.
pub fn check(root: &Path, online: bool) -> Report {
    let mut problems = Vec::new();
    let current = lock();

    if current.schema != LOCK_SCHEMA {
        problems.push(Problem {
            subject: LOCK_PATH.to_owned(),
            detail: format!(
                "schema is {}, and this build understands {LOCK_SCHEMA}; rebuild zup or migrate the lock"
            , current.schema),
        });
    }

    for (repository, action) in &current.actions {
        problems.extend(verify(repository, action));
    }

    problems.extend(required_presence());
    problems.extend(committed_workflows(root));

    let mut newer = Vec::new();
    let mut moved = Vec::new();
    if online {
        let (ahead, gone) = report_online(&current);
        newer = ahead;
        moved = gone;
    }

    let detail = if !problems.is_empty() {
        format!("{} problem(s)", problems.len())
    } else if online && !moved.is_empty() {
        // A moved tag is a supply-chain event, not housekeeping, so it outranks a
        // newer series.
        format!(
            "{} actions tracked; {} ref(s) now point somewhere else",
            current.actions.len(),
            moved.len()
        )
    } else if online && !newer.is_empty() {
        format!(
            "{} actions tracked; {} have a newer major series (run `cargo xtask github-action-pins refresh`)",
            current.actions.len(),
            newer.len()
        )
    } else {
        format!("{} actions tracked", current.actions.len())
    };

    Report {
        tracked: current.actions.len(),
        problems,
        newer,
        moved,
        detail,
    }
}

/// One entry's own consistency.
fn verify(repository: &str, action: &zup_publish_github::LockedAction) -> Vec<Problem> {
    let mut problems = Vec::new();
    let subject = format!("{LOCK_PATH} → {repository}");

    if action.sha.len() != 40 || !action.sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        problems.push(Problem {
            subject: subject.clone(),
            detail: format!(
                "`sha` is `{}`, which is not a full 40-character commit SHA",
                action.sha
            ),
        });
    } else if action.sha.bytes().any(|byte| byte.is_ascii_uppercase()) {
        problems.push(Problem {
            subject: subject.clone(),
            detail: "`sha` is uppercase; a commit SHA is lowercase everywhere else".to_owned(),
        });
    }

    // A `uses:` line is machine-parsed by GitHub and read by dependabot, so a ref
    // with a space or a comment in it fails on a runner rather than here.
    if action.version.trim().is_empty()
        || action
            .version
            .chars()
            .any(|c| c.is_whitespace() || c == '#' || c == '@' || c == '/')
    {
        problems.push(Problem {
            subject: subject.clone(),
            detail: format!(
                "`version` is `{}`, which is not a ref a workflow can use after `@`",
                action.version
            ),
        });
    }

    if !is_date(&action.checked_at) {
        problems.push(Problem {
            subject,
            detail: format!(
                "`checkedAt` is `{}`, which is not an ISO `YYYY-MM-DD` date",
                action.checked_at
            ),
        });
    }

    problems
}

fn is_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && bytes[4] == b'-'
        && bytes[5..7].iter().all(u8::is_ascii_digit)
        && bytes[7] == b'-'
        && bytes[8..].iter().all(u8::is_ascii_digit)
}

/// Actions zup's generated workflows use but the lock does not name.
fn required_presence() -> Vec<Problem> {
    let locked: BTreeMap<&str, ()> = pins()
        .iter()
        .map(|pin| (pin.repository.as_str(), ()))
        .collect();
    generated_actions()
        .iter()
        .filter(|repository| !locked.contains_key(**repository))
        .map(|repository| Problem {
            subject: LOCK_PATH.to_owned(),
            detail: format!(
                "`{repository}` is missing; a generated workflow uses it. Add it with \
                 `cargo xtask github-action-pins refresh --add {repository}`"
            ),
        })
        .collect()
}

/// Every `uses:` line in every committed workflow that names a locked action.
///
/// What makes the lock load-bearing rather than decorative: it catches a workflow
/// hand-edited to a ref the lock does not track.
fn committed_workflows(root: &Path) -> Vec<Problem> {
    let mut problems = Vec::new();
    let directory = root.join(".github").join("workflows");
    let Ok(entries) = std::fs::read_dir(&directory) else {
        return problems;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("yml"))
                || path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("yaml"))
        })
        .collect();
    files.sort();

    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        let relative = file
            .strip_prefix(root)
            .unwrap_or(&file)
            .display()
            .to_string()
            .replace('\\', "/");
        for (number, line) in text.lines().enumerate() {
            let Some(reference) = uses_reference(line) else {
                continue;
            };
            let (repository, revision) = reference
                .split_once('@')
                .unwrap_or((reference.as_str(), ""));
            let Some(locked) = pins()
                .iter()
                .find(|pin| pin.repository == repository.trim())
            else {
                // A workflow may use an action zup does not track — a project's own
                // action, a fixture. That is not this lock's business.
                continue;
            };
            if revision == locked.version {
                continue;
            }
            // Drift is the failure a version ref makes possible: a developer bumped
            // one file in a hurry, or a bot bumped a subset. A full commit SHA in a
            // workflow is reported too, and differently — it is not drift, it is a
            // file that no longer matches the repository's convention.
            let remedy = if locked.is_series() && looks_like_a_series(revision) {
                format!("expected `@{}`", locked.version)
            } else {
                format!(
                    "expected `@{}`, which resolves to {} as of {}",
                    locked.version, locked.sha, locked.checked_at
                )
            };
            problems.push(Problem {
                subject: format!("{relative}:{}", number + 1),
                detail: format!(
                    "`uses: {repository}@{revision}` does not match the lock: {remedy}"
                ),
            });
        }
    }
    problems
}

/// Whether a ref is a bare major alias like `v7`.
fn looks_like_a_series(revision: &str) -> bool {
    revision
        .strip_prefix('v')
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()))
}

/// The repository and revision on a `uses:` line, without a trailing comment.
fn uses_reference(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let rest = trimmed
        .strip_prefix("- uses: ")
        .or_else(|| trimmed.strip_prefix("uses: "))?;
    Some(
        rest.split('#')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('"')
            .trim()
            .to_owned(),
    )
}

/// Report what the tracked refs look like upstream, right now.
///
/// A workflow reading `@v7` runs whatever `v7` means today, so the lock's recorded
/// SHA is the only thing that distinguishes "unchanged" from "somebody moved a tag".
fn report_online(current: &PinLock) -> (Vec<Outdated>, Vec<Moved>) {
    let mut newer = Vec::new();
    let mut moved = Vec::new();
    for pin in current.pins() {
        // Two independent calls, deliberately not `let … else { continue }`: a
        // version where the tag listing failed would skip the moved check, and "the
        // check did not run" is indistinguishable from "it found nothing".
        if let Some(tags) = list_tags(&pin.repository)
            && let Some(available) = newer_series(&tags, &pin)
        {
            newer.push(Outdated {
                repository: pin.repository.clone(),
                tracked: pin.version.clone(),
                available,
            });
        }
        // A tag that no longer points where it did is a supply-chain event. A
        // *branch* that moved is a channel doing its job, so it is not a finding —
        // the lock still records where it points, which is the useful part.
        if let Some((commit, kind)) = resolve(&pin.repository, &pin.version)
            && !kind.moves_by_design()
            && commit != pin.sha
        {
            moved.push(Moved {
                repository: pin.repository,
                ref_name: pin.version,
                recorded: pin.sha,
                actual: commit,
            });
        }
    }
    (newer, moved)
}

/// The newest major series upstream publishes, when it is not the tracked one.
///
/// `None` for anything that is not a series: a channel like `stable` has no newer
/// self, and a fixed release like `v7.0.1` is as new as it will ever be.
fn newer_series(tags: &[String], pin: &ActionPin) -> Option<String> {
    let tracked = pin.series()?;
    tags.iter()
        .filter_map(|tag| tag.strip_prefix('v'))
        .filter(|rest| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()))
        .filter_map(|rest| rest.parse::<u64>().ok())
        .max()
        .filter(|latest| *latest > tracked)
        .map(|latest| format!("v{latest}"))
}

/// Advance every tracked series to the newest major upstream publishes, and
/// re-record the commit each ref resolves to. Fails rather than writing a
/// partial file.
pub fn refresh(root: &Path, added: &[String]) -> std::result::Result<Report, Error> {
    let mut current = lock();
    for repository in added {
        if current.actions.contains_key(repository) {
            continue;
        }
        let tags = list_tags(repository).ok_or_else(|| Error::Unreachable {
            repository: repository.clone(),
        })?;
        // A new entry gets the newest *series*: a workflow naming a fixed patch
        // release has opted out of dependabot, and zup's own workflows have not.
        let version = newest_series_tag(&tags)
            .or_else(|| newest_stable(&tags))
            .or_else(|| tags.first().cloned())
            .ok_or_else(|| Error::NoTags {
                repository: repository.clone(),
            })?;
        let sha = resolve_commit(repository, &version).ok_or_else(|| Error::Unreachable {
            repository: repository.clone(),
        })?;
        current.actions.insert(
            repository.clone(),
            zup_publish_github::LockedAction {
                version,
                sha,
                checked_at: today(),
            },
        );
    }

    let today = today();
    let mut resolved = BTreeMap::new();
    for (repository, action) in &current.actions {
        // A series moves forward; a channel is already the newest it can be, and a
        // fixed release was chosen on purpose.
        let version = {
            let tags = list_tags(repository).unwrap_or_default();
            let pin = ActionPin {
                repository: repository.clone(),
                version: action.version.clone(),
                sha: action.sha.clone(),
                checked_at: action.checked_at.clone(),
            };
            newer_series(&tags, &pin).unwrap_or_else(|| action.version.clone())
        };
        // A ref whose commit cannot be read is not written: a lock with a
        // plausible-looking SHA nobody resolved is worse than a failed refresh. It
        // also keeps `checkedAt` meaning "verified on" rather than "somebody ran a
        // command".
        let sha = resolve_commit(repository, &version).ok_or_else(|| Error::Unreachable {
            repository: repository.clone(),
        })?;
        resolved.insert(
            repository.clone(),
            zup_publish_github::LockedAction {
                version,
                sha,
                checked_at: today.clone(),
            },
        );
    }
    current.actions = resolved;

    let text = render(&current);
    let path = root.join(LOCK_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| Error::Io {
            path: parent.to_path_buf(),
            reason: error.to_string(),
        })?;
    }
    std::fs::write(&path, &text).map_err(|error| Error::Io {
        path: path.clone(),
        reason: error.to_string(),
    })?;
    Ok(check(root, false))
}

/// Render the lock with its `$comment` and a stable key order.
///
/// The comment is carried over from the file on disk rather than regenerated, so a
/// refresh does not delete the explanation the next person needs.
fn render(current: &PinLock) -> String {
    let existing: PinLock = serde_json::from_str(lock_json()).expect("the lock parses");
    let mut value = serde_json::to_value(current).expect("a lock serializes");
    if let Some(object) = value.as_object_mut()
        && let Some(comment) = existing.comment.clone()
    {
        object.insert("$comment".to_owned(), comment);
    }
    let mut text = serde_json::to_string_pretty(&value).expect("a lock serializes");
    text.push('\n');
    text
}

/// Today's date, for `checkedAt`.
///
/// A UTC date rather than a timestamp: a timestamp would make `refresh` produce a
/// different file every run.
fn today() -> String {
    // `SystemTime` cannot format a civil date, and `jiff` would be a dependency for
    // a string. This is Howard Hinnant's `civil_from_days`, the algorithm `date`
    // uses, and the only arithmetic in the file.
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0);
    let days = seconds.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

/// Every tag upstream publishes, newest last.
///
/// Takes a repository *name*, never a URL: `upstream` is the one place a URL is
/// built, and a caller that can also build one will eventually build two.
fn list_tags(repository: &str) -> Option<Vec<String>> {
    let output = Command::new("git")
        .args(["ls-remote", "--tags", "--refs", &upstream(repository)])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let mut tags: Vec<String> = text
        .lines()
        .filter_map(|line| line.split_once("refs/tags/"))
        .map(|(_, tag)| tag.trim().to_owned())
        .collect();
    tags.sort_by_key(|tag| tag_key(tag));
    tags.dedup();
    Some(tags)
}

/// Where a ref lives upstream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefKind {
    /// `refs/tags/<name>` — a release, or an alias that points at releases.
    Tag,
    /// `refs/heads/<name>` — a branch. `dtolnay/rust-toolchain@stable` is one.
    Branch,
}

impl RefKind {
    /// Whether this kind is supposed to move.
    ///
    /// A branch is a channel: `stable` pointing at a newer toolchain is the feature
    /// working. Reporting that weekly would train people to ignore the report, which
    /// is how the one finding that matters gets missed.
    fn moves_by_design(self) -> bool {
        matches!(self, Self::Branch)
    }
}

/// The commit a ref names, and where it lives.
fn resolve(repository: &str, ref_name: &str) -> Option<(String, RefKind)> {
    for (namespace, kind) in [
        ("refs/tags/", RefKind::Tag),
        ("refs/heads/", RefKind::Branch),
    ] {
        let output = Command::new("git")
            .args([
                "ls-remote",
                &upstream(repository),
                &format!("{namespace}{ref_name}"),
            ])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        if let Some(commit) = String::from_utf8(output.stdout)
            .ok()?
            .lines()
            .find_map(|line| line.split_whitespace().next().map(str::to_owned))
        {
            return Some((commit, kind));
        }
    }
    None
}

/// The commit a ref names, wherever it lives.
fn resolve_commit(repository: &str, ref_name: &str) -> Option<String> {
    resolve(repository, ref_name).map(|(commit, _)| commit)
}

/// The newest stable semver tag, `v`-prefixed.
///
/// `git ls-remote` output is in no particular order, so the list is sorted rather
/// than scanned: "the last one that parses" is not "the newest one". A prerelease
/// is not a candidate, because a pipeline that depends on `v2.0.0-rc.1` is a
/// pipeline whose next run resolves to a different binary.
fn newest_stable(tags: &[String]) -> Option<String> {
    let mut stable: Vec<&String> = tags.iter().filter(|tag| is_stable_version(tag)).collect();
    stable.sort_by_key(|tag| tag_key(tag));
    stable.last().map(|tag| (*tag).clone())
}

/// The newest bare major alias upstream publishes, e.g. `v7`.
///
/// Not a fixed patch release: a workflow naming `v7.0.1` has opted out of
/// dependabot, and this lock is for the workflows that have not.
fn newest_series_tag(tags: &[String]) -> Option<String> {
    let mut series: Vec<u64> = tags
        .iter()
        .filter_map(|tag| tag.strip_prefix('v'))
        .filter(|rest| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()))
        .filter_map(|rest| rest.parse().ok())
        .collect();
    series.sort_unstable();
    series.last().map(|latest| format!("v{latest}"))
}

/// Whether a tag is a concrete stable release.
fn is_stable_version(tag: &str) -> bool {
    let rest = tag.strip_prefix('v').unwrap_or(tag);
    semver::Version::parse(rest).is_ok_and(|version| version.pre.is_empty())
}

/// A total order over tag names, so "newest" means something without a parser.
///
/// A lexical sort would put `v10.0.0` before `v9.0.0`, which is the bug this
/// avoids.
fn tag_key(tag: &str) -> (u64, Vec<u64>, u8, String) {
    let rest = tag.strip_prefix('v').unwrap_or(tag);
    let (core, pre) = match rest.split_once('-') {
        Some((core, pre)) => (core, pre.to_owned()),
        None => (rest, String::new()),
    };
    let numbers: Vec<u64> = core
        .split('.')
        .map(|part| part.parse().unwrap_or(0))
        .collect();
    (
        numbers.first().copied().unwrap_or(0),
        numbers,
        // 0 for a pre-release, 1 for the release, so `2.0.0-rc.1` precedes
        // `2.0.0`.
        u8::from(pre.is_empty()),
        pre,
    )
}

/// Why a refresh could not finish.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Upstream could not be read.
    #[error("`{repository}` could not be read from GitHub; is the name right and the network up?")]
    Unreachable {
        /// The action that could not be resolved.
        repository: String,
    },
    /// The repository publishes no tags.
    #[error("`{repository}` publishes no tags")]
    NoTags {
        /// The action with no tags.
        repository: String,
    },
    /// The lock could not be written.
    #[error("{}: {reason}", path.display())]
    Io {
        /// The file that could not be written.
        path: PathBuf,
        /// Why.
        reason: String,
    },
}

/// Print a check report the way a terminal wants it.
pub fn render_report(report: &Report) -> String {
    let mut out = String::new();
    for problem in &report.problems {
        let _ = writeln!(out, "  {}  {}", problem.subject, problem.detail);
    }
    // A moved ref first, and loud: it is the one finding here that is not
    // housekeeping. Everything else on this list is a version the project may adopt.
    for moved in &report.moved {
        let _ = writeln!(
            out,
            "  {}@{}  MOVED: was {} on the last check, now {}",
            moved.repository, moved.ref_name, moved.recorded, moved.actual
        );
    }
    for outdated in &report.newer {
        let _ = writeln!(
            out,
            "  {}  tracks {}, upstream has {}",
            outdated.repository, outdated.tracked, outdated.available
        );
    }
    out
}

/// The lock's actions, for a report.
pub fn locked() -> &'static [ActionPin] {
    pins()
}

/// The actions only zup's own CI uses.
pub fn infrastructure() -> &'static [&'static str] {
    infrastructure_actions()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `v10` sorting before `v9` is a lexically-correct answer and a practically
    /// useless one, and a prerelease must land before the release it leads to or a
    /// pipeline resolves to `2.0.0-rc.1` the moment somebody tags one.
    #[test]
    fn tags_sort_numerically_and_prereleases_come_before_their_release() {
        let mut tags = vec![
            "v2.0.0".to_owned(),
            "v10.0.0".to_owned(),
            "v9.0.0".to_owned(),
            "v2.0.0-rc.1".to_owned(),
            "v2.1.0-rc.1".to_owned(),
        ];
        tags.sort_by_key(|tag| tag_key(tag));
        assert_eq!(
            tags,
            vec!["v2.0.0-rc.1", "v2.0.0", "v2.1.0-rc.1", "v9.0.0", "v10.0.0",],
            "{tags:?}"
        );
    }

    /// A pipeline that silently starts resolving to `2.0.0-rc.1` because somebody
    /// tagged a release candidate would ship a prerelease to everybody.
    #[test]
    fn a_prerelease_is_never_the_newest_stable() {
        for tags in [
            &["v1.0.0", "v2.0.0-beta.1", "v1.9.0"][..],
            &["v1.9.0", "v2.0.0-rc.1"][..],
        ] {
            assert_eq!(
                newest_stable(&tags_of(tags)).as_deref(),
                Some("v1.9.0"),
                "{tags:?}"
            );
        }
    }

    /// `newest_stable` reads the tag list, and that list arrives from the network
    /// in whatever order the API returned, so the answer must not depend on it.
    #[test]
    fn an_unordered_tag_list_finds_the_right_release() {
        let ordered = tags_of(&["v1.0.0", "v1.9.0", "v2.0.0-rc.1", "v10.0.0"]);
        let mut shuffled = ordered.clone();
        shuffled.reverse();
        assert_eq!(newest_stable(&ordered), newest_stable(&shuffled));
        assert_eq!(newest_stable(&ordered).as_deref(), Some("v10.0.0"));
    }

    /// A tag list, from string literals. Named apart from `tags` so a test's local
    /// `tags` binding does not shadow it.
    fn tags_of(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    /// A version that is not a *series* must not be advanced. A `stable` channel
    /// and a pinned release are both deliberate choices, and a check that rewrote
    /// either would make a correct lock look stale forever.
    #[test]
    fn a_ref_that_is_not_a_series_is_never_advanced() {
        for (repository, version) in [
            ("dtolnay/rust-toolchain", "stable"),
            ("example/action", "v7.0.1"),
        ] {
            let pin = ActionPin {
                repository: repository.to_owned(),
                version: version.to_owned(),
                sha: "0".repeat(40),
                checked_at: "2026-09-27".to_owned(),
            };
            assert!(!pin.is_series(), "{version}");
            assert_eq!(newer_series(&tags_of(&["v8", "v9", "v99"]), &pin), None);
        }
    }

    #[test]
    fn a_series_is_advanced_only_forward() {
        let pin = ActionPin {
            repository: "actions/checkout".to_owned(),
            version: "v7".to_owned(),
            sha: "0".repeat(40),
            checked_at: "2026-09-27".to_owned(),
        };
        let all = tags_of(&["v1", "v7", "v8", "v9", "v2.0.0", "v8.1.0"]);
        // `v9` is the newest *series*; `v8.1.0` is a release, not a series.
        assert_eq!(newer_series(&all, &pin).as_deref(), Some("v9"), "{all:?}");

        let ahead = ActionPin {
            version: "v10".to_owned(),
            ..pin.clone()
        };
        assert_eq!(
            newer_series(&all, &ahead),
            None,
            "nothing is newer than v10"
        );
    }

    #[test]
    fn the_newest_series_is_a_bare_alias() {
        // A new entry gets `v7`, never `v7.4.1`, so dependabot keeps advancing it.
        let all = tags_of(&["v1.0.0", "v7.0.1", "v7.4.1", "v8.0.0", "v8"]);
        assert_eq!(newest_series_tag(&all).as_deref(), Some("v8"));
    }

    /// A `uses:` line is what the lock is checked against, so a reference read out
    /// of one must be the whole reference — with a comment tolerated, because
    /// nothing writes one but somebody will add one, and a `run:` line must yield
    /// nothing rather than a misread.
    #[test]
    fn a_uses_line_is_read_without_its_comment() {
        assert_eq!(
            uses_reference("        uses: actions/checkout@v7").as_deref(),
            Some("actions/checkout@v7")
        );
        assert_eq!(
            uses_reference("  - uses: \"acme/zup@v1\"").as_deref(),
            Some("acme/zup@v1")
        );
        assert_eq!(
            uses_reference("        uses: actions/checkout@abc123 # v4.1.7").as_deref(),
            Some("actions/checkout@abc123")
        );
        assert_eq!(uses_reference("        run: zup build"), None);
    }

    /// A SHA is not a *different series*, and the report has to say so —
    /// otherwise the remedy names a ref the file obviously does not use.
    #[test]
    fn only_a_major_series_looks_like_a_series() {
        assert!(!looks_like_a_series(
            "3d3c42e5aac5ba805825da76410c181273ba90b1"
        ));
        assert!(looks_like_a_series("v7"));
        assert!(!looks_like_a_series("stable"));
    }
}
