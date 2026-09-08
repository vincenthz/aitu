//! Configuration: which agent homes to scan, and price overrides.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::pricing::PriceOverride;

/// One set of agent state directories. A home is the unit of attribution:
/// every event carries the name of the home it was found in.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Home {
    pub name: String,
    /// Claude Code config directory, i.e. the one holding `projects/`.
    pub claude: Option<PathBuf>,
    /// Codex home, i.e. the one holding `sessions/`.
    pub codex: Option<PathBuf>,
}

#[derive(Debug)]
pub struct Config {
    pub homes: Vec<Home>,
    pub prices: Vec<(String, PriceOverride)>,
    /// Where the config was read from, or where it would be read from.
    pub path: PathBuf,
    pub loaded: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default, alias = "home")]
    homes: Vec<RawHome>,
    #[serde(default)]
    prices: std::collections::BTreeMap<String, PriceOverride>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHome {
    name: String,
    claude: Option<String>,
    codex: Option<String>,
}

pub fn config_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("AITU_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    if let Some(dir) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(dir).join("aitu").join("config.toml"));
    }
    Ok(home_dir()?.join(".config").join("aitu").join("config.toml"))
}

pub fn load() -> Result<Config> {
    let path = config_path()?;
    if !path.exists() {
        return Ok(Config {
            homes: vec![default_home()?],
            prices: Vec::new(),
            path,
            loaded: false,
        });
    }

    let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let raw: RawConfig =
        toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;

    let mut homes = Vec::new();
    for entry in raw.homes {
        if entry.name.trim().is_empty() {
            return Err(anyhow!("{}: a [[home]] has an empty name", path.display()));
        }
        if homes.iter().any(|home: &Home| home.name == entry.name) {
            return Err(anyhow!(
                "{}: duplicate home name {:?}",
                path.display(),
                entry.name
            ));
        }
        if entry.claude.is_none() && entry.codex.is_none() {
            return Err(anyhow!(
                "{}: home {:?} sets neither `claude` nor `codex`",
                path.display(),
                entry.name
            ));
        }
        homes.push(Home {
            name: entry.name,
            claude: entry.claude.map(|p| expand(&p)).transpose()?,
            codex: entry.codex.map(|p| expand(&p)).transpose()?,
        });
    }
    if homes.is_empty() {
        homes.push(default_home()?);
    }

    Ok(Config {
        homes,
        prices: raw.prices.into_iter().collect(),
        path,
        loaded: true,
    })
}

/// With no config file, fall back to the locations the agents themselves use,
/// honouring the env vars that relocate them.
fn default_home() -> Result<Home> {
    let home = home_dir()?;
    let claude = match env::var_os("CLAUDE_CONFIG_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => home.join(".claude"),
    };
    let codex = match env::var_os("CODEX_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => home.join(".codex"),
    };
    Ok(Home {
        name: String::from("default"),
        claude: Some(claude),
        codex: Some(codex),
    })
}

fn expand(path: &str) -> Result<PathBuf> {
    let path = path.trim();
    if path == "~" {
        return home_dir();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return Ok(home_dir()?.join(rest));
    }
    Ok(PathBuf::from(path))
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("HOME is not set; set AITU_CONFIG and list homes explicitly"))
}

/// Written by `aitu init` so the format is discoverable without docs.
pub fn example_config(home: &Path) -> String {
    format!(
        "# aitu - AI token usage
#
# Each [[home]] is one set of agent state directories, scanned and reported
# under its name. Add as many as you have. `claude` points at the directory
# holding `projects/`; `codex` at the one holding `sessions/`.

[[home]]
name = \"personal\"
claude = \"{claude}\"
codex = \"{codex}\"

# [[home]]
# name = \"work\"
# claude = \"~/work/.claude\"

# Override or add model prices, in USD per million tokens. Anything not set
# is derived from `input`: 1.25x for a 5-minute cache write, 2x for a 1-hour
# write, 0.1x for a cache read. Use `aitu models` to find unpriced models.
#
# [prices.\"some-new-model\"]
# input = 5.0
# output = 25.0
# cache_read = 0.5
",
        claude = home.join(".claude").display(),
        codex = home.join(".codex").display(),
    )
}

pub fn write_example_config(path: &Path) -> Result<()> {
    if path.exists() {
        return Err(anyhow!("{} already exists", path.display()));
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    fs::write(path, example_config(&home_dir()?))
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Vec<Home>> {
        let raw: RawConfig = toml::from_str(text)?;
        raw.homes
            .into_iter()
            .map(|entry| {
                Ok(Home {
                    name: entry.name,
                    claude: entry.claude.map(|p| expand(&p)).transpose()?,
                    codex: entry.codex.map(|p| expand(&p)).transpose()?,
                })
            })
            .collect()
    }

    #[test]
    fn reads_several_homes_with_independent_agent_paths() {
        let homes = parse(
            r#"
            [[home]]
            name = "personal"
            claude = "/a/.claude"
            codex = "/a/.codex"

            [[home]]
            name = "work"
            claude = "/b/elsewhere/claude"
            "#,
        )
        .unwrap();

        assert_eq!(homes.len(), 2);
        assert_eq!(homes[0].codex, Some(PathBuf::from("/a/.codex")));
        assert_eq!(homes[1].claude, Some(PathBuf::from("/b/elsewhere/claude")));
        assert_eq!(homes[1].codex, None);
    }

    #[test]
    fn accepts_singular_home_key_as_alias() {
        let homes = parse("[[home]]\nname = \"only\"\nclaude = \"/x\"\n").unwrap();
        assert_eq!(homes[0].name, "only");
    }

    #[test]
    fn rejects_unknown_keys_rather_than_ignoring_them() {
        let err = toml::from_str::<RawConfig>("[[home]]\nname = \"a\"\nclaud = \"/x\"\n");
        assert!(err.is_err());
    }

    #[test]
    fn expands_leading_tilde_only() {
        unsafe { env::set_var("HOME", "/home/test") };
        assert_eq!(expand("~/x").unwrap(), PathBuf::from("/home/test/x"));
        assert_eq!(expand("~").unwrap(), PathBuf::from("/home/test"));
        assert_eq!(expand("/a/~/b").unwrap(), PathBuf::from("/a/~/b"));
    }
}
