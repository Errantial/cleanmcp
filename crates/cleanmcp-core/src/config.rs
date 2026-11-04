//! User configuration (`config.toml`).
//!
//! Location: `$CLEANMCP_CONFIG`, else `%APPDATA%\cleanmcp\config.toml` on Windows
//! or `~/.config/cleanmcp/config.toml` on Linux. A missing file means defaults.
//!
//! ```toml
//! # Always skip these while scanning (same syntax as --exclude).
//! exclude = ["**/.git"]
//! # Turn off built-in rules by id (see `cleanmcp rules`).
//! disable_rules = ["xdg_cache"]
//!
//! [[rule]]
//! id = "old_renders"
//! path = "**/Videos/renders"      # or: dir_name = "renders"
//! safety = "review"               # or "regenerable"
//! reason = "Rendered video output; can be re-rendered"
//! advice = "Keep the final cut only"
//! ```

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::rules::{Rule, RuleSet, builtin_rules};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub disable_rules: Vec<String>,
    #[serde(default, rename = "rule")]
    pub rules: Vec<Rule>,
}

/// Where logs and other app data go: `%LOCALAPPDATA%\cleanmcp` or `~/.local/share/cleanmcp`.
pub fn data_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|d| d.join("cleanmcp"))
}

/// Where the config file is read from.
pub fn config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CLEANMCP_CONFIG") {
        return Some(PathBuf::from(p));
    }
    dirs::config_dir().map(|d| d.join("cleanmcp").join("config.toml"))
}

impl Config {
    /// Load from `path` (or the default location). A missing file gives the default config.
    pub fn load(path: Option<&Path>) -> Result<Config, String> {
        let Some(path) = path.map(Path::to_path_buf).or_else(config_path) else {
            return Ok(Config::default());
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        let cfg: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        let builtin_ids: Vec<String> = builtin_rules().into_iter().map(|r| r.id).collect();
        for id in &cfg.disable_rules {
            if !builtin_ids.contains(id) {
                return Err(format!(
                    "disable_rules: no built-in rule '{id}' (see `cleanmcp rules`)"
                ));
            }
        }
        for r in &cfg.rules {
            if let Some(p) = &r.path {
                let p = p.replace('\\', "/");
                let anchored =
                    p.starts_with('/') || p.starts_with("**") || p.as_bytes().get(1) == Some(&b':');
                if !anchored {
                    return Err(format!(
                        "rule '{}': path '{p}' is matched against full paths; start it with '**/' or a drive/root",
                        r.id
                    ));
                }
            }
            if r.dir_name.is_some() == r.path.is_some() {
                return Err(format!(
                    "rule '{}': set exactly one of dir_name or path",
                    r.id
                ));
            }
        }
        Ok(cfg)
    }

    /// User rules first (so they win), then built-ins that are not disabled.
    pub fn rule_set(&self) -> Result<RuleSet, String> {
        let mut rules: Vec<Rule> = self.rules.clone();
        rules.extend(
            builtin_rules()
                .into_iter()
                .filter(|r| !self.disable_rules.contains(&r.id)),
        );
        RuleSet::new(rules).map_err(|e| format!("invalid rule glob: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example_and_validates() {
        let cfg = Config::parse(
            r#"
            exclude = ["**/.git"]
            disable_rules = ["node_modules"]
            [[rule]]
            id = "renders"
            path = "**/renders"
            safety = "review"
            reason = "old renders"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.exclude, ["**/.git"]);
        let set = cfg.rule_set().unwrap();
        assert_eq!(set.rules()[0].id, "renders");
        assert!(set.rules().iter().all(|r| r.id != "node_modules"));

        let bad = Config::parse("[[rule]]\nid='x'\nsafety='review'\nreason='r'\n");
        assert!(bad.is_err());
        assert!(Config::parse("unknown_key = 1").is_err());
        assert!(Config::parse("disable_rules = ['no_such_rule']").is_err());
        let relative = "[[rule]]\nid='x'\npath='Videos/r'\nsafety='review'\nreason='r'\n";
        assert!(Config::parse(relative).is_err());
    }

    #[test]
    fn missing_file_is_default() {
        let cfg = Config::load(Some(Path::new("/definitely/not/here.toml"))).unwrap();
        assert!(cfg.rules.is_empty());
    }
}
