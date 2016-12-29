#![feature(conservative_impl_trait)]
#![feature(ordering_chaining)]
#![feature(proc_macro)]

#![warn(missing_copy_implementations)]
#![warn(missing_debug_implementations)]
#![warn(trivial_casts)]
#![warn(trivial_numeric_casts)]
#![warn(unused_extern_crates)]
#![warn(unused_import_braces)]
#![warn(unused_qualifications)]
#![warn(unused_results)]

#![cfg_attr(feature="clippy", feature(plugin))]
#![cfg_attr(feature="clippy", plugin(clippy))]
#![cfg_attr(feature="clippy", warn(filter_map))]
#![cfg_attr(feature="clippy", warn(mut_mut))]
#![cfg_attr(feature="clippy", warn(option_map_unwrap_or))]
#![cfg_attr(feature="clippy", warn(option_map_unwrap_or_else))]
// #![cfg_attr(feature="clippy", warn(single_match_else))]
#![cfg_attr(feature="clippy", warn(string_add))]
#![cfg_attr(feature="clippy", warn(string_add_assign))]
#![cfg_attr(feature="clippy", warn(stutter))]
// #![cfg_attr(feature="clippy", warn(used_underscore_binding))]

//! Jaba - Just Another Build Automation
//!
//! ![State transition diagram](../../../img/state_transition.png)

extern crate chrono;
extern crate clap;
#[macro_use]
extern crate error_chain;
extern crate git2;
extern crate gitlab;
extern crate log;
#[macro_use]
extern crate matches;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate serde;
#[macro_use]
extern crate slog;
#[macro_use]
extern crate slog_envlogger;
extern crate slog_stdlog;
extern crate slog_term;
extern crate toml;

use build_state::ApprovalInfo as ApprovalStateInfo;
use config::{Git as GitConfig, Repo as RepoConfig};
use errors::*;
use gitlab_ext::GitlabExt;
use log::LogLevelFilter;
use merge_request::{MergeRequest, State as MergeRequestState};
use project::{BranchInfo, Project};
use slog::{DrainExt, Level, LevelFilter, Logger};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::collections::hash_map::Entry;

mod build_state;
mod config;
mod errors;
mod gitlab_ext;
mod merge_request;
mod project;

const APP_NAME: &'static str = env!("CARGO_PKG_NAME");
const APP_VERSION: &'static str = env!("CARGO_PKG_VERSION");

#[derive(Debug)]
struct Arg {
    log_level: u64,
}

fn parse_arg() -> Arg {
    let matches = clap::App::new(APP_NAME)
        .version(APP_VERSION)
        .author(env!("CARGO_PKG_AUTHORS"))
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .arg(clap::Arg::with_name("v").short("v").multiple(true).help("Sets a level of verbosity"))
        .get_matches();

    Arg { log_level: matches.occurrences_of("v") }
}

fn create_logger(log_level: u64) -> Logger {
    let (slog_level, stdlog_level) = match log_level {
        0 => (Level::Info, LogLevelFilter::Info),
        1 => (Level::Debug, LogLevelFilter::Debug),
        _ => (Level::Trace, LogLevelFilter::Trace),
    };

    let term = slog_term::streamer().async().compact().build();
    let level = LevelFilter::new(term, slog_level);
    let drain = slog_envlogger::new(level);

    let root = Logger::root(drain.fuse(), o!());
    let log = root.new(o!());
    slog_stdlog::set_logger_level(log.clone(), stdlog_level).expect("failed to set logget level");

    log
}

fn dump_error(log: &Logger, e: &Error) {
    warn!(log, "error: {}", e);
    for e in e.iter().skip(1) {
        warn!(log, "caused by: {}", e);
    }

    if let Some(backtrace) = e.backtrace() {
        warn!(log, "{:?}", backtrace);
    }
}

#[derive(Debug)]
struct SortBy<K, V>(K, V);
impl<K, V> PartialEq for SortBy<K, V>
    where K: PartialEq
{
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl<K, V> Eq for SortBy<K, V> where K: Eq {}
impl<K, V> PartialOrd for SortBy<K, V>
    where K: PartialOrd
{
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.0.partial_cmp(&other.0)
    }
}

impl<K, V> Ord for SortBy<K, V>
    where K: Ord
{
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

struct Queue<'a> {
    target_branch: BranchInfo<'a>,
    errored: Vec<MergeRequest<'a>>,
    init: Vec<MergeRequest<'a>>,
    approved: BinaryHeap<SortBy<ApprovalStateInfo, MergeRequest<'a>>>,
    running: BinaryHeap<SortBy<ApprovalStateInfo, MergeRequest<'a>>>,
    success: BinaryHeap<SortBy<ApprovalStateInfo, MergeRequest<'a>>>,
    merged: Vec<MergeRequest<'a>>,
    failed: Vec<(Option<ApprovalStateInfo>, MergeRequest<'a>)>,
}

impl<'a> Queue<'a> {
    fn new(project: &'a Project, target_branch_name: &str) -> Result<Self> {
        Ok(Queue {
            target_branch: project.repository_fetch_branch("origin", target_branch_name)?,
            errored: vec![],
            init: vec![],
            approved: BinaryHeap::new(),
            running: BinaryHeap::new(),
            success: BinaryHeap::new(),
            merged: vec![],
            failed: vec![],
        })
    }

    fn push(&mut self, mr: MergeRequest<'a>) {
        match mr.state().clone() {
            MergeRequestState::Init => self.init.push(mr),
            MergeRequestState::Approved(approval) => self.approved.push(SortBy(approval, mr)),
            MergeRequestState::Running(approval) => self.running.push(SortBy(approval, mr)),
            MergeRequestState::Success(approval) => self.success.push(SortBy(approval, mr)),
            MergeRequestState::Merged(_approval) => self.merged.push(mr),
            MergeRequestState::Failed(approval) => self.failed.push((approval, mr)),
            MergeRequestState::Errored => self.errored.push(mr),
        }
    }
}

fn run_repo_target(log: &Logger, queue: &mut Queue) -> Result<()> {
    info!(log, "# of queue";
              "errored" => queue.errored.len(),
              "init" => queue.init.len(),
              "approved" => queue.approved.len(),
              "running" => queue.running.len(),
              "success" => queue.success.len(),
              "merged" => queue.merged.len(),
              "failed" => queue.failed.len());

    while let Some(SortBy(_approval, mut mr)) = queue.success.pop() {
        info!(mr.log(), "success mr"; "mr" => *mr.state());

        let is_pushed = match mr.push_merged(&queue.target_branch) {
            Err(e) => {
                warn!(mr.log(), "failed to push merged");
                dump_error(mr.log(), &e);
                queue.errored.push(mr);
                continue;
            }
            Ok(is_pushed) => is_pushed,
        };

        queue.push(mr);
        if is_pushed {
            return Ok(());
        } else {
            continue;
        }
    }

    if let Some(SortBy(_approval, mr)) = queue.running.pop() {
        info!(mr.log(), "runnning mr"; "mr" => *mr.state());

        // TODO: check merge request

        // Do nothing
        queue.push(mr);
        return Ok(());
    }

    while let Some(SortBy(_approval, mut mr)) = queue.approved.pop() {
        info!(mr.log(), "approved mr"; "mr" => *mr.state());

        let is_started = match mr.start_test(&queue.target_branch) {
            Err(e) => {
                warn!(mr.log(), "failed to start test");
                dump_error(mr.log(), &e);
                queue.errored.push(mr);
                continue;
            }
            Ok(is_started) => is_started,
        };

        queue.push(mr);
        if is_started {
            return Ok(());
        } else {
            continue;
        }
    }

    Ok(())
}

fn run_repo(log: &Logger,
            label: &str,
            repo_config: &RepoConfig,
            gitlab: &GitlabExt,
            git_config: &GitConfig)
            -> Result<()> {
    let project = Project::new(log, label, repo_config, git_config, gitlab)?;

    let mut map = HashMap::new();
    for mut mr in project.opened_merge_requests()? {
        let mut queue = {
            let target_branch_name = &mr.merge_request().target_branch;

            match map.entry(target_branch_name.clone()) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => {
                    let queue = Queue::new(&project, target_branch_name)?;
                    e.insert(queue)
                }
            }
        };

        if let Err(e) = mr.update_target_branch(&queue.target_branch) {
            warn!(mr.log(), "failed to update target branch info");
            dump_error(mr.log(), &e);
            queue.errored.push(mr);
            continue;
        }

        queue.push(mr);
    }

    for (target_branch_name, queue) in &mut map {
        let log = log.new(o!("target_branch" => target_branch_name.to_string()));
        if let Err(e) = run_repo_target(&log, queue) {
            warn!(project.log(), "failed to handle target branch";
                  "taget_branch" => *target_branch_name);
            dump_error(&log, &e);
        };
    }

    Ok(())
}

fn run(log: Logger, _arg: Arg) -> Result<()> {
    info!(log, "start"; "package" => APP_NAME, "version" => APP_VERSION);

    let config = config::from_path("cfg.toml")?;
    debug!(log, "configuration file loaded");

    let gitlab = GitlabExt::new(&log, &config.gitlab)?;

    for (label, repo) in &config.repo {
        if let Err(e) = run_repo(&log, label, repo, &gitlab, &config.git) {
            warn!(log, "failed to running on repository";
                  "repository" => label.as_str());
            dump_error(&log, &e);
        }
    }

    Ok(())
}

fn main() {
    let arg = parse_arg();
    let log = create_logger(arg.log_level);

    if let Err(e) = run(log, arg) {
        println!("error: {}", e);
        for e in e.iter().skip(1) {
            println!("caused by: {}", e);
        }

        if let Some(backtrace) = e.backtrace() {
            println!("{:?}", backtrace);
        }
    }
}
