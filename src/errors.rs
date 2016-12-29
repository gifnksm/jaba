use config;
use git2;
use gitlab;
use log;
use serde_json;
use std::io;
use toml;

error_chain! {
    foreign_links {
        Git(git2::Error);
        Gitlab(gitlab::Error);
        TomlDecode(toml::DecodeError);
        TomlParser(config::TomlParserError);
        SetLogger(log::SetLoggerError);
        SerdeJson(serde_json::Error);
        Io(io::Error);
    }
}
