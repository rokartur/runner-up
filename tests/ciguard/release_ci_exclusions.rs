use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use serde_yaml::{Mapping, Value};

use crate::{RELEASE_WORKFLOW_PATH, REQUIRED_CHECK_CONTEXTS};

/// The part of release-please-config.json that names the files a release PR
/// touches.
#[derive(Deserialize)]
struct ReleasePleaseConfig {
    #[serde(default)]
    packages: BTreeMap<String, ReleasePleasePackage>,
}

#[derive(Deserialize)]
struct ReleasePleasePackage {
    #[serde(rename = "release-type", default)]
    release_type: String,
    #[serde(rename = "extra-files", default)]
    extra_files: Vec<String>,
}

fn read_release_please_config() -> ReleasePleaseConfig {
    let data = std::fs::read_to_string("release-please-config.json").expect("read release-please-config.json");
    serde_json::from_str(&data).expect("parse release-please-config.json")
}

/// Every path release-please writes for this repository, sorted. It is the
/// source of truth for pull_request path filters: every PR-triggered workflow
/// must exclude every path here, or release PRs start creating
/// action_required runs again.
fn expected_release_outputs(cfg: &ReleasePleaseConfig) -> Result<Vec<String>, String> {
    if cfg.packages.is_empty() {
        return Err("release-please-config.json has no packages".into());
    }
    let mut seen = BTreeSet::from([".release-please-manifest.json".to_string()]);
    for (pkg_path, pkg) in &cfg.packages {
        let at_root = |file: &str| if pkg_path == "." || pkg_path.is_empty() { file.to_string() } else { format!("{pkg_path}/{file}") };
        match pkg.release_type.as_str() {
            "" => return Err(format!("package {pkg_path:?} missing release-type")),
            "simple" => {
                seen.insert(at_root("version.txt"));
            }
            "node" => {
                seen.insert(at_root("package.json"));
                // pnpm repositories keep a root pnpm-lock.yaml that release-please does not write.
                let lock = at_root("package-lock.json");
                if std::path::Path::new(&lock).exists() {
                    seen.insert(lock);
                }
            }
            // Unknown types still produce a CHANGELOG at the package root.
            _ => {}
        }
        seen.insert(at_root("CHANGELOG.md"));
        for extra in pkg.extra_files.iter().filter(|e| !e.is_empty()) {
            seen.insert(extra.replace('\\', "/"));
        }
    }
    Ok(seen.into_iter().collect())
}

/// A workflow's `on:` map. serde_yaml reads YAML 1.2, where a bare `on` key is
/// the string "on"; a YAML 1.1 reader would make it boolean true.
pub fn parse_workflow_on(data: &str) -> Result<Mapping, String> {
    let raw: Mapping = serde_yaml::from_str(data).map_err(|e| e.to_string())?;
    for key in [Value::from("on"), Value::Bool(true)] {
        if let Some(on) = raw.get(&key) {
            return on.as_mapping().cloned().ok_or_else(|| format!("unexpected on: value {on:?}"));
        }
    }
    Err("workflow has no on: trigger map".into())
}

/// How a pull_request trigger filters by path. Allow patterns starting with
/// '!' are negative.
#[derive(Debug, PartialEq)]
pub enum PathFilter {
    None,
    Ignore(Vec<String>),
    Allow(Vec<String>),
}

/// The pull_request trigger's path filter, or None when the workflow has no
/// pull_request trigger.
pub fn pull_request_path_filter(on: &Mapping) -> Result<Option<PathFilter>, String> {
    let Some(pr) = on.get("pull_request") else { return Ok(None) };
    // A bare `pull_request:` or the list form `pull_request: [opened]` filters no path.
    let Some(pr) = pr.as_mapping() else { return Ok(Some(PathFilter::None)) };
    let ignore = string_list_field(pr, "paths-ignore")?;
    let allow = string_list_field(pr, "paths")?;
    match (ignore, allow) {
        (Some(_), Some(_)) => Err("pull_request sets both paths and paths-ignore".into()),
        (Some(ignore), None) => Ok(Some(PathFilter::Ignore(ignore))),
        (None, Some(allow)) => Ok(Some(PathFilter::Allow(allow))),
        (None, None) => Ok(Some(PathFilter::None)),
    }
}

fn string_list_field(m: &Mapping, key: &str) -> Result<Option<Vec<String>>, String> {
    let Some(v) = m.get(key).filter(|v| !v.is_null()) else { return Ok(None) };
    let Some(items) = v.as_sequence() else { return Err(format!("{key} is {v:?}, want list")) };
    let mut out = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let Some(s) = item.as_str() else { return Err(format!("{key}[{i}] is {item:?}, want string")) };
        out.push(s.to_string());
    }
    Ok(Some(out))
}

/// Whether the filter keeps path from starting a run, following GitHub's
/// paths / paths-ignore rules for exact file paths: in an allow list a later
/// negative pattern re-excludes a path an earlier positive one matched.
fn path_excluded(filter: &PathFilter, path: &str) -> bool {
    match filter {
        PathFilter::None => false,
        PathFilter::Ignore(patterns) => patterns.iter().any(|p| match_github_path(p, path)),
        PathFilter::Allow(patterns) => {
            let mut matched = false;
            for pattern in patterns {
                match pattern.strip_prefix('!') {
                    Some(negative) => matched = matched && !match_github_path(negative, path),
                    None => matched = matched || match_github_path(pattern, path),
                }
            }
            !matched
        }
    }
}

/// A matcher for the patterns this repository uses: exact paths, trailing
/// /**, **/name, **/*.ext and *.ext.
fn match_github_path(pattern: &str, path: &str) -> bool {
    let pattern = pattern.replace('\\', "/");
    let path = path.replace('\\', "/");
    if pattern == path {
        return true;
    }
    if let Some(dir) = pattern.strip_suffix("/**")
        && (path == dir || path.starts_with(&format!("{dir}/")))
    {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix("**/") {
        if path == suffix || path.ends_with(&format!("/{suffix}")) {
            return true;
        }
        if let Some(ext) = suffix.strip_prefix('*')
            && ext.starts_with('.')
            && path.ends_with(ext)
        {
            return true;
        }
    }
    match pattern.strip_prefix('*') {
        Some(ext) if ext.starts_with('.') => path.ends_with(ext),
        _ => false,
    }
}

/// Whether a workflow backs a required status check on main, known by its job
/// names since a required check's context is the job name GitHub reports. Such
/// a workflow must not filter by path: a required check that never runs never
/// reports, and the pull request waits on it forever. Release PRs get every
/// required context stamped by release.yml instead.
fn publishes_required_check(data: &str) -> Result<bool, String> {
    #[derive(Deserialize)]
    struct Job {
        #[serde(default)]
        name: String,
    }
    #[derive(Deserialize)]
    struct Workflow {
        #[serde(default)]
        jobs: BTreeMap<String, Job>,
    }
    let wf: Workflow = serde_yaml::from_str(data).map_err(|e| e.to_string())?;
    Ok(wf.jobs.values().any(|job| REQUIRED_CHECK_CONTEXTS.contains(&job.name.as_str())))
}

// Intent: a release-please PR is opened with GITHUB_TOKEN, so a PR-triggered
// workflow that does not exclude every file release-please writes leaves an
// action_required run on it. The required-check workflows are the exception:
// they must never filter by path, and exactly two of them exist.
#[test]
fn pull_request_workflows_exclude_release_please_outputs() {
    let expected = expected_release_outputs(&read_release_please_config()).expect("derive release outputs");
    assert!(!expected.is_empty(), "derived empty release output set");

    let mut paths: Vec<_> = std::fs::read_dir(".github/workflows")
        .expect("read .github/workflows")
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file() && matches!(p.extension().and_then(|e| e.to_str()), Some("yml" | "yaml")))
        .collect();
    paths.sort();

    let (mut pr_workflows, mut required_check_workflows) = (0, 0);
    let mut errors = Vec::new();
    for path in paths {
        let path = path.display().to_string();
        let data = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let on = parse_workflow_on(&data).unwrap_or_else(|e| panic!("parse on: in {path}: {e}"));
        let Some(filter) = pull_request_path_filter(&on).unwrap_or_else(|e| panic!("{path}: {e}")) else { continue };
        if publishes_required_check(&data).unwrap_or_else(|e| panic!("parse jobs in {path}: {e}")) {
            required_check_workflows += 1;
            if filter != PathFilter::None {
                errors.push(format!("{path} backs a required check and must not filter by path (a filtered required check never reports)"));
            }
            continue;
        }
        pr_workflows += 1;
        let missing: Vec<_> = expected.iter().filter(|rel| !path_excluded(&filter, rel)).cloned().collect();
        if !missing.is_empty() {
            errors.push(format!("{path} pull_request filter must exclude every release-please output; missing: {}", missing.join(", ")));
        }
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
    assert!(pr_workflows > 0, "no pull_request-triggered workflows found under .github/workflows");
    // Another one means a new workflow claimed a required-check name, which needs a look.
    assert_eq!(
        required_check_workflows,
        REQUIRED_CHECK_CONTEXTS.len(),
        "pull_request workflows backing the required checks {REQUIRED_CHECK_CONTEXTS:?}"
    );
}

// The derivation must surface every extra file of this repository's config, so
// a future extra-files addition cannot silently drop out of the guard above.
#[test]
fn expected_release_outputs_include_configured_extra_files() {
    let got = expected_release_outputs(&read_release_please_config()).unwrap();
    assert_eq!(got, [".release-please-manifest.json", "CHANGELOG.md", "default.nix", "flake.nix", "package.nix"]);
}

// Intent (README, "Updating runner-up": whoever holds the release signing key
// controls every Windows machine): the release workflow holds the release
// signing key, so it must use third-party actions pinned by commit, never by a
// movable tag or branch.
#[test]
fn privileged_workflows_pin_actions_by_commit() {
    let data = std::fs::read_to_string(RELEASE_WORKFLOW_PATH).unwrap();
    let mut errors = Vec::new();
    for (i, line) in data.lines().enumerate() {
        let line = line.trim();
        let Some(uses) = line.strip_prefix("- ").unwrap_or(line).trim().strip_prefix("uses: ") else { continue };
        let reference = uses.split_whitespace().next().unwrap_or("");
        if !pinned_by_commit(reference) {
            errors.push(format!("{RELEASE_WORKFLOW_PATH}:{} uses {reference:?}; pin it to a full commit id", i + 1));
        }
    }
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

fn pinned_by_commit(reference: &str) -> bool {
    let Some((action, commit)) = reference.split_once('@') else { return false };
    !action.is_empty()
        && !commit.contains('@')
        && commit.len() == 40
        && commit.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// The two matchers above decide what the guard accepts, so each must reject
// what it should: an unpinned ref, and a path a filter lets through.
#[test]
fn guard_matchers_reject_what_they_should() {
    assert!(pinned_by_commit("actions/checkout@11d5960a326750d5838078e36cf38b85af677262"));
    for bad in
        ["actions/checkout@v4", "actions/checkout@main", "actions/checkout", "actions/checkout@11D5960A326750D5838078E36CF38B85AF677262"]
    {
        assert!(!pinned_by_commit(bad), "{bad} counted as pinned");
    }
    let ignore = PathFilter::Ignore(vec!["docs/**".into(), "**/*.md".into(), "flake.nix".into()]);
    assert!(path_excluded(&ignore, "CHANGELOG.md"));
    assert!(path_excluded(&ignore, "docs/install.sh"));
    assert!(path_excluded(&ignore, "flake.nix"));
    assert!(!path_excluded(&ignore, "package.nix"));
    let allow = PathFilter::Allow(vec!["src/**".into(), "!src/release.rs".into()]);
    assert!(!path_excluded(&allow, "src/main.rs"));
    assert!(path_excluded(&allow, "src/release.rs"));
    assert!(path_excluded(&allow, "flake.nix"));
}
