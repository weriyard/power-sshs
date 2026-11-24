//! Live SSH connections per host, and the terminal tabs that hold them.
//!
//! Connections are read from the process table: every `ssh` process whose
//! standard input is a pseudo-terminal counts, whoever started it. Which tab
//! owns that terminal is asked of the terminal emulator:
//!
//! - `WezTerm` reports the tty of every pane (`wezterm cli list`), so a
//!   connection maps to its tab exactly.
//! - GNOME Terminal does not expose ttys. Its tab search (the same D-Bus
//!   search provider GNOME Shell uses) matches tab titles, so a tab is found
//!   only when its title contains the host name.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::BuildHasher;
use std::process::Command;
use std::sync::LazyLock;

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::Deserialize;

/// The terminal emulator this process runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalKind {
    WezTerm,
    Gnome,
    /// Any other terminal: connections are counted, tabs cannot be focused.
    Other,
}

impl TerminalKind {
    #[must_use]
    pub fn detect() -> TerminalKind {
        let var = |name: &str| std::env::var(name).unwrap_or_default();
        if var("TERM_PROGRAM") == "WezTerm" || !var("WEZTERM_PANE").is_empty() {
            TerminalKind::WezTerm
        } else if !var("GNOME_TERMINAL_SCREEN").is_empty() {
            TerminalKind::Gnome
        } else {
            TerminalKind::Other
        }
    }
}

/// A running interactive `ssh` process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    /// The destination as given to ssh, without user and port.
    pub destination: String,
    /// The pseudo-terminal it runs on, for example `/dev/pts/5`.
    pub tty: String,
}

/// A terminal tab that can be brought to front.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub label: String,
    pub target: JumpTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JumpTarget {
    WezTermPane { tab_id: u64, pane_id: u64 },
    GnomeScreen(String),
}

/// ssh options that take a value, per `ssh(1)`.
const SSH_OPTIONS_WITH_VALUE: &str = "BbcDEeFIiJLlmOoPpQRSWw";

/// The destination argument of an ssh command line (without `argv[0]`).
#[must_use]
pub fn ssh_destination(args: &[String]) -> Option<String> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "--" {
            return iter.next().map(|d| normalize_destination(d));
        }
        if let Some(flags) = arg.strip_prefix('-').filter(|f| !f.is_empty()) {
            for (i, c) in flags.char_indices() {
                if SSH_OPTIONS_WITH_VALUE.contains(c) {
                    // The value is either attached (`-p22`) or the next argument.
                    if i + c.len_utf8() == flags.len() {
                        iter.next();
                    }
                    break;
                }
            }
            continue;
        }
        return Some(normalize_destination(arg));
    }
    None
}

/// `user@host`, `ssh://user@host:port` → `host`.
#[must_use]
pub fn normalize_destination(raw: &str) -> String {
    let uri = raw.strip_prefix("ssh://");
    let rest = uri.unwrap_or(raw);
    let host = rest.rsplit_once('@').map_or(rest, |(_, h)| h);
    let host = match uri {
        Some(_) => host.split(':').next().unwrap_or(host),
        None => host,
    };
    host.to_string()
}

/// Every interactive `ssh` process of this user.
#[must_use]
pub fn live_connections() -> Vec<Connection> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };

    entries
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_string_lossy()
                .bytes()
                .all(|b| b.is_ascii_digit())
        })
        .filter_map(|e| {
            let dir = e.path();
            if fs::read_to_string(dir.join("comm")).ok()?.trim_end() != "ssh" {
                return None;
            }
            let tty = fs::read_link(dir.join("fd/0")).ok()?;
            let tty = tty.to_string_lossy();
            if !tty.starts_with("/dev/pts/") {
                return None;
            }
            let cmdline = fs::read(dir.join("cmdline")).ok()?;
            let args: Vec<String> = cmdline
                .split(|b| *b == 0)
                .filter(|a| !a.is_empty())
                .skip(1)
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect();
            Some(Connection {
                destination: ssh_destination(&args)?,
                tty: tty.into_owned(),
            })
        })
        .collect()
}

/// The ttys holding a connection to a host, matched by its name or by its
/// `HostName`.
#[must_use]
pub fn ttys_for(connections: &[Connection], name: &str, destination: &str) -> HashSet<String> {
    connections
        .iter()
        .filter(|c| {
            c.destination.eq_ignore_ascii_case(name)
                || c.destination.eq_ignore_ascii_case(destination)
        })
        .map(|c| c.tty.clone())
        .collect()
}

/// The `wezterm` CLI of the running `WezTerm`.
///
/// `WezTerm` exports the directory of its own executables. Using the CLI from
/// there avoids `PATH`, where `wezterm` may be an `AppImage` that has to be
/// mounted again on every call — which can fail, and is slow.
fn wezterm() -> Command {
    let bundled = std::env::var_os("WEZTERM_EXECUTABLE_DIR")
        .map(|dir| std::path::PathBuf::from(dir).join("wezterm"))
        .filter(|path| path.is_file());
    Command::new(bundled.map_or_else(|| "wezterm".into(), std::path::PathBuf::into_os_string))
}

#[derive(Debug, Deserialize)]
pub struct WezTermPane {
    pub window_id: u64,
    pub tab_id: u64,
    pub pane_id: u64,
    pub title: String,
    pub tty_name: Option<String>,
}

/// # Errors
///
/// Will return `Err` if the output is not the JSON of `wezterm cli list`.
pub fn parse_wezterm_list(json: &str) -> Result<Vec<WezTermPane>> {
    serde_json::from_str(json).context("Unexpected output of `wezterm cli list`")
}

/// The `WezTerm` tabs whose pane runs on one of `ttys`, labelled with the tab's
/// position in its window (1-based) and the pane title.
#[must_use]
pub fn wezterm_tabs<S: BuildHasher>(panes: &[WezTermPane], ttys: &HashSet<String, S>) -> Vec<Tab> {
    let mut positions: HashMap<(u64, u64), usize> = HashMap::new();
    let mut per_window: HashMap<u64, usize> = HashMap::new();
    for pane in panes {
        positions
            .entry((pane.window_id, pane.tab_id))
            .or_insert_with(|| {
                let n = per_window.entry(pane.window_id).or_insert(0);
                *n += 1;
                *n
            });
    }

    panes
        .iter()
        .filter(|p| p.tty_name.as_ref().is_some_and(|t| ttys.contains(t)))
        .map(|p| Tab {
            label: format!("tab {}: {}", positions[&(p.window_id, p.tab_id)], p.title),
            target: JumpTarget::WezTermPane {
                tab_id: p.tab_id,
                pane_id: p.pane_id,
            },
        })
        .collect()
}

const GNOME_SEARCH: [&str; 6] = [
    "call",
    "--session",
    "--dest",
    "org.gnome.Terminal",
    "--object-path",
    "/org/gnome/Terminal/SearchProvider",
];

fn gdbus(method: &str, params: &[String]) -> Result<String> {
    let out = Command::new("gdbus")
        .args(GNOME_SEARCH)
        .arg("--method")
        .arg(format!("org.gnome.Shell.SearchProvider2.{method}"))
        .args(params)
        .output()
        .context("Failed to run gdbus")?;
    if !out.status.success() {
        bail!(
            "gdbus {method} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `GVariant` text of a string array, for gdbus.
fn gvariant_strings(items: &[&str]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|s| format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'")))
        .collect();
    format!("[{}]", quoted.join(", "))
}

/// A single-quoted `GVariant` string, with `\'` escapes.
const QUOTED: &str = r"'((?:[^'\\]|\\.)*)'";

static QUOTED_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(QUOTED).expect("valid regex"));
static META_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("'id': <{QUOTED}>")).expect("valid regex"));
static META_NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!("'name': <{QUOTED}>")).expect("valid regex"));

/// The ids in `(['a', 'b'],)`.
#[must_use]
pub fn parse_gdbus_ids(out: &str) -> Vec<String> {
    QUOTED_RE
        .captures_iter(out)
        .map(|c| c[1].to_string())
        .collect()
}

/// `(id, name)` of each entry of `([{'id': <'…'>, 'name': <'…'>, …}, …],)`.
#[must_use]
pub fn parse_gdbus_metas(out: &str) -> Vec<(String, String)> {
    out.split("}, {")
        .filter_map(|entry| {
            let id = META_ID_RE.captures(entry)?[1].to_string();
            let name = META_NAME_RE
                .captures(entry)
                .map_or_else(String::new, |c| c[1].replace("\\'", "'"));
            Some((id, name))
        })
        .collect()
}

/// GNOME Terminal tabs whose title matches `term`.
///
/// # Errors
///
/// Will return `Err` if GNOME Terminal's search provider cannot be called.
pub fn gnome_tabs(term: &str) -> Result<Vec<Tab>> {
    let ids = parse_gdbus_ids(&gdbus("GetInitialResultSet", &[gvariant_strings(&[term])])?);
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    let metas = parse_gdbus_metas(&gdbus("GetResultMetas", &[gvariant_strings(&id_refs)])?);

    Ok(metas
        .into_iter()
        .map(|(id, name)| Tab {
            label: name,
            target: JumpTarget::GnomeScreen(id),
        })
        .collect())
}

/// The tabs that hold a connection to a host.
///
/// # Errors
///
/// Will return `Err` if the terminal cannot be asked for its tabs.
pub fn tabs_for<S: BuildHasher>(
    kind: TerminalKind,
    ttys: &HashSet<String, S>,
    host_name: &str,
) -> Result<Vec<Tab>> {
    match kind {
        TerminalKind::WezTerm => {
            let out = wezterm()
                .args(["cli", "list", "--format", "json"])
                .output()
                .context("Failed to run wezterm")?;
            if !out.status.success() {
                bail!(
                    "wezterm cli list failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(wezterm_tabs(
                &parse_wezterm_list(&String::from_utf8_lossy(&out.stdout))?,
                ttys,
            ))
        }
        TerminalKind::Gnome => gnome_tabs(host_name),
        TerminalKind::Other => Ok(Vec::new()),
    }
}

/// Brings a tab to front.
///
/// # Errors
///
/// Will return `Err` if the terminal refuses or cannot be reached.
pub fn jump(target: &JumpTarget) -> Result<()> {
    match target {
        JumpTarget::WezTermPane { tab_id, pane_id } => {
            // `activate-pane` alone does not reliably switch to a pane in
            // another tab; `activate-tab` does, then the pane is focused
            // within its (possibly split) tab.
            for args in [
                ["cli", "activate-tab", "--tab-id", &tab_id.to_string()],
                ["cli", "activate-pane", "--pane-id", &pane_id.to_string()],
            ] {
                let status = wezterm()
                    .args(args)
                    .status()
                    .context("Failed to run wezterm")?;
                if !status.success() {
                    bail!("wezterm {} failed ({status})", args[1]);
                }
            }
        }
        JumpTarget::GnomeScreen(id) => {
            gdbus(
                "ActivateResult",
                &[id.clone(), "[]".to_string(), "0".to_string()],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn test_ssh_destination() {
        assert_eq!(
            ssh_destination(&args("dev-web")).as_deref(),
            Some("dev-web")
        );
        assert_eq!(
            ssh_destination(&args("-p 2222 admin@db.example.com uptime")).as_deref(),
            Some("db.example.com")
        );
        assert_eq!(
            ssh_destination(&args("-p2222 -tt acme-web-b")).as_deref(),
            Some("acme-web-b")
        );
        assert_eq!(
            ssh_destination(&args("-o ServerAliveInterval=60 -J jump zeta-web-g")).as_deref(),
            Some("zeta-web-g")
        );
        assert_eq!(
            ssh_destination(&args("-vAi ~/.ssh/key acme-web-g")).as_deref(),
            Some("acme-web-g")
        );
        assert_eq!(
            ssh_destination(&args("-- -odd-name")).as_deref(),
            Some("-odd-name")
        );
        assert_eq!(
            ssh_destination(&args("ssh://deploy@dev-web:2222")).as_deref(),
            Some("dev-web")
        );
        assert_eq!(ssh_destination(&args("-N -L 8080:localhost:80")), None);
    }

    #[test]
    fn test_ttys_for_matches_alias_or_hostname() {
        let connections = vec![
            Connection {
                destination: "dev-web".into(),
                tty: "/dev/pts/3".into(),
            },
            Connection {
                destination: "10.0.0.10".into(),
                tty: "/dev/pts/4".into(),
            },
            Connection {
                destination: "DEV-WEB".into(),
                tty: "/dev/pts/3".into(),
            },
            Connection {
                destination: "acme-web-b".into(),
                tty: "/dev/pts/7".into(),
            },
        ];
        let ttys = ttys_for(&connections, "dev-web", "10.0.0.10");
        assert_eq!(ttys.len(), 2);
        assert!(ttys.contains("/dev/pts/3") && ttys.contains("/dev/pts/4"));
        assert_eq!(
            ttys_for(&connections, "zeta-web-g", "zeta-web-g"),
            HashSet::new()
        );
    }

    #[test]
    fn test_wezterm_tabs_by_tty_with_position_in_window() {
        let json = r#"[
          {"window_id":0,"tab_id":0,"pane_id":0,"title":"shell","tty_name":"/dev/pts/2"},
          {"window_id":0,"tab_id":4,"pane_id":7,"title":"deploy@dev-web: ~","tty_name":"/dev/pts/5"},
          {"window_id":0,"tab_id":9,"pane_id":8,"title":"left","tty_name":"/dev/pts/6"},
          {"window_id":0,"tab_id":9,"pane_id":9,"title":"right","tty_name":"/dev/pts/8"},
          {"window_id":1,"tab_id":11,"pane_id":12,"title":"other window","tty_name":"/dev/pts/9"}
        ]"#;
        let panes = parse_wezterm_list(json).unwrap();
        let ttys: HashSet<String> = ["/dev/pts/5", "/dev/pts/8", "/dev/pts/9"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();

        assert_eq!(
            wezterm_tabs(&panes, &ttys),
            vec![
                Tab {
                    label: "tab 2: deploy@dev-web: ~".into(),
                    target: JumpTarget::WezTermPane {
                        tab_id: 4,
                        pane_id: 7
                    }
                },
                Tab {
                    label: "tab 3: right".into(),
                    target: JumpTarget::WezTermPane {
                        tab_id: 9,
                        pane_id: 9
                    }
                },
                Tab {
                    label: "tab 1: other window".into(),
                    target: JumpTarget::WezTermPane {
                        tab_id: 11,
                        pane_id: 12
                    }
                },
            ]
        );
    }

    #[test]
    fn test_parse_gdbus_output() {
        assert_eq!(
            parse_gdbus_ids("(['3f0c', 'a1b2'],)\n"),
            vec!["3f0c".to_string(), "a1b2".to_string()]
        );
        assert_eq!(parse_gdbus_ids("([],)\n"), Vec::<String>::new());

        let metas = "([{'id': <'3f0c'>, 'name': <'deploy@dev-web: ~'>, 'description': <''>}, {'name': <'it\\'s acme'>, 'id': <'a1b2'>}],)\n";
        assert_eq!(
            parse_gdbus_metas(metas),
            vec![
                ("3f0c".to_string(), "deploy@dev-web: ~".to_string()),
                ("a1b2".to_string(), "it's acme".to_string()),
            ]
        );
    }

    #[test]
    fn test_gvariant_strings_escapes_quotes() {
        assert_eq!(gvariant_strings(&["dev-web"]), "['dev-web']");
        assert_eq!(gvariant_strings(&["a'b"]), "['a\\'b']");
    }
}
