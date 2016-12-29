use build_state::{Approval as ApprovalState, ApprovalInfo as ApprovalStateInfo,
                  ApprovalKind as ApprovalStateKind, State as BuildState, Test as TestState,
                  TestInfo as TestStateInfo, TestKind as TestStateKind};
use errors::*;
use git2::{STATUS_CONFLICTED, Signature};
use git2::build::CheckoutBuilder;
use gitlab::{self, CommitNote, CommitStatus, MergeStatus, ObjectId, ProjectId, StatusState,
             UserFull};
use gitlab_ext::GitlabExt;
use project::{BranchInfo, Project};
use slog::{self, Logger};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt::Debug;

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum State {
    Init,
    Approved(ApprovalStateInfo),
    Running(ApprovalStateInfo),
    Success(ApprovalStateInfo),
    Merged(ApprovalStateInfo),
    Failed(Option<ApprovalStateInfo>),
    Errored,
}

impl State {
    fn as_str(&self) -> &'static str {
        match *self {
            State::Init => "init",
            State::Approved { .. } => "approved",
            State::Running { .. } => "running",
            State::Success { .. } => "success",
            State::Merged { .. } => "merged",
            State::Failed { .. } => "failed",
            State::Errored => "errored",
        }
    }
}

impl slog::Serialize for State {
    fn serialize(&self,
                 _record: &slog::Record,
                 key: &'static str,
                 serializer: &mut slog::Serializer)
                 -> slog::ser::Result {
        match *self {
            State::Init | State::Errored => serializer.emit_str(key, self.as_str()),
            State::Approved(ref approval) |
            State::Running(ref approval) |
            State::Success(ref approval) |
            State::Merged(ref approval) |
            State::Failed(Some(ref approval)) => {
                serializer.emit_arguments(key,
                                          &format_args!("{}(p={},date={},approved_by={})",
                                                        self.as_str(),
                                                        approval.priority,
                                                        approval.time,
                                                        approval.username))
            }
            State::Failed(None) => {
                serializer.emit_arguments(key, &format_args!("{}(None)", self.as_str()))
            }
        }
    }
}

pub struct MergeRequest<'a> {
    log: Logger,
    project: &'a Project<'a>,
    merge_request: gitlab::MergeRequest,
    state: State,
    approval_state: ApprovalState,
    test_state: TestState,
    merged: bool,
    pipeline_state: HashMap<String, CommitStatus>,
}

impl<'a> MergeRequest<'a> {
    pub fn from_gitlab_mr(project: &'a Project, mr: gitlab::MergeRequest) -> Self {
        let log = project.log().new(o!("merge_request" => mr.id.value()));
        debug!(log, "start merge_request";
               "source_project" => mr.source_project_id.value(),
               "source_branch" => mr.source_branch.to_string(),
               "target_branch" => mr.target_branch.to_string(),
               "merge_status" => mr.merge_status.as_str());

        assert_eq!(project.project().id, mr.target_project_id);

        let gitlab = project.gitlab();

        let (mut result, pipeline_state) = match last_pipeline_statuses(gitlab,
                                                                        mr.source_project_id,
                                                                        &mr.source_branch,
                                                                        mr.sha.value()) {
            Ok(statuses) => (Ok(()), statuses),
            Err(e) => {
                warn!(log, "failed to get pipeline status");
                super::dump_error(&log, &e);
                (Err(()), HashMap::new())
            }
        };

        let approval_state: ApprovalState = create_state_from_pipeline(&log, &mr, &pipeline_state);
        let test_state: TestState = create_state_from_pipeline(&log, &mr, &pipeline_state);

        let mut obj = MergeRequest {
            log: log,
            project: project,
            state: State::Init,
            merge_request: mr,
            test_state: test_state,
            approval_state: approval_state,
            merged: false,
            pipeline_state: pipeline_state,
        };

        while result.is_ok() {
            obj.state = obj.next_state();

            if let Err(e) = obj.update_approval_status() {
                warn!(obj.log, "failed to update approval status");
                super::dump_error(&obj.log, &e);
                result = Err(());
                break;
            }

            if let Err(e) = obj.update_test_status() {
                warn!(obj.log, "failed to update approval status");
                super::dump_error(&obj.log, &e);
                result = Err(());
                break;
            }

            if let Err(e) = obj.sync_commit_status() {
                warn!(obj.log, "failed to sync commit status");
                super::dump_error(&obj.log, &e);
                result = Err(());
                break;
            }

            break;
        }

        if result.is_err() {
            obj.state = State::Errored;
        };

        info!(obj.log, "loaded merge request status"; "status" => obj.state);

        obj
    }

    pub fn log(&self) -> &Logger {
        &self.log
    }

    pub fn state(&self) -> &State {
        &self.state
    }

    pub fn merge_request(&self) -> &gitlab::MergeRequest {
        &self.merge_request
    }

    pub fn update_target_branch(&mut self, target_branch: &BranchInfo) -> Result<()> {
        let info = if let Some(info) = self.test_state.info() {
            info.clone()
        } else {
            debug!(self.log, "test status not changed via target branch info";
                   "status" => *self.test_state.kind());
            return Ok(());
        };

        let target_sha = target_branch.gitlab_object_id();

        if info.target_sha != target_sha {
            // TODO: Cancel current running build
            let next_kind = TestStateKind::Pending;
            debug!(self.log, "test status changed via target branch info";
                   "before" => *self.test_state.kind(),
                   "after" => next_kind);
            self.test_state.update_kind(next_kind);
            self.trans_state()?;
        } else {
            debug!(self.log, "test status not changed via target branch info";
                   "status" => *self.test_state.kind());
        }

        Ok(())
    }

    pub fn start_test(&mut self, target_branch: &BranchInfo) -> Result<bool> {
        assert_matches!(self.state, State::Approved {..});
        assert_matches!(*self.test_state.kind(), TestStateKind::Pending);

        let source_project =
            self.project.gitlab().gitlab().project(self.merge_request.source_project_id)?;
        let repository = self.project.repository();

        // Fetch source branch
        repository.remote_set_url("mr", &source_project.ssh_url_to_repo)?;
        let source_branch = self.project
            .repository_fetch_branch("mr", &self.merge_request.source_branch)?;

        // Avoid force update current HEAD branch error
        self.project.repository_reset_branch(&target_branch.branch)?;

        // Create merge branch
        let merge_branch_name = format!("auto-{}", self.merge_request.target_branch);
        let merge_branch = repository.branch(&merge_branch_name, &target_branch.commit, true)?;
        let merge_branch_ref = merge_branch.get().name().unwrap();

        self.project.repository_reset_branch(&merge_branch)?;

        // Merge
        let annotated_commits =
            &[&repository.reference_to_annotated_commit(source_branch.branch.get())?];
        let mut cb = CheckoutBuilder::new();
        let _ = cb.force();
        repository.merge(annotated_commits, None, Some(&mut cb))?;
        let conflicted = repository.statuses(None)?
            .iter()
            .any(|state| state.status() == STATUS_CONFLICTED);

        if conflicted {
            info!(self.log, "conflicted!");
            repository.cleanup_state()?;
            self.test_state.update_kind(TestStateKind::Failed(None));
            self.trans_state()?;
            self.sync_commit_status()?;
            return Ok(false);
        }

        // Commit
        let merge_sha = {
            let update_ref = Some(merge_branch_ref);
            let sig = self.merge_commit_signature()?;
            let message = self.merge_commit_message(&source_project);
            let tree_oid = repository.index()?.write_tree()?;
            let tree = repository.find_tree(tree_oid)?;
            let parents = &[&target_branch.commit, &source_branch.commit];
            let merge_commit_oid =
                repository.commit(update_ref, &sig, &sig, &message, &tree, parents)?;

            repository.cleanup_state()?;

            merge_commit_oid.to_string()
        };

        info!(self.log, "successfully merged"; "sha" => merge_sha);

        // Force push
        let refspec = format!("+{}", merge_branch_ref);
        self.project.repository_push_branch("origin", &refspec)?;
        info!(self.log, "successfully pushed");

        // Update status
        let test = TestStateInfo {
            build_url: format!("{}/commit/{}/builds", self.project.project().web_url, merge_sha),
            merge_sha: ObjectId::new(&merge_sha),
            merge_branch: merge_branch_name,
            source_project_id: self.merge_request.source_project_id,
            source_branch: self.merge_request.source_branch.clone(),
            source_sha: source_branch.gitlab_object_id(),
            target_project_id: self.merge_request.target_project_id,
            target_branch: self.merge_request.target_branch.clone(),
            target_sha: target_branch.gitlab_object_id(),
        };

        self.test_state.update_kind(TestStateKind::new_running(test)?);
        self.trans_state()?;
        self.sync_commit_status()?;

        Ok(true)
    }

    pub fn push_merged(&mut self, target_branch: &BranchInfo) -> Result<bool> {
        assert_matches!(self.state, State::Success {..});
        assert_matches!(*self.test_state.kind(), TestStateKind::Success{..});

        let test_info = self.test_state.kind().info().cloned().expect("invalid test status");

        if target_branch.gitlab_object_id() != test_info.target_sha {
            // Retry
            info!(self.log, "test info not matched");
            self.test_state.update_kind(TestStateKind::Pending);
            self.trans_state()?;
            self.sync_commit_status()?;
            return Ok(false);
        }
        // TODO: more checkes

        let repository = self.project.repository();

        // Fetch merge branch
        // TODO: is there any better way?
        let merge_remote_branch = self.project
            .repository_fetch_branch("origin", &test_info.merge_branch)?;
        if test_info.merge_sha != merge_remote_branch.gitlab_object_id() {
            // Retry
            warn!(self.log, "target updated";
                  "before" => *test_info.merge_sha.value(),
                  "after" => *merge_remote_branch.gitlab_object_id().value());

            self.test_state.update_kind(TestStateKind::Pending);
            self.trans_state()?;
            self.sync_commit_status()?;
            return Ok(false);
        }

        // Avoid force update current HEAD branch error
        self.project.repository_reset_branch(&merge_remote_branch.branch)?;

        // Create merge branch
        let merge_branch =
            repository.branch(&test_info.merge_branch, &merge_remote_branch.commit, true)?;
        self.project.repository_reset_branch(&merge_branch)?;

        // Push
        let merge_branch_ref = merge_branch.get().name().unwrap();
        let target_branch_name = test_info.target_branch;
        let refspec = format!("{}:refs/heads/{}", merge_branch_ref, target_branch_name);
        // TODO: push error not shown (e.g. current user does not have enough permission)
        if let Err(e) = self.project.repository_push_branch("origin", &refspec) {
            // updated remote branch
            warn!(self.log, "failed to push. target may be updated");
            super::dump_error(&self.log, &e);
            self.test_state.update_kind(TestStateKind::Pending);
            self.trans_state()?;
            self.sync_commit_status()?;
            return Ok(false);
        }

        info!(self.log, "successfully pushed");
        // TODO: remove source branch

        self.merged = true;
        self.trans_state()?;
        self.sync_commit_status()?;

        Ok(true)
    }

    fn merge_commit_signature(&self) -> Result<Signature> {
        let current_user = self.project.gitlab().current_user();
        let sig = Signature::now(&current_user.name, &current_user.email)?;
        Ok(sig)
    }

    fn merge_commit_message(&self, source_project: &gitlab::Project) -> String {
        let approval = self.approval_state.kind().info().expect("invalid approval status");
        let mr_desc =
            self.merge_request.description.as_ref().map(|s| s.as_str()).unwrap_or_default();

        let summary = format!("Auto merge of !{} - {}:{}, r={}",
                self.merge_request.id,
                source_project.namespace.name,
                self.merge_request.source_branch,
                approval.username
        );
        let desc = format!("{}\n\n{}",
                              self.merge_request.title,
                              mr_desc);
        let appendix = format!("See merge request !{}", self.merge_request.id);

        format!("{}\n\n{}\n\n{}",
                summary,
                desc.trim_right(),
                appendix)
    }

    fn update_approval_status(&mut self) -> Result<()> {
        let next_kind = {
            let gitlab::MergeRequest { source_project_id, ref sha, .. } = self.merge_request;

            let gitlab = self.project.gitlab();

            let comments = gitlab.gitlab().commit_comments(source_project_id, sha.value())?;
            let reviewer_comments = comments.into_iter()
                .filter(|c| self.project.is_reviewer(c.author.id))
                .collect::<Vec<_>>();

            parse_comments(&reviewer_comments, gitlab.current_user())?
        };

        if next_kind != *self.approval_state.kind() {
            debug!(self.log, "approval status updated via GitLab comments";
                   "before" => *self.approval_state.kind(),
                   "after" => next_kind);
            self.approval_state.update_kind(next_kind);
            self.trans_state()?;
        } else {
            debug!(self.log, "approval status not updated via GitLab comments";
                   "status" => next_kind);
        }

        Ok(())
    }

    fn update_test_status(&mut self) -> Result<()> {
        let info = if let Some(info) = self.test_state.info() {
            info.clone()
        } else {
            debug!(self.log, "test status not updated via GitLab build status";
                   "status" => *self.test_state.kind());
            return Ok(());
        };

        let gitlab = self.project.gitlab();
        let builds = gitlab.gitlab()
            .commit_latest_builds(self.merge_request.target_project_id, info.merge_sha.value())?;

        if info.source_project_id != self.merge_request.source_project_id ||
           info.source_branch != self.merge_request.source_branch ||
           info.target_project_id != self.merge_request.target_project_id ||
           info.target_branch != self.merge_request.target_branch {
            // TODO: Cancel current running build
            let next_kind = TestStateKind::Pending;
            info!(self.log, "test status updated via merge request status";
                      "before" => *self.test_state.kind(),
                      "after" => next_kind);
            self.test_state.update_kind(next_kind);
            self.trans_state()?;
            return Ok(());
        }

        let next_kind = if builds.iter()
            .any(|b| b.status == StatusState::Pending || b.status == StatusState::Running) {
            TestStateKind::new_running(info)?
        } else if builds.iter().any(|b| b.status == StatusState::Canceled) {
            TestStateKind::new_canceled(info)?
        } else if builds.iter().any(|b| b.status == StatusState::Failed) {
            TestStateKind::new_failed(info)?
        } else if builds.iter().all(|b| b.status == StatusState::Success) {
            TestStateKind::new_success(info)?
        } else {
            warn!(self.log, "odd build statuses";
                  "status" => format!("{:?}", builds.iter().map(|b| b.status).collect::<Vec<_>>()));
            TestStateKind::new_running(info)?
        };

        if next_kind != *self.test_state.kind() {
            debug!(self.log, "test status updated via GitLab build status";
                   "before" => *self.test_state.kind(),
                   "after" => next_kind);
            self.test_state.update_kind(next_kind);
            self.trans_state()?;
        } else {
            debug!(self.log, "test status not updated via GitLab build status";
                   "status" => next_kind);
        }

        Ok(())
    }

    fn sync_commit_status(&mut self) -> Result<()> {
        sync_commit_status(&self.log,
                           self.project.gitlab(),
                           &self.approval_state,
                           &mut self.pipeline_state)?;
        sync_commit_status(&self.log,
                           self.project.gitlab(),
                           &self.test_state,
                           &mut self.pipeline_state)?;
        Ok(())
    }

    fn next_state(&self) -> State {
        let can_be_merged = match self.merge_request.merge_status {
            MergeStatus::Unchecked | MergeStatus::CanBeMerged => true,
            MergeStatus::CannotBeMerged => false,
        };

        let approval = self.approval_state.kind().info();
        if !can_be_merged {
            return State::Failed(approval.cloned());
        }

        if let Some(approval) = approval {
            return match *self.test_state.kind() {
                TestStateKind::Pending => State::Approved(approval.clone()),
                TestStateKind::Running { .. } => State::Running(approval.clone()),
                TestStateKind::Success { .. } => {
                    if self.merged {
                        State::Merged(approval.clone())
                    } else {
                        State::Success(approval.clone())
                    }
                }
                TestStateKind::Failed { .. } |
                TestStateKind::Canceled { .. } => State::Failed(Some(approval.clone())),
            };
        } else {
            State::Init
        }
    }

    fn trans_state(&mut self) -> Result<()> {
        let next_state = self.next_state();

        if self.state != next_state {
            info!(self.log, "merge request status changed";
                  "before" => self.state,
                  "after" => next_state);
            self.state = next_state;
        } else {
            debug!(self.log, "merge request status not changed";
                   "status" => next_state);
        }

        Ok(())
    }
}

fn last_pipeline_statuses(gitlab: &GitlabExt,
                          prj_id: ProjectId,
                          refname: &str,
                          commit: &str)
                          -> Result<HashMap<String, CommitStatus>> {
    let all_builds = gitlab.gitlab().commit_latest_builds(prj_id, commit)?;
    let all_statuses = gitlab.gitlab().commit_latest_statuses(prj_id, commit)?;

    // Get latest pipeline's first build
    let first_build = all_builds.iter().max_by(|a, b| {
        match a.pipeline.id.value().cmp(&b.pipeline.id.value()) {
            Ordering::Equal => a.id.value().cmp(&b.id.value()).reverse(),
            other => other,
        }
    });
    let first_build_id = first_build.map_or(0, |build| build.id.value());

    let refname = refname.to_string();
    let map = all_statuses.into_iter()
        .filter_map(move |s| {
            let is_last = s.id.value() >= first_build_id && s.ref_.as_ref() == Some(&refname);
            if is_last {
                Some((s.name.clone(), s))
            } else {
                None
            }
        })
        .collect();
    Ok(map)
}

fn create_state_from_pipeline<T>(log: &Logger,
                                 merge_request: &gitlab::MergeRequest,
                                 pipeline_state: &HashMap<String, CommitStatus>)
                                 -> T
    where T: BuildState + Debug
{
    let gitlab::MergeRequest { source_project_id: project_id,
                               source_branch: ref branch,
                               ref sha,
                               .. } = *merge_request;
    let name = T::status_name();
    let log = log.new(o!("commit_status" => name));

    let gitlab_state = pipeline_state.get(name);

    let state = gitlab_state.and_then(|commit_state| {
            match T::from_commit_status(project_id, commit_state) {
                Ok(state) => Some(state),
                Err(e) => {
                    warn!(log, "failed to parse existing commit status.");
                    super::dump_error(&log, &e);
                    trace!(log, "detail";
                           "gitlab_state" => format!("{:?}", commit_state));
                    None
                }
            }
        })
        .unwrap_or_else(|| T::init_state(project_id, branch.clone(), sha.clone()));

    debug!(log, "build status loaded";
           "gitlab_status" => gitlab_state.map(|s| s.status.as_str()),
           "loaded_kind" => *state.kind());
    trace!(log, "detail";
           "loaded_status" => format!("{:?}", state));
    state
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
enum Command {
    Approve(u64),
    CancelApprove,
}

fn parse_command(command: &str, me: &UserFull) -> Option<Command> {
    let mention = format!("@{}", me.username);
    let mut words = command.split_whitespace().skip_while(|s| *s != mention).skip(1);

    words.next().and_then(|word| {
        match word {
            "r+" => {
                let priority = words.next()
                    .and_then(|s| {
                        if s.starts_with("p=") {
                            s.trim_left_matches("p=").parse::<u64>().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                Some(Command::Approve(priority))
            }
            "r-" => Some(Command::CancelApprove),
            _ => None,
        }
    })
}

fn parse_comments<'a, I>(comments: I, me: &UserFull) -> Result<ApprovalStateKind>
    where I: IntoIterator<Item = &'a CommitNote>
{
    let mut kind = ApprovalStateKind::NotApproved;
    for comment in comments {
        if let Some(command) = parse_command(&comment.note, me) {
            match command {
                Command::Approve(p) => {
                    kind = ApprovalStateKind::new_approved(ApprovalStateInfo {
                        priority: p,
                        time: comment.created_at,
                        username: comment.author.username.clone(),
                    })?;
                }
                Command::CancelApprove => kind = ApprovalStateKind::NotApproved,
            }
        }
    }
    Ok(kind)
}

fn sync_commit_status<T>(log: &Logger,
                         gitlab: &GitlabExt,
                         state: &T,
                         pipeline_state: &mut HashMap<String, CommitStatus>)
                         -> Result<()>
    where T: BuildState
{
    let name = T::status_name();
    let log = log.new(o!("commit_status" => name));

    match pipeline_state.entry(name.into()) {
        Entry::Vacant(e) => {
            trace!(log, "no status found on GitLab. do sync.");
            let new_state = state.sync(gitlab, None)?;
            let _ = e.insert(new_state);
        }
        Entry::Occupied(mut e) => {
            let v = e.get_mut();
            if state.need_sync(v) {
                trace!(log, "override exisiting state.");
                let new_state = state.sync(gitlab, Some(v.status))?;
                *v = new_state;
            } else {
                trace!(log, "nothing to do.");
            }
        }
    }

    trace!(log, "new pipeline state";
           "state" => format!("{:?}", pipeline_state.get(name)));

    Ok(())
}
