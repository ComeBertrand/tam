//! The repositories `tam new` creates worktrees in, as named on the command
//! line: a path to (a directory in) the repository, or its project name as
//! `tam ls` and `tam pick` show it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tam_worktree::pretty::PrettyEntry;

/// The main checkouts of the repositories `args` name, in order; without
/// any, the repository containing `cwd`.
///
/// An argument is a path (relative to `cwd`) when it exists: the repository
/// containing it (a worktree stands for its repository). Else it is a
/// project name, exactly as `tam ls --porcelain` prints it, among the
/// projects `discover` finds. `discover` runs only then, at most once.
pub fn resolve(
    args: &[String],
    cwd: &Path,
    discover: impl FnOnce() -> Result<Vec<PathBuf>>,
) -> Result<Vec<PathBuf>> {
    if args.is_empty() {
        let root = main_checkout(cwd).map_err(|_| {
            anyhow::anyhow!(
                "not inside a git repository: run this from within a project, or name \
                 repositories with --repo"
            )
        })?;
        return Ok(vec![root]);
    }
    let mut projects = Projects {
        discover: Some(discover),
        entries: None,
    };
    let mut roots: Vec<(&str, PathBuf)> = Vec::new();
    for arg in args {
        let path = cwd.join(arg);
        let dir = if path.exists() {
            path
        } else {
            projects.find(arg)?
        };
        let root = main_checkout(&dir).with_context(|| format!("'{arg}'"))?;
        if let Some((other, _)) = roots.iter().find(|(_, r)| *r == root) {
            bail!(
                "'{other}' and '{arg}' are the same repository ({})",
                root.display()
            );
        }
        roots.push((arg, root));
    }
    Ok(roots.into_iter().map(|(_, root)| root).collect())
}

/// The projects `tam ls` and `tam pick` show: git checkouts under the home
/// directory, as the worktree config's discovery settings find them.
pub fn discover(config: &tam_worktree::config::Config) -> Result<Vec<PathBuf>> {
    let root = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let ignore = tam_worktree::discovery::build_ignore_set(&config.ignore)?;
    tam_worktree::discovery::discover(&root, &ignore, config.max_depth)
}

/// The environment the daemon runs git with when it creates our worktrees
/// (`CreateMembers`): ours, so that hooks and filters (git-lfs) see our
/// `PATH` and credentials, without the variables that tie git to one
/// repository (`git rev-parse --local-env-vars`). Variables that are not
/// UTF-8 are left out.
pub fn git_env() -> BTreeMap<String, String> {
    without_local_git_vars(
        std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))),
    )
}

fn without_local_git_vars(
    vars: impl Iterator<Item = (String, String)>,
) -> BTreeMap<String, String> {
    const LOCAL: &[&str] = &[
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_OBJECT_DIRECTORY",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_GRAFT_FILE",
        "GIT_INDEX_FILE",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_REPLACE_REF_BASE",
        "GIT_PREFIX",
        "GIT_SHALLOW_FILE",
        "GIT_COMMON_DIR",
    ];
    vars.filter(|(k, _)| !LOCAL.contains(&k.as_str())).collect()
}

/// The members to create for task `name` (`CreateMembers`): a worktree on
/// branch `name` in each repository (main checkouts), where the worktree
/// config puts it. Each repository is fetched first: the daemon does not
/// fetch.
pub fn new_members(
    roots: &[PathBuf],
    name: &str,
    config: &tam_worktree::config::Config,
) -> Result<Vec<tam_proto::NewMember>> {
    for root in roots {
        tam_worktree::git::fetch(root)?;
    }
    Ok(roots
        .iter()
        .map(|root| tam_proto::NewMember {
            repo_root: root.clone(),
            branch: name.to_string(),
            dir: tam_worktree::worktree::target_dir(config, root, name),
        })
        .collect())
}

/// The main checkout of the repository containing `dir`, resolved.
pub fn main_checkout(dir: &Path) -> Result<PathBuf> {
    let root = tam_worktree::git::repo_root(dir)
        .with_context(|| format!("{} is not in a git repository", dir.display()))?;
    root.canonicalize()
        .with_context(|| format!("cannot resolve {}", root.display()))
}

/// Discovered projects, found when first needed.
struct Projects<F> {
    discover: Option<F>,
    entries: Option<Vec<PrettyEntry>>,
}

impl<F: FnOnce() -> Result<Vec<PathBuf>>> Projects<F> {
    /// The project named `name` (its pretty name).
    fn find(&mut self, name: &str) -> Result<PathBuf> {
        if self.entries.is_none() {
            let discover = self.discover.take().expect("discovered once");
            let paths = discover().context("cannot list the projects")?;
            self.entries = Some(tam_worktree::pretty::build_pretty_names(&paths));
        }
        let entries = self.entries.as_deref().unwrap_or_default();
        if let Some(entry) = entries.iter().find(|e| e.display_name == name) {
            return Ok(entry.path.clone());
        }
        // A short name shared by several projects: say what they are called.
        let near: Vec<String> = entries
            .iter()
            .filter(|e| e.base_name == name)
            .map(|e| format!("'{}'", e.display_name))
            .collect();
        if near.is_empty() {
            bail!("no directory or project named '{name}' (tam ls lists the projects)");
        }
        bail!(
            "no project is named '{name}' exactly: did you mean {}?",
            near.join(" or ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    }

    fn repo(path: &Path) -> PathBuf {
        std::fs::create_dir_all(path).unwrap();
        git(path, &["init", "-q", "-b", "main"]);
        git(
            path,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        path.canonicalize().unwrap()
    }

    fn args(names: &[&str]) -> Vec<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    fn never() -> Result<Vec<PathBuf>> {
        panic!("no discovery for paths")
    }

    #[test]
    fn paths_name_their_repository() {
        let tmp = TempDir::new().unwrap();
        let api = repo(&tmp.path().join("src/api"));
        let web = repo(&tmp.path().join("src/web"));
        std::fs::create_dir_all(api.join("lib")).unwrap();
        let wt = tmp.path().join("api--feat");
        git(
            &api,
            &["worktree", "add", "-q", "-b", "feat", wt.to_str().unwrap()],
        );

        // Relative to the current directory, from a subdirectory, from a
        // worktree.
        let roots = resolve(&args(&["api/lib", "web"]), &tmp.path().join("src"), never).unwrap();
        assert_eq!(roots, [api.clone(), web]);
        let roots = resolve(&args(&[wt.to_str().unwrap()]), tmp.path(), never).unwrap();
        assert_eq!(roots, std::slice::from_ref(&api));
        // Without arguments, the current one.
        let roots = resolve(&[], &api.join("lib"), never).unwrap();
        assert_eq!(roots, std::slice::from_ref(&api));

        let err = resolve(&[], tmp.path(), never).unwrap_err().to_string();
        assert!(err.contains("--repo"), "{err}");
        let err = format!(
            "{:#}",
            resolve(&args(&["api", "api/lib"]), &tmp.path().join("src"), never).unwrap_err()
        );
        assert!(err.contains("same repository"), "{err}");
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let err = format!(
            "{:#}",
            resolve(&args(&["plain"]), tmp.path(), never).unwrap_err()
        );
        assert!(err.contains("not in a git repository"), "{err}");
    }

    #[test]
    fn git_runs_in_our_environment_but_not_tied_to_a_repository() {
        let vars = [
            ("PATH", "/bin"),
            ("GIT_DIR", "/x/.git"),
            ("GIT_SSH_COMMAND", "ssh"),
        ];
        let env = without_local_git_vars(vars.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        let keys: Vec<&str> = env.keys().map(String::as_str).collect();
        assert_eq!(keys, ["GIT_SSH_COMMAND", "PATH"]);
    }

    #[test]
    fn names_are_the_ones_tam_ls_shows() {
        let tmp = TempDir::new().unwrap();
        let api = repo(&tmp.path().join("src/api"));
        let mnemo_a = repo(&tmp.path().join("work/mnemo"));
        let mnemo_b = repo(&tmp.path().join("perso/mnemo"));
        let projects = vec![api.clone(), mnemo_a.clone(), mnemo_b.clone()];
        let cwd = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&cwd).unwrap();

        let discovered = || Ok(projects.clone());
        let roots = resolve(&args(&["api", "mnemo (work)"]), &cwd, discovered).unwrap();
        assert_eq!(roots, [api, mnemo_a]);

        // Discovered once for all the names.
        let mut calls = 0;
        let counting = || {
            calls += 1;
            Ok(projects.clone())
        };
        resolve(&args(&["mnemo (perso)", "mnemo (work)"]), &cwd, counting).unwrap();
        assert_eq!(calls, 1);

        let err = resolve(&args(&["mnemo"]), &cwd, || Ok(projects.clone()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("did you mean"), "{err}");
        assert!(
            err.contains("'mnemo (perso)'") && err.contains("'mnemo (work)'"),
            "{err}"
        );
        let err = resolve(&args(&["nope"]), &cwd, || Ok(projects.clone()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no directory or project named 'nope'"),
            "{err}"
        );
    }
}
