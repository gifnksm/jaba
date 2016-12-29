use chrono::{DateTime, UTC};
use errors::*;
use gitlab::{CommitStatus, CommitStatusInfo, ObjectId, ProjectId, StatusState};
use gitlab_ext::GitlabExt;
use serde_json;
use slog;
use std::cmp::Ordering;

pub trait State
    where Self: Sized
{
    type Kind: slog::Serialize;

    fn init_state(project_id: ProjectId, refname: String, sha: ObjectId) -> Self;
    fn from_commit_status(project_id: ProjectId, commit_status: &CommitStatus) -> Result<Self>;

    fn status_name() -> &'static str;

    fn kind(&self) -> &Self::Kind;
    fn project_id(&self) -> ProjectId;
    fn sha(&self) -> &ObjectId;

    fn to_status_state(&self) -> StatusState;
    fn to_commit_status_info(&self) -> CommitStatusInfo;

    fn need_sync(&self, commit_status: &CommitStatus) -> bool {
        if self.to_status_state() != commit_status.status {
            return true;
        }

        let info = self.to_commit_status_info();

        info.refname != commit_status.ref_.as_ref().map(|s| s.as_str()) ||
        info.name != Some(commit_status.name.as_str()) ||
        info.target_url != commit_status.target_url.as_ref().map(|s| s.as_str()) ||
        info.description != commit_status.description.as_ref().map(|s| s.as_str())
    }

    fn sync(&self, gitlab: &GitlabExt, old_state: Option<StatusState>) -> Result<CommitStatus> {
        let status_state = self.to_status_state();
        let status_info = self.to_commit_status_info();

        // TODO: Need correct state transition
        #[cfg_attr(feature="clippy",allow(match_same_arms))]
        let need_cancel = match (old_state, status_state) {
            (None, _) => false,
            (Some(StatusState::Pending), StatusState::Pending) => true,
            (Some(StatusState::Pending), _) => false,

            (Some(StatusState::Running), StatusState::Pending) => true,
            (Some(StatusState::Running), StatusState::Running) => true,
            (Some(StatusState::Running), _) => false,

            (Some(StatusState::Success), StatusState::Pending) => true,
            (Some(StatusState::Success), StatusState::Running) => true,
            (Some(StatusState::Success), StatusState::Success) => true,
            (Some(StatusState::Success), _) => false,

            (Some(StatusState::Failed), StatusState::Failed) => true,
            (Some(StatusState::Failed), _) => false,

            (Some(StatusState::Canceled), _) => false,
        };

        if need_cancel {
            let _ = gitlab.gitlab()
                .create_commit_status(self.project_id(),
                                      self.sha().value(),
                                      StatusState::Canceled,
                                      &status_info)?;
        }

        let commit_status = gitlab.gitlab()
            .create_commit_status(self.project_id(),
                                  self.sha().value(),
                                  status_state,
                                  &status_info)?;

        Ok(commit_status)
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum ApprovalKind {
    NotApproved,
    Approved { desc: String, info: ApprovalInfo },
}

impl slog::Serialize for ApprovalKind {
    fn serialize(&self,
                 _record: &slog::Record,
                 key: &'static str,
                 serializer: &mut slog::Serializer)
                 -> slog::ser::Result {
        match *self {
            ApprovalKind::NotApproved => serializer.emit_str(key, self.as_str()),
            ApprovalKind::Approved { ref info, .. } => {
                serializer.emit_arguments(key,
                                          &format_args!("{}(p={},date={},approved_by={})",
                                                        self.as_str(),
                                                        info.priority,
                                                        info.time,
                                                        info.username))
            }
        }
    }
}

impl ApprovalKind {
    pub fn new_approved(info: ApprovalInfo) -> Result<Self> {
        let desc = serde_json::to_string(&info)?;
        Ok(ApprovalKind::Approved {
            desc: desc,
            info: info,
        })
    }

    pub fn info(&self) -> Option<&ApprovalInfo> {
        if let ApprovalKind::Approved { ref info, .. } = *self {
            Some(info)
        } else {
            None
        }
    }

    fn from_commit_status(commit_status: &CommitStatus) -> Result<Self> {
        let status = match commit_status.status {
            StatusState::Pending => ApprovalKind::NotApproved,
            StatusState::Success => {
                let info = ApprovalInfo::from_commit_status(commit_status)?;
                Self::new_approved(info)?
            }
            status => bail!("invalid commit status: {:?}", status),
        };

        Ok(status)
    }

    fn as_str(&self) -> &'static str {
        match *self {
            ApprovalKind::NotApproved => "not_approved",
            ApprovalKind::Approved { .. } => "approved",
        }
    }

    fn to_status_state(&self) -> StatusState {
        match *self {
            ApprovalKind::NotApproved => StatusState::Pending,
            ApprovalKind::Approved { .. } => StatusState::Success,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct ApprovalInfo {
    pub priority: u64,
    pub time: DateTime<UTC>,
    pub username: String,
}

impl Ord for ApprovalInfo {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .cmp(&other.priority)
            .then_with(|| self.time.cmp(&other.time).reverse())
            .then_with(|| self.username.cmp(&other.username).reverse())
    }
}

impl PartialOrd for ApprovalInfo {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl ApprovalInfo {
    fn from_commit_status(commit_status: &CommitStatus) -> Result<Self> {
        let description = if let Some(ref description) = commit_status.description {
            description
        } else {
            bail!("description not found")
        };

        let info: Self = serde_json::from_str(description)?;
        Ok(info)
    }
}

#[derive(Debug)]
pub struct Approval {
    project_id: ProjectId,
    refname: String,
    sha: ObjectId,
    kind: ApprovalKind,
}

impl Approval {
    pub fn update_kind(&mut self, kind: ApprovalKind) {
        self.kind = kind;
    }
}

impl State for Approval {
    type Kind = ApprovalKind;

    fn init_state(project_id: ProjectId, refname: String, sha: ObjectId) -> Self {
        Approval {
            project_id: project_id,
            refname: refname,
            sha: sha,
            kind: ApprovalKind::NotApproved,
        }
    }

    fn from_commit_status(project_id: ProjectId, commit_status: &CommitStatus) -> Result<Self> {
        let kind = ApprovalKind::from_commit_status(commit_status)?;

        let refname = if let Some(ref refname) = commit_status.ref_ {
            refname.clone()
        } else {
            bail!("refname not found")
        };

        Ok(Approval {
            project_id: project_id,
            refname: refname,
            sha: commit_status.sha.clone(),
            kind: kind,
        })
    }

    fn status_name() -> &'static str {
        "jaba:approval"
    }

    fn kind(&self) -> &Self::Kind {
        &self.kind
    }

    fn project_id(&self) -> ProjectId {
        self.project_id
    }

    fn sha(&self) -> &ObjectId {
        &self.sha
    }

    fn to_status_state(&self) -> StatusState {
        self.kind.to_status_state()
    }

    fn to_commit_status_info(&self) -> CommitStatusInfo {
        let description = match self.kind {
            ApprovalKind::NotApproved => None,
            ApprovalKind::Approved { ref desc, .. } => Some(desc.as_str()),
        };

        CommitStatusInfo {
            refname: Some(&self.refname),
            name: Some(Self::status_name()),
            target_url: None,
            description: description,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum TestKind {
    Pending,
    Running { desc: String, info: TestInfo },
    Success { desc: String, info: TestInfo },
    Failed(Option<(String, TestInfo)>),
    Canceled { desc: String, info: TestInfo },
}

impl slog::Serialize for TestKind {
    fn serialize(&self,
                 _record: &slog::Record,
                 key: &'static str,
                 serializer: &mut slog::Serializer)
                 -> slog::ser::Result {
        match *self {
            TestKind::Pending |
            TestKind::Failed(None) => serializer.emit_str(key, self.as_str()),
            TestKind::Running { ref info, .. } |
            TestKind::Success { ref info, .. } |
            TestKind::Failed(Some((_, ref info))) |
            TestKind::Canceled { ref info, .. } => {
                serializer.emit_arguments(key,
                                          &format_args!("{}(sha={})",
                                                        self.as_str(),
                                                        info.merge_sha.value()))
            }
        }
    }
}

impl TestKind {
    pub fn new_running(info: TestInfo) -> Result<Self> {
        let desc = serde_json::to_string(&info)?;
        Ok(TestKind::Running {
            desc: desc,
            info: info,
        })
    }

    pub fn new_success(info: TestInfo) -> Result<Self> {
        let desc = serde_json::to_string(&info)?;
        Ok(TestKind::Success {
            desc: desc,
            info: info,
        })
    }

    pub fn new_failed(info: TestInfo) -> Result<Self> {
        let desc = serde_json::to_string(&info)?;
        Ok(TestKind::Failed(Some((desc, info))))
    }

    pub fn new_canceled(info: TestInfo) -> Result<Self> {
        let desc = serde_json::to_string(&info)?;
        Ok(TestKind::Canceled {
            desc: desc,
            info: info,
        })
    }

    pub fn info(&self) -> Option<&TestInfo> {
        match *self {
            TestKind::Pending |
            TestKind::Failed(None) => None,
            TestKind::Running { ref info, .. } |
            TestKind::Success { ref info, .. } |
            TestKind::Failed(Some((_, ref info))) |
            TestKind::Canceled { ref info, .. } => Some(info),
        }
    }

    fn from_commit_status(commit_status: &CommitStatus) -> Result<Self> {
        if commit_status.status == StatusState::Pending {
            return Ok(TestKind::Pending);
        }
        if commit_status.status == StatusState::Failed && commit_status.description.is_none() {
            return Ok(TestKind::Failed(None));
        }

        let info = TestInfo::from_commit_status(commit_status)?;

        match commit_status.status {
            StatusState::Pending => unreachable!(),
            StatusState::Running => Self::new_running(info),
            StatusState::Success => Self::new_success(info),
            StatusState::Failed => Self::new_failed(info),
            StatusState::Canceled => Self::new_canceled(info),
        }
    }

    fn as_str(&self) -> &'static str {
        match *self {
            TestKind::Pending => "pending",
            TestKind::Running { .. } => "running",
            TestKind::Success { .. } => "success",
            TestKind::Failed { .. } => "failed",
            TestKind::Canceled { .. } => "canceled",
        }
    }

    fn to_status_state(&self) -> StatusState {
        match *self {
            TestKind::Pending => StatusState::Pending,
            TestKind::Running { .. } => StatusState::Running,
            TestKind::Success { .. } => StatusState::Success,
            TestKind::Failed { .. } => StatusState::Failed,
            TestKind::Canceled { .. } => StatusState::Canceled,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct TestInfo {
    pub build_url: String,
    pub merge_sha: ObjectId,
    pub merge_branch: String,
    pub source_project_id: ProjectId,
    pub source_branch: String,
    pub source_sha: ObjectId,
    pub target_project_id: ProjectId,
    pub target_branch: String,
    pub target_sha: ObjectId,
}

impl TestInfo {
    fn from_commit_status(commit_status: &CommitStatus) -> Result<Self> {
        let description = if let Some(ref description) = commit_status.description {
            description
        } else {
            bail!("description not found")
        };

        let info: Self = serde_json::from_str(description)?;
        Ok(info)
    }
}

#[derive(Debug)]
pub struct Test {
    project_id: ProjectId,
    refname: String,
    sha: ObjectId,
    kind: TestKind,
}

impl State for Test {
    type Kind = TestKind;

    fn init_state(project_id: ProjectId, refname: String, sha: ObjectId) -> Self {
        Test {
            project_id: project_id,
            refname: refname,
            sha: sha,
            kind: TestKind::Pending,
        }
    }

    fn from_commit_status(project_id: ProjectId, commit_status: &CommitStatus) -> Result<Self> {
        let kind = TestKind::from_commit_status(commit_status)?;

        let refname = if let Some(ref refname) = commit_status.ref_ {
            refname.clone()
        } else {
            bail!("refname not found")
        };

        Ok(Test {
            project_id: project_id,
            refname: refname,
            sha: commit_status.sha.clone(),
            kind: kind,
        })
    }

    fn status_name() -> &'static str {
        "jaba:test"
    }

    fn kind(&self) -> &Self::Kind {
        &self.kind
    }

    fn project_id(&self) -> ProjectId {
        self.project_id
    }

    fn sha(&self) -> &ObjectId {
        &self.sha
    }

    fn to_status_state(&self) -> StatusState {
        self.kind.to_status_state()
    }

    fn to_commit_status_info(&self) -> CommitStatusInfo {
        let (target_url, description) = match self.kind {
            TestKind::Pending |
            TestKind::Failed(None) => (None, None),
            TestKind::Running { ref desc, ref info } |
            TestKind::Success { ref desc, ref info } |
            TestKind::Failed(Some((ref desc, ref info))) |
            TestKind::Canceled { ref desc, ref info } => {
                (Some(info.build_url.as_str()), Some(desc.as_str()))
            }
        };

        CommitStatusInfo {
            refname: Some(&self.refname),
            name: Some(Self::status_name()),
            target_url: target_url,
            description: description,
        }
    }
}

impl Test {
    pub fn info(&self) -> Option<&TestInfo> {
        match self.kind {
            TestKind::Pending |
            TestKind::Failed(None) => None,
            TestKind::Running { ref info, .. } |
            TestKind::Success { ref info, .. } |
            TestKind::Failed(Some((_, ref info))) |
            TestKind::Canceled { ref info, .. } => Some(info),
        }
    }

    pub fn update_kind(&mut self, kind: TestKind) {
        self.kind = kind;
    }
}
