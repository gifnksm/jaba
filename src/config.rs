pub use errors::*;
use serde::Deserialize;
use std::{error, fmt};
use std::collections::HashMap;
use std::fs::File;
use std::io::prelude::*;
use std::path::Path;
use toml;

#[derive(Debug, Clone)]
pub struct Config {
    pub gitlab: Gitlab,
    pub git: Git,
    pub repo: HashMap<String, Repo>,
}

#[derive(Debug, Clone)]
pub struct Gitlab {
    pub host: String,
    pub access_token: String,
    pub insecure: bool,
}

#[derive(Debug, Clone)]
pub struct Git {
    pub ssh_key: String,
}

#[derive(Debug, Clone)]
pub struct Repo {
    pub name: String,
}

pub fn from_path<P>(path: P) -> Result<Config>
    where P: AsRef<Path>
{
    let file = read_file(path)?;
    let toml = parse_toml(&file)?;
    decode(toml)
}

fn read_file<P>(path: P) -> Result<String>
    where P: AsRef<Path>
{
    let mut file = File::open(path)?;
    let mut input = String::new();
    let _ = file.read_to_string(&mut input)?;
    Ok(input)
}

fn parse_toml(input: &str) -> Result<toml::Value> {
    let mut parser = toml::Parser::new(input);
    let toml = match parser.parse() {
        None => return Err(TomlParserError::new(&parser).unwrap().into()),
        Some(v) => toml::Value::Table(v),
    };
    Ok(toml)
}

fn decode(toml: toml::Value) -> Result<Config> {
    let raw: RawConfig = Deserialize::deserialize(&mut toml::Decoder::new(toml))?;
    Ok(raw.into())
}

#[derive(Deserialize)]
struct RawConfig {
    gitlab: RawGitlab,
    git: RawGit,
    repo: HashMap<String, RawRepo>,
}

impl Into<Config> for RawConfig {
    fn into(self) -> Config {
        Config {
            gitlab: self.gitlab.into(),
            git: self.git.into(),
            repo: self.repo.into_iter().map(|(name, repo)| (name, repo.into())).collect(),
        }
    }
}

#[derive(Deserialize)]
struct RawGitlab {
    host: String,
    access_token: String,
    insecure: Option<bool>,
}

impl Into<Gitlab> for RawGitlab {
    fn into(self) -> Gitlab {
        Gitlab {
            host: self.host,
            access_token: self.access_token,
            insecure: self.insecure.unwrap_or(false),
        }
    }
}

#[derive(Deserialize)]
struct RawGit {
    ssh_key: String,
}

impl Into<Git> for RawGit {
    fn into(self) -> Git {
        Git { ssh_key: self.ssh_key }
    }
}

#[derive(Deserialize)]
struct RawRepo {
    name: String,
}

impl Into<Repo> for RawRepo {
    fn into(self) -> Repo {
        Repo { name: self.name }
    }
}

#[derive(Debug)]
pub struct TomlParserError {
    lo_pos: (usize, usize),
    hi_pos: (usize, usize),
    cause: toml::ParserError,
}

impl TomlParserError {
    fn new(parser: &toml::Parser) -> Option<TomlParserError> {
        if parser.errors.is_empty() {
            return None;
        }
        let e = &parser.errors[0];
        Some(TomlParserError {
            lo_pos: parser.to_linecol(e.lo),
            hi_pos: parser.to_linecol(e.hi),
            cause: e.clone(),
        })
    }
}

impl fmt::Display for TomlParserError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f,
               "{}:{}: {}:{} {}",
               self.lo_pos.0,
               self.lo_pos.1,
               self.hi_pos.0,
               self.hi_pos.1,
               self.cause)
    }
}

impl error::Error for TomlParserError {
    fn description(&self) -> &str {
        self.cause.description()
    }

    fn cause(&self) -> Option<&error::Error> {
        Some(&self.cause)
    }
}