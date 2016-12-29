use config::Gitlab as GitlabConfig;
use errors::*;
use gitlab::{Gitlab, UserFull};
use slog::Logger;

#[derive(Debug)]
pub struct GitlabExt {
    log: Logger,
    gitlab: Gitlab,
    current_user: UserFull,
}

impl GitlabExt {
    pub fn new(log: &Logger, conf: &GitlabConfig) -> Result<Self> {
        let log = log.new(o!("scope" => "gitlab"));

        let gitlab = if conf.insecure {
            Gitlab::new_insecure(&conf.host, &conf.access_token)?
        } else {
            Gitlab::new(&conf.host, &conf.access_token)?
        };

        let current_user = gitlab.current_user()?;
        debug!(log, "logged in successfully";
               "username" => current_user.username,
               "user" => current_user.name,
               "email" => current_user.email);

        Ok(GitlabExt {
            log: log,
            gitlab: gitlab,
            current_user: current_user,
        })
    }

    pub fn gitlab(&self) -> &Gitlab {
        &self.gitlab
    }

    pub fn current_user(&self) -> &UserFull {
        &self.current_user
    }
}
