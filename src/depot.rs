/*
 * Copyright (C) 2019 Josh Gao
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use std::path::{Path, PathBuf};

use failure::Error;
use failure::ResultExt;

use super::config;
use super::util;

#[derive(Clone, Debug)]
pub struct Depot {
  name: String,
  path: PathBuf,
}

impl Depot {
  pub fn new(name: String, path: PathBuf) -> Result<Depot, Error> {
    Ok(Depot { name, path })
  }

  fn open_or_create_bare_repo<T: AsRef<Path>>(path: T) -> Result<git2::Repository, Error> {
    let repo = match git2::Repository::open_bare(&path) {
      Ok(repo) => repo,
      Err(err) => git2::Repository::init_bare(&path).context("failed to create repository")?,
    };
    Ok(repo)
  }

  // Reimplementation of clone by hand, because libgit2 doesn't support `clone -l`.
  fn clone_alternates<T: AsRef<Path>>(src: T, dst: T, bare: bool) -> Result<git2::Repository, Error> {
    let src: &Path = src.as_ref();
    let dst: &Path = dst.as_ref();

    let repo = if bare {
      git2::Repository::init_bare(&dst)
    } else {
      git2::Repository::init(&dst)
    };
    let repo = repo.context(format!("failed to create repository at {:?}", dst))?;

    let git_path = if bare { dst.to_path_buf() } else { dst.join(".git") };

    // Set its alternates.
    let alternates_path = git_path.join("objects").join("info").join("alternates");
    let source_path = src.join("objects");
    let alternates_contents = source_path.to_str().unwrap().to_owned() + "\n";
    std::fs::write(&alternates_path, &alternates_contents)
      .context(format!("failed to set alternates for new repository {:?}", dst))?;

    Ok(repo)
  }

  /// Get the path of the git directory given a path to a bare or non-bare repository.
  fn git_path<T: AsRef<Path>>(path: T) -> PathBuf {
    let path: &Path = path.as_ref();
    let nonbare = path.join(".git");
    if nonbare.exists() {
      nonbare
    } else {
      path.to_path_buf()
    }
  }

  fn replace_dir<T: AsRef<Path>>(src: T, dst: T) -> Result<(), Error> {
    let src: &Path = src.as_ref();
    let dst: &Path = dst.as_ref();

    ensure!(
      src.exists(),
      "attempted to replace {:?} with nonexistent directory {:?}",
      dst,
      src
    );

    if dst.exists() {
      std::fs::remove_dir_all(&dst).context(format!("failed to delete {:?}", dst))?;
    }

    std::fs::create_dir_all(&dst).context(format!("failed to create directory {:?}", dst))?;

    let entries = std::fs::read_dir(&src).context(format!("failed to read directory {:?}", src))?;

    for entry in entries {
      let entry = entry?;
      std::fs::copy(entry.path(), dst.join(entry.file_name())).context(format!(
        "failed to copy {:?} to {:?}",
        entry.path(),
        dst
      ))?;
    }

    Ok(())
  }

  pub fn objects_mirror<T: Into<String>>(&self, project: T) -> PathBuf {
    let repo_name: String = project.into() + ".git";
    self.path.join("objects").join(repo_name)
  }

  pub fn refs_mirror<T: AsRef<str>, U: Into<String>>(&self, remote: T, project: U) -> PathBuf {
    let remote: &str = remote.as_ref();
    let repo_name: String = project.into() + ".git";
    self.path.join("refs").join(remote).join(repo_name)
  }

  pub fn fetch_repo(
    &self,
    remote_config: &config::RemoteConfig,
    project: &str,
    branch: &str,
    depth: Option<i32>,
    progress: Option<&indicatif::ProgressBar>,
  ) -> Result<(), Error> {
    ensure!(!project.starts_with('/'), "invalid project path {}", project);
    ensure!(!project.ends_with('/'), "invalid project path {}", project);

    // TODO: Add locking?
    let objects_path = self.objects_mirror(project);
    let repo_url = remote_config.url.to_owned() + project + ".git";

    let objects_repo = Depot::open_or_create_bare_repo(&objects_path)?;
    let mut remote = match objects_repo.find_remote(&remote_config.name) {
      Ok(remote) => {
        objects_repo.remote_set_url(&remote_config.name, &repo_url)?;
        objects_repo.find_remote(&remote_config.name).unwrap()
      }
      Err(err) => objects_repo
        .remote(&remote_config.name, &repo_url)
        .context("failed to create remote")?,
    };

    // Use libgit2 when we can, because it's significantly faster than shelling out to git.
    let parsed_url = url::Url::parse(&repo_url)?;
    let scheme = parsed_url.scheme();
    let scheme_supported = scheme == "git" || scheme == "https" || scheme == "http" || scheme == "ssh" || scheme == "";
    let use_git2 = scheme_supported && depth.is_none();

    if use_git2 {
      let mut fetch_opts = git2::FetchOptions::new();
      fetch_opts
        .prune(git2::FetchPrune::Off)
        .update_fetchhead(true)
        .download_tags(git2::AutotagOption::None);

      remote
        .fetch(&[branch], Some(&mut fetch_opts), None)
        .context("failed to fetch")?;
    } else {
      let mut cmd = std::process::Command::new("git");
      cmd
        .arg("-C")
        .arg(&objects_path)
        .arg("fetch")
        .arg(&remote_config.name)
        .arg(&branch)
        .arg("--no-tags");

      if let Some(depth) = depth {
        cmd.arg("--depth");
        cmd.arg(depth.to_string());
      }

      let git_output = cmd.output().context("failed to spawn git fetch")?;
      if !git_output.status.success() {
        bail!("git fetch failed: {}", String::from_utf8_lossy(&git_output.stderr));
      }
    }

    let refs_path = self.refs_mirror(&remote_config.name, project);
    let refs_repo = match git2::Repository::open(&refs_path) {
      Ok(repo) => repo,
      Err(err) => Depot::clone_alternates(&objects_path, &refs_path, true)?,
    };

    let objects_refs = objects_path.join("refs").join("remotes").join(&remote_config.name);
    let refs_refs = refs_path.join("refs").join("heads");
    Depot::replace_dir(&objects_refs, &refs_refs)?;

    Ok(())
  }

  pub fn clone_repo<T: AsRef<Path>>(
    &self,
    remote_config: &config::RemoteConfig,
    project: &str,
    branch: &str,
    path: T,
  ) -> Result<(), Error> {
    let path: &Path = path.as_ref();

    let repo = Depot::clone_alternates(self.objects_mirror(project), path.to_path_buf(), false)?;
    repo
      .remote(
        &remote_config.name,
        self.refs_mirror(&remote_config.name, project).to_str().unwrap(),
      )
      .context("failed to create remote")?;
    repo
      .remote_set_pushurl(&remote_config.name, Some(&format!("{}{}", remote_config.url, project)))
      .context("failed to set remote pushurl")?;

    self.update_remote_refs(&remote_config, project, &path)?;

    let head = util::parse_revision(&repo, &remote_config.name, &branch)?;
    repo
      .checkout_tree(&head, None)
      .context(format!("failed to checkout HEAD at {:?}", repo.path()))?;
    repo
      .set_head_detached(head.id())
      .context(format!("failed to set HEAD to {:?}", repo.path()))?;
    Ok(())
  }

  pub fn update_remote_refs<T: AsRef<Path>>(
    &self,
    remote_config: &config::RemoteConfig,
    project: &str,
    path: T,
  ) -> Result<(), Error> {
    let path: &Path = path.as_ref();

    let mirror_refs = self
      .refs_mirror(&remote_config.name, project)
      .join("refs")
      .join("heads");
    let repo_refs = Depot::git_path(path)
      .join("refs")
      .join("remotes")
      .join(&remote_config.name);

    Depot::replace_dir(&mirror_refs, &repo_refs)
  }
}
