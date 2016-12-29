use config::{Git as GitConfig, Repo as RepoConfig};
use errors::*;
use git2::{Branch, BranchType, Commit, Cred, FetchOptions, FetchPrune, ObjectType, PushOptions,
           RemoteCallbacks, Repository, ResetType};
use gitlab::{self, AccessLevel, Member, MergeRequestStateFilter, NamespaceId, ObjectId, UserId};
use gitlab_ext::GitlabExt;
use merge_request::MergeRequest;
use slog::Logger;
use std::path::{Path, PathBuf};

pub struct Project<'a> {
    log: Logger,
    gitlab: &'a GitlabExt,
    project: gitlab::Project,
    repository: Repository,
    _repo_config: &'a RepoConfig,
    git_config: &'a GitConfig,
    members: Vec<Member>,
}

impl<'a> Project<'a> {
    pub fn new(log: &Logger,
               label: &str,
               repo_config: &'a RepoConfig,
               git_config: &'a GitConfig,
               gitlab: &'a GitlabExt)
               -> Result<Self> {
        let log = log.new(o!("project" => label.to_string()));

        let project = gitlab.gitlab().project_by_name(&repo_config.name)?;
        let repository = open_repository(&project)?;

        let mut members = gitlab.gitlab().project_members(project.id)?;
        if let NamespaceId::Group(groupid) = project.namespace.owner_id() {
            members.extend(gitlab.gitlab().group_members(groupid)?);
        }

        info!(log, "start project";
              "id" => project.id.value(),
              "path" => project.path_with_namespace);

        let project = Project {
            log: log,
            gitlab: gitlab,
            project: project,
            repository: repository,
            _repo_config: repo_config,
            git_config: git_config,
            members: members,
        };

        Ok(project)
    }

    pub fn log(&self) -> &Logger {
        &self.log
    }

    pub fn gitlab(&self) -> &GitlabExt {
        self.gitlab
    }

    pub fn project(&self) -> &gitlab::Project {
        &self.project
    }

    pub fn repository(&self) -> &Repository {
        &self.repository
    }

    pub fn repository_fetch_branch(&'a self,
                                   remote_name: &str,
                                   branch_name: &str)
                                   -> Result<BranchInfo<'a>> {
        let mut remote = self.repository.find_remote(remote_name)?;
        let mut cb = RemoteCallbacks::new();
        let _ = cb.credentials(|_, _, _| {
                Cred::ssh_key("git", None, Path::new(&self.git_config.ssh_key), None)
            })
            .sideband_progress(|data| {
                debug!(self.log, "fetch: receive progress";
                       "data" => String::from_utf8_lossy(data).to_string());
                true
            });

        let mut fo = FetchOptions::new();
        let _ = fo.remote_callbacks(cb)
            .prune(FetchPrune::On);

        remote.fetch(&[branch_name], Some(&mut fo), None)?;

        let branch = self.repository
            .find_branch(&format!("{}/{}", remote_name, branch_name),
                         BranchType::Remote)?;
        let commit = self.repository.find_commit(branch.get().target().unwrap())?;

        Ok(BranchInfo {
            branch: branch,
            commit: commit,
        })
    }

    pub fn repository_push_branch(&self, remote_name: &str, refspec: &str) -> Result<()> {
        let mut remote = self.repository.find_remote(remote_name)?;
        let mut cb = RemoteCallbacks::new();
        let _ = cb.credentials(|_, _, _| {
                Cred::ssh_key("git", None, Path::new(&self.git_config.ssh_key), None)
            })
            .sideband_progress(|data| {
                debug!(self.log, "push: receive progress";
                       "data" => String::from_utf8_lossy(data).to_string());
                true
            });

        let mut po = PushOptions::new();
        let _ = po.remote_callbacks(cb);

        remote.push(&[refspec], Some(&mut po))?;

        Ok(())
    }

    pub fn repository_reset_branch(&self, branch: &Branch) -> Result<()> {
        let refname = branch.get().name().unwrap();
        self.repository.set_head(refname)?;
        self.repository
            .reset(&branch.get().peel(ObjectType::Any)?, ResetType::Hard, None)?;
        Ok(())
    }

    pub fn opened_merge_requests(&'a self) -> Result<impl Iterator<Item = MergeRequest<'a>> + 'a> {
        Ok(self.gitlab
            .gitlab()
            .merge_requests_with_state(self.project.id, MergeRequestStateFilter::Opened)?
            .into_iter()
            .map(move |mr| MergeRequest::from_gitlab_mr(self, mr)))
    }

    pub fn is_reviewer(&self, id: UserId) -> bool {
        self.members
            .iter()
            .find(|member| member.id == id)
            .map_or(false,
                    |member| member.access_level >= AccessLevel::Master.into())
    }
}

pub struct BranchInfo<'repo> {
    pub branch: Branch<'repo>,
    pub commit: Commit<'repo>,
}

impl<'repo> BranchInfo<'repo> {
    pub fn gitlab_object_id(&self) -> ObjectId {
        ObjectId::new(self.commit.id())
    }
}

fn open_repository(project: &gitlab::Project) -> Result<Repository> {
    let mut path = PathBuf::from("cache");
    path.push(&project.path_with_namespace);

    let repo = if !path.exists() {
        let repo = Repository::init(&path)?;
        let _ = repo.remote("origin", &project.ssh_url_to_repo)?;
        let _ = repo.remote("mr", "https://example.com/")?; // Dummy URL
        repo
    } else {
        Repository::open(&path)?
    };

    Ok(repo)
}
