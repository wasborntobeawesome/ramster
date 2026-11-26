//! Rules as data: `~/.ramster/rules.d/*.json`, so people can write and share
//! rule packs without touching the code.
//!
//! A file holds `{"rules": [...]}` or a bare array. One rule:
//!
//! ```json
//! {
//!   "id": "elasticsearch",
//!   "title": "Elasticsearch dev node",
//!   "match": {"args": "org.elasticsearch.bootstrap.Elasticsearch"},
//!   "tier": "confirm",
//!   "min_mb": 500,
//!   "when": "idle",
//!   "action": "term",
//!   "tip": "Lower -Xmx in config/jvm.options.d for local development."
//! }
//! ```
//!
//! `match` fields (`name`, `args`, `path`, `app`) take a string or a list of
//! strings; a field matches if any entry is a case-insensitive substring
//! (`app` must equal the app name). Every given field must match. With only
//! `app`, the rule applies to the whole app. Busy processes are never
//! suggested, whatever the rule says.

use serde::{Deserialize, Serialize};
use std::path::Path;

use super::rules::{short_cmd, Action, Ctx, Finding, CONFIRM, INFO, SAFE};
use super::{exe, Idleness};
use crate::probe::Proc;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CustomRule {
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(rename = "match")]
    pub matcher: Match,
    #[serde(default = "default_tier")]
    pub tier: String,
    #[serde(default = "default_min_mb")]
    pub min_mb: u64,
    #[serde(default)]
    pub when: When,
    #[serde(default)]
    pub action: CustomAction,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tip: String,
    /// File the rule came from.
    #[serde(default, skip_deserializing)]
    pub source: String,
}

fn default_tier() -> String {
    CONFIRM.into()
}

fn default_min_mb() -> u64 {
    100
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Match {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<Patterns>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Patterns>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<Patterns>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<Patterns>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Patterns {
    One(String),
    Many(Vec<String>),
}

impl Patterns {
    fn list(&self) -> Vec<String> {
        match self {
            Patterns::One(s) => vec![s.to_lowercase()],
            Patterns::Many(v) => v.iter().map(|s| s.to_lowercase()).collect(),
        }
    }

    fn contained_in(&self, field: &str) -> bool {
        let f = field.to_lowercase();
        self.list()
            .iter()
            .any(|p| !p.is_empty() && f.contains(p.as_str()))
    }

    fn equals(&self, field: &str) -> bool {
        let f = field.to_lowercase();
        self.list().contains(&f)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum When {
    /// Idle now (or, with the sampler, for `idle_minutes`).
    #[default]
    Idle,
    /// Idle and reparented to launchd (its terminal or agent is gone).
    Detached,
    /// Whenever it is not busy.
    Always,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CustomAction {
    /// "term", "quit" or "none".
    Named(String),
    Cmd {
        cmd: Vec<String>,
    },
}

impl Default for CustomAction {
    fn default() -> Self {
        CustomAction::Named("term".into())
    }
}

impl CustomRule {
    fn app_only(&self) -> bool {
        let m = &self.matcher;
        m.app.is_some() && m.name.is_none() && m.args.is_none() && m.path.is_none()
    }

    /// Problems that make the rule unusable.
    pub fn validate(&self) -> Result<(), String> {
        let m = &self.matcher;
        if self.id.trim().is_empty() || self.id.contains(char::is_whitespace) {
            return Err("\"id\" must be a non-empty word like \"my-server\"".into());
        }
        if m.name.is_none() && m.args.is_none() && m.path.is_none() && m.app.is_none() {
            return Err("\"match\" needs at least one of name, args, path, app".into());
        }
        if self.app_only() && self.when == When::Detached {
            return Err(
                "apps are never detached: use \"when\": \"idle\" with an \"app\" match".into(),
            );
        }
        if ![SAFE, CONFIRM, INFO].contains(&self.tier.as_str()) {
            return Err(format!(
                "\"tier\" must be safe, confirm or info, not {:?}",
                self.tier
            ));
        }
        match &self.action {
            CustomAction::Named(a) if a == "quit" && !self.app_only() => {
                Err("\"action\": \"quit\" needs a match on \"app\" only".into())
            }
            CustomAction::Named(a) if !matches!(a.as_str(), "term" | "quit" | "none") => Err(
                format!("\"action\" must be term, quit, none or {{\"cmd\": [...]}}, not {a:?}"),
            ),
            CustomAction::Cmd { cmd } if cmd.is_empty() || cmd[0].is_empty() => {
                Err("\"cmd\" must name a program".into())
            }
            _ => Ok(()),
        }
    }
}

/// Load every `*.json` in `dir` (sorted by name). A broken rule is reported
/// and skipped; the rest of its file still loads.
pub fn load(dir: &Path) -> (Vec<CustomRule>, Vec<String>) {
    let (mut rules, mut errors) = (Vec::new(), Vec::new());
    let Ok(rd) = std::fs::read_dir(dir) else {
        return (rules, errors);
    };
    let mut files: Vec<_> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    for f in files {
        let name = f
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let parsed: Result<serde_json::Value, String> = std::fs::read(&f)
            .map_err(|e| e.to_string())
            .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()));
        let items = match parsed {
            Ok(serde_json::Value::Array(a)) => a,
            Ok(serde_json::Value::Object(mut o)) => match o.remove("rules") {
                Some(serde_json::Value::Array(a)) => a,
                _ => {
                    errors.push(format!("{name}: expected {{\"rules\": [...]}} or an array"));
                    continue;
                }
            },
            Ok(_) => {
                errors.push(format!("{name}: expected {{\"rules\": [...]}} or an array"));
                continue;
            }
            Err(e) => {
                errors.push(format!("{name}: {e}"));
                continue;
            }
        };
        for (i, item) in items.into_iter().enumerate() {
            match serde_json::from_value::<CustomRule>(item)
                .map_err(|e| e.to_string())
                .and_then(|r| r.validate().map(|_| r))
            {
                Ok(mut r) => {
                    r.source = name.clone();
                    rules.push(r);
                }
                Err(e) => errors.push(format!("{name}: rule {}: {e}", i + 1)),
            }
        }
    }
    (rules, errors)
}

fn proc_matches(c: &Ctx, r: &CustomRule, p: &Proc) -> bool {
    let m = &r.matcher;
    let name_ok = m
        .name
        .as_ref()
        .is_none_or(|pat| pat.contained_in(exe(p)) || pat.contained_in(&p.name));
    let args_ok = m.args.as_ref().is_none_or(|pat| pat.contained_in(&p.args));
    let path_ok = m.path.as_ref().is_none_or(|pat| pat.contained_in(&p.path));
    let app_ok = m
        .app
        .as_ref()
        .is_none_or(|pat| c.s.group_of.get(&p.pid).is_some_and(|g| pat.equals(g)));
    name_ok && args_ok && path_ok && app_ok
}

fn wanted(r: &CustomRule, idle: &Idleness, detached: bool) -> bool {
    if idle.known && !idle.idle {
        return false; // never interrupt work, whatever the rule says
    }
    match r.when {
        When::Always => true,
        When::Idle => idle.known,
        When::Detached => idle.known && detached,
    }
}

fn act_of(r: &CustomRule) -> (Action, String) {
    match &r.action {
        CustomAction::Cmd { cmd } => (Action::Cmd(cmd.clone()), format!("run: {}", cmd.join(" "))),
        CustomAction::Named(a) => match a.as_str() {
            "term" => (Action::Term, "stop it (SIGTERM)".into()),
            "quit" => (Action::None, "quit it normally".into()), // filled in per app
            _ => (Action::None, String::new()),
        },
    }
}

fn tier(r: &CustomRule) -> &'static str {
    match r.tier.as_str() {
        SAFE => SAFE,
        INFO => INFO,
        _ => CONFIRM,
    }
}

pub(super) fn run(c: &Ctx) -> Vec<Finding> {
    let mut out = Vec::new();
    let me = std::process::id() as i32;
    for r in &c.cfg.custom_rules {
        if r.app_only() {
            out.extend(app_rule(c, r));
            continue;
        }
        for p in &c.s.procs {
            if p.uid != c.me || !p.readable || p.pid == me || !proc_matches(c, r, p) {
                continue;
            }
            if c.s.proc(p.ppid).is_some_and(|q| proc_matches(c, r, q)) {
                continue; // counted with its parent
            }
            let pids = c.s.tree(p.pid);
            if c.s.bytes_of(&pids) >> 20 < r.min_mb {
                continue;
            }
            let idle = c.s.idle_procs(&pids, c.cfg.idle_minutes);
            if !wanted(r, &idle, p.ppid == 1) {
                continue;
            }
            let title = if r.title.is_empty() {
                format!("{}: {}", r.id, short_cmd(p))
            } else {
                r.title.clone()
            };
            let mut detail = format!("{idle}, up {}", c.age(p));
            if !r.detail.is_empty() {
                detail = format!("{}, {detail}", r.detail);
            }
            let (act, action) = act_of(r);
            let mut f = c.term(&r.id, p, pids, tier(r), title, detail, &action);
            f.act = act;
            f.tip = r.tip.clone();
            out.push(f);
        }
    }
    out
}

fn app_rule(c: &Ctx, r: &CustomRule) -> Vec<Finding> {
    let Some(pat) = &r.matcher.app else {
        return vec![];
    };
    c.s.groups
        .iter()
        .filter(|g| pat.equals(&g.name) && g.mb >= r.min_mb)
        .filter_map(|g| {
            let mut idle = c.s.idle_group(&g.name, c.cfg.idle_minutes);
            if !idle.known {
                idle = c.s.idle_procs(&g.pids, c.cfg.idle_minutes);
            }
            if !wanted(r, &idle, false) {
                return None;
            }
            let (mut act, mut action) = act_of(r);
            if matches!(&r.action, CustomAction::Named(a) if a == "quit") {
                act = Action::Quit(g.name.clone());
                action = "quit it normally (it may ask to save)".into();
            }
            let starts = g
                .pids
                .iter()
                .filter_map(|&p| c.s.proc(p).map(|q| (p, q.start)))
                .collect();
            let mut detail = format!("{idle}, {} processes", g.procs);
            if !r.detail.is_empty() {
                detail = format!("{}, {detail}", r.detail);
            }
            Some(Finding {
                id: r.id.clone(),
                rule: r.id.clone(),
                safety: tier(r),
                title: if r.title.is_empty() {
                    format!("{} uses {}", g.name, crate::util::fmt_mb(g.mb))
                } else {
                    r.title.clone()
                },
                detail,
                frees_mb: g.mb,
                action,
                tip: r.tip.clone(),
                pids: g.pids.clone(),
                act,
                starts,
            })
        })
        .collect()
}
