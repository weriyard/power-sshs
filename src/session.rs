//! One tmux session per host: open it, return to it, and tell which hosts
//! have one alive.
//!
//! The session is named after the host. Its command is the rendered command
//! template (by default `ssh "<host>"`), so the session ends when SSH does.

use std::collections::HashSet;
use std::process::{Command, ExitStatus};

use anyhow::{anyhow, Result};

/// Marker shown in front of a host that has a live session.
pub const LIVE_MARKER: &str = "● ";

/// The tmux session name for a host. tmux does not accept `.` or `:` in a
/// session name, so they become `_`.
#[must_use]
pub fn session_name(host_name: &str) -> String {
    host_name.replace(['.', ':'], "_")
}

/// tmux target matching the session name exactly rather than by prefix.
fn exact_target(session: &str) -> String {
    format!("={session}")
}

/// Whether this process runs inside a tmux client.
#[must_use]
pub fn inside_tmux() -> bool {
    std::env::var_os("TMUX").is_some_and(|v| !v.is_empty())
}

/// The tmux invocations that open or return to `session`.
///
/// Outside tmux one `new-session -A` attaches to the session, creating it
/// first when needed, and blocks until the user detaches or the session ends.
/// Inside tmux the session is created detached when missing and the current
/// client switches to it, so tmux is never nested.
#[must_use]
pub fn open_commands(session: &str, command: &str, inside: bool, exists: bool) -> Vec<Vec<String>> {
    let new_session = |extra: &str| {
        vec![
            "new-session".to_string(),
            extra.to_string(),
            "-s".to_string(),
            session.to_string(),
            command.to_string(),
        ]
    };

    if !inside {
        return vec![new_session("-A")];
    }

    let mut commands = Vec::new();
    if !exists {
        commands.push(new_session("-d"));
    }
    commands.push(vec![
        "switch-client".to_string(),
        "-t".to_string(),
        exact_target(session),
    ]);
    commands
}

/// Parses `tmux list-sessions -F '#{session_name}'`.
#[must_use]
pub fn parse_sessions(stdout: &str) -> HashSet<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Names of the live tmux sessions. Empty when tmux is not installed or no
/// server runs — both mean there is no session to show.
#[must_use]
pub fn live_sessions() -> HashSet<String> {
    match Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
    {
        Ok(out) if out.status.success() => parse_sessions(&String::from_utf8_lossy(&out.stdout)),
        _ => HashSet::new(),
    }
}

/// Opens or returns to the session of a host, running `command` in it when
/// the session has to be created.
///
/// # Errors
///
/// Will return `Err` if tmux cannot be started or one of its commands fails.
pub fn open(host_name: &str, command: &str) -> Result<ExitStatus> {
    let session = session_name(host_name);
    let inside = inside_tmux();
    let exists = live_sessions().contains(&session);

    let mut last = None;
    for args in open_commands(&session, command, inside, exists) {
        let status = Command::new("tmux")
            .args(&args)
            .status()
            .map_err(|e| anyhow!("Failed to run tmux: {e}. Is tmux installed?"))?;
        if !status.success() && inside {
            anyhow::bail!("tmux {} failed with {status}", args[0]);
        }
        last = Some(status);
    }

    last.ok_or_else(|| anyhow!("No tmux command to run"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_name_replaces_characters_tmux_rejects() {
        assert_eq!(session_name("dev-web"), "dev-web");
        assert_eq!(session_name("web.example.com"), "web_example_com");
        assert_eq!(session_name("host:2222"), "host_2222");
    }

    #[test]
    fn test_open_outside_tmux_attaches_or_creates() {
        let commands = open_commands("acme-web-b", "ssh \"acme-web-b\"", false, true);
        assert_eq!(
            commands,
            vec![vec![
                "new-session",
                "-A",
                "-s",
                "acme-web-b",
                "ssh \"acme-web-b\""
            ]]
        );
        // The same command whether or not the session already exists.
        assert_eq!(
            open_commands("acme-web-b", "ssh \"acme-web-b\"", false, false),
            commands
        );
    }

    #[test]
    fn test_open_inside_tmux_creates_detached_then_switches() {
        assert_eq!(
            open_commands("dev-web", "ssh dev-web", true, false),
            vec![
                vec!["new-session", "-d", "-s", "dev-web", "ssh dev-web"],
                vec!["switch-client", "-t", "=dev-web"],
            ]
        );
    }

    #[test]
    fn test_open_inside_tmux_switches_to_existing_session() {
        assert_eq!(
            open_commands("dev-web", "ssh dev-web", true, true),
            vec![vec!["switch-client", "-t", "=dev-web"]]
        );
    }

    #[test]
    fn test_parse_sessions() {
        let sessions = parse_sessions("dev-web\nacme-web-b\n\n");
        assert_eq!(sessions.len(), 2);
        assert!(sessions.contains("dev-web"));
        assert!(sessions.contains("acme-web-b"));
        assert!(parse_sessions("").is_empty());
    }
}
