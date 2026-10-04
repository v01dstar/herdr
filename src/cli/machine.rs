use std::io::IsTerminal as _;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, SetAttribute};
use crossterm::{cursor, execute, terminal};
use serde::Serialize;

use crate::client::endpoint::{EndpointCatalog, ProfileId, MAX_LABEL_BYTES};

const HELP: &str = "Usage:
  herdr machine list [--json]
  herdr machine status [<label-or-id>] [--json]
  herdr machine reconnect <label-or-id>
  herdr machine add <ssh-target> [--label <label>] [--remote-session <name>]
  herdr machine rename <profile-id> --label <label>
  herdr machine remove <profile-id> [--delete-machine]
  herdr machine enable <profile-id>
  herdr machine disable <profile-id>

Add prepares the remote Herdr installation and starts its server before saving.
Missing or incompatible installations require approval in an interactive terminal.
Changes apply automatically to open local Herdr clients.
Removing or disabling an SSH machine leaves its remote sessions running.
Saved machines contain only a label, SSH target, explicit Herdr session, and enabled state.
SSH credentials and key material remain owned by OpenSSH.

hangar machines are not saved: Herdr lists every machine of the hangar account you are
signed in to (shared with the hangar CLI) and connects the running ones through the
hangar gateway with short-lived certificates. Machines created or deleted elsewhere
appear and disappear automatically; `list` refreshes the list first. For a hangar
machine, rename sets the name Herdr shows, disable hides it from the sidebar (it is not
connected) and enable shows it again. It cannot be removed from Herdr alone: remove
--delete-machine deletes it in hangar, with its disks and snapshots, which cannot be
undone.";

#[derive(Serialize)]
struct MachineListRow {
    id: String,
    label: String,
    target: String,
    session: String,
    enabled: bool,
    selected: bool,
    /// `ssh` (saved here) or `hangar` (listed from the hangar account).
    source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    server: Option<String>,
    /// The hangar machine state.
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'static str>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    hidden: bool,
    /// Why a hangar list may be outdated (`offline`, `signed out`, `not synced yet`).
    #[serde(skip_serializing_if = "Option::is_none")]
    sync: Option<&'static str>,
}

pub(super) fn run_machine_command(args: &[String]) -> std::io::Result<i32> {
    match args.first().map(String::as_str) {
        Some("list") => list(&args[1..]),
        Some("status") => status(&args[1..]),
        Some("reconnect") => reconnect(&args[1..]),
        Some("add") => add(&args[1..]),
        Some("rename") => rename(&args[1..]),
        Some("remove") => remove(&args[1..]),
        Some("enable") => set_enabled(&args[1..], true),
        Some("disable") => set_enabled(&args[1..], false),
        Some("help" | "--help" | "-h") => {
            println!("{HELP}");
            Ok(0)
        }
        _ => {
            eprintln!("{HELP}");
            Ok(2)
        }
    }
}

fn list(args: &[String]) -> std::io::Result<i32> {
    let json = match args {
        [] => false,
        [flag] if flag == "--json" => true,
        _ => {
            eprintln!("usage: herdr machine list [--json]");
            return Ok(2);
        }
    };
    // The hangar account is the truth for hangar machines; a failed fetch keeps the
    // last list and marks it.
    let report = crate::client::locations::sync::sync_now();
    for error in &report.errors {
        eprintln!("warning: showing the last synced hangar machines: {error}");
    }
    let remotes = load_remotes()?;
    let selected = crate::client::locations::effective_catalog()
        .ok()
        .and_then(|catalog| catalog.selected_profile);
    let rows = list_rows(&remotes, selected.as_ref());
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(std::io::Error::other)?
        );
        return Ok(0);
    }
    if rows.is_empty() {
        println!("No saved SSH machines or hangar machines.");
        return Ok(0);
    }
    for row in rows {
        let state = match row.state {
            None if row.enabled => "enabled".to_owned(),
            None => "disabled".to_owned(),
            Some(state) => {
                let mut text = format!("hangar {state}");
                if row.hidden {
                    text.push_str(", hidden");
                }
                if let Some(note) = row.sync {
                    text.push_str(&format!(", {note}"));
                }
                text
            }
        };
        println!(
            "{}\t{}\t{}\t{}\t{}",
            row.id, row.label, row.target, row.session, state
        );
    }
    Ok(0)
}

fn list_rows(
    remotes: &crate::client::locations::Remotes,
    selected: Option<&ProfileId>,
) -> Vec<MachineListRow> {
    let ssh = remotes.ssh.iter().map(|profile| MachineListRow {
        id: profile.id.to_string(),
        label: profile.label.clone(),
        target: profile.target.clone(),
        session: profile.session.clone(),
        enabled: profile.enabled,
        selected: selected == Some(&profile.id),
        source: "ssh",
        server: None,
        state: None,
        hidden: false,
        sync: None,
    });
    let hangar = remotes.hangar.iter().map(|remote| MachineListRow {
        id: remote.profile.id.to_string(),
        label: remote.profile.label.clone(),
        target: remote.profile.target.clone(),
        session: remote.profile.session.clone(),
        enabled: remote.profile.enabled,
        selected: selected == Some(&remote.profile.id),
        source: "hangar",
        server: Some(remote.binding.server.clone()),
        state: Some(remote.state.as_str()),
        hidden: remote.hidden,
        sync: remote.sync.note(),
    });
    ssh.chain(hangar).collect()
}

#[derive(Serialize)]
struct MachineStatusRow<'a> {
    id: &'a str,
    label: &'a str,
    status: &'static str,
    error: Option<String>,
}

fn status(args: &[String]) -> std::io::Result<i32> {
    let mut json = false;
    let mut selector = None;
    for arg in args {
        if arg == "--json" && !json {
            json = true;
        } else if !arg.starts_with('-') && selector.is_none() {
            selector = Some(arg.as_str());
        } else {
            eprintln!("usage: herdr machine status [<label-or-id>] [--json]");
            return Ok(2);
        }
    }
    let all = load_remotes()?.profiles(true);
    let profiles = match selector {
        Some(selector) => match super::target::resolve_machine(&all, selector) {
            Ok(profile) => vec![profile],
            Err(error) => {
                eprintln!("{error}");
                return Ok(2);
            }
        },
        None => all.iter().collect(),
    };
    let rows = profiles
        .into_iter()
        .map(|profile| {
            let (status, error) = if !profile.enabled {
                ("disabled", None)
            } else {
                match crate::remote::check_saved_ssh(&profile.target, &profile.session) {
                    Ok(()) => ("reachable", None),
                    Err(error) => {
                        let message = error.to_string();
                        let status = if crate::remote::ssh_error_requires_authentication(&message) {
                            "auth required"
                        } else {
                            "error"
                        };
                        (status, Some(message))
                    }
                }
            };
            MachineStatusRow {
                id: profile.id.as_str(),
                label: &profile.label,
                status,
                error,
            }
        })
        .collect::<Vec<_>>();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(std::io::Error::other)?
        );
    } else {
        for row in &rows {
            println!("{}\t{}\t{}", row.id, row.label, row.status);
            if let Some(error) = &row.error {
                println!("  {}", error.escape_debug());
            }
        }
        if rows.is_empty() {
            println!("No saved SSH machines.");
        }
    }
    Ok(i32::from(rows.iter().any(|row| row.error.is_some())))
}

fn reconnect(args: &[String]) -> std::io::Result<i32> {
    use std::io::IsTerminal;
    let [selector] = args else {
        eprintln!("usage: herdr machine reconnect <label-or-id>");
        return Ok(2);
    };
    let all = load_remotes()?.profiles(true);
    let profile = match super::target::resolve_machine(&all, selector) {
        Ok(profile) => profile,
        Err(error) => {
            eprintln!("{error}");
            return Ok(2);
        }
    };
    if !std::io::stdin().is_terminal() {
        eprintln!("reconnect requires an interactive terminal; use herdr machine status for noninteractive checks");
        return Ok(2);
    }
    let mut authentication = crate::remote::ssh_authentication_command(&profile.target)?;
    if !authentication.command.status()?.success() {
        eprintln!("SSH authentication failed; the saved machine was not changed.");
        return Ok(1);
    }
    crate::remote::check_saved_ssh(&profile.target, &profile.session)?;
    println!(
        "Machine {} is reachable. Open Herdr clients retry within 30 seconds.",
        profile.id
    );
    Ok(0)
}

#[derive(Debug, PartialEq, Eq)]
struct AddArgs {
    target: String,
    label: Option<String>,
    session: Option<String>,
    hangar: bool,
}

fn parse_add_args(args: &[String]) -> Result<AddArgs, String> {
    let args = super::expand_equals_args(args, &["--label", "--remote-session"]);
    let mut target = None;
    let mut label = None;
    let mut session = None;
    let mut hangar = false;
    let mut index = 0;
    while index < args.len() {
        let (name, value) = match args[index].as_str() {
            "--hangar" if !hangar => {
                hangar = true;
                index += 1;
                continue;
            }
            "--label" | "--remote-session" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(format!("missing value for {}", args[index]));
                };
                index += 2;
                (args[index - 2].as_str(), value.clone())
            }
            positional if !positional.starts_with('-') && target.is_none() => {
                target = Some(positional.to_owned());
                index += 1;
                continue;
            }
            unknown => {
                return Err(format!("unknown machine add option: {unknown}"));
            }
        };
        match name {
            "--label" if label.is_none() => label = Some(value),
            "--remote-session" if session.is_none() => session = Some(value),
            "--remote-session" => {
                return Err("--remote-session can only be specified once".into());
            }
            "--label" => {
                return Err("--label can only be specified once".into());
            }
            _ => unreachable!("validated machine add option"),
        }
    }
    let target = target.ok_or_else(|| {
        "usage: herdr machine add <ssh-target> [--label <label>] [--remote-session <name>]"
            .to_owned()
    })?;
    Ok(AddArgs {
        target,
        label,
        session,
        hangar,
    })
}

/// Names the machine after the SSH host, plus the session when it is not the default.
fn default_label(target: &str, session: &str) -> String {
    let url = target
        .strip_prefix("ssh://")
        .map(|authority| authority.trim_end_matches('/'));
    let authority = url.unwrap_or(target);
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = match (url, host.strip_prefix('[')) {
        (Some(_), Some(bracketed)) => bracketed.split_once(']').map_or(host, |(ip, _)| ip),
        (Some(_), None) => host.split_once(':').map_or(host, |(host, _)| host),
        (None, _) => host,
    };
    if session == crate::session::DEFAULT_SESSION_NAME {
        host.to_owned()
    } else {
        format!("{host}/{session}")
    }
}

fn check_default_label(catalog: &EndpointCatalog, label: &str) -> Result<(), String> {
    if label.len() > MAX_LABEL_BYTES {
        return Err(format!(
            "default machine name '{label}' is longer than {MAX_LABEL_BYTES} bytes; pass --label to choose a name"
        ));
    }
    if catalog.ssh.iter().any(|profile| profile.label == label) {
        return Err(format!(
            "a machine named '{label}' already exists; pass --label to choose another name"
        ));
    }
    Ok(())
}

fn add(args: &[String]) -> std::io::Result<i32> {
    let AddArgs {
        target,
        label,
        session,
        hangar,
    } = match parse_add_args(args) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            return Ok(2);
        }
    };
    if hangar {
        return add_hangar(&target, label, session);
    }
    let mut setup = None;
    let session =
        if session.is_none() && std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
            let discovered = (|| {
                let connection = crate::remote::SavedSshSetup::connect(&target)?;
                let sessions = connection.running_sessions()?;
                let session = select_remote_session(&sessions, &target)?;
                setup = Some(connection);
                Ok::<_, std::io::Error>(session)
            })();
            match discovered {
                Ok(session) => session,
                Err(error) => {
                    eprintln!("error: {error}; machine was not saved");
                    crate::remote::print_saved_ssh_error_hint(&error, &target);
                    return Ok(1);
                }
            }
        } else {
            session.unwrap_or_else(|| crate::session::DEFAULT_SESSION_NAME.to_owned())
        };
    let label_is_default = label.is_none();
    let label = label.unwrap_or_else(|| default_label(&target, &session));
    let mut catalog = load_catalog()?;
    if label_is_default {
        if let Err(error) = check_default_label(&catalog, &label) {
            eprintln!("error: {error}");
            return Ok(2);
        }
    }
    match catalog.add_ssh(label.clone(), &target, session.clone()) {
        Ok(_) => {}
        Err(error) => {
            eprintln!("error: {error}");
            return Ok(2);
        }
    }
    let metadata = match setup
        .map(Ok)
        .unwrap_or_else(|| crate::remote::SavedSshSetup::connect(&target))
        .and_then(|setup| setup.prepare(&session))
    {
        Ok(metadata) => metadata,
        Err(error) => {
            eprintln!("error: {error}; machine was not saved");
            crate::remote::print_saved_ssh_error_hint(&error, &target);
            return Ok(1);
        }
    };
    // Setup can wait for human approval. Do not overwrite catalog edits made meanwhile.
    let mut catalog = load_catalog().map_err(|error| {
        std::io::Error::other(format!(
            "remote prepared, but machine was not saved: {error}"
        ))
    })?;
    if label_is_default {
        if let Err(error) = check_default_label(&catalog, &label) {
            eprintln!("error: {error}; machine was not saved");
            return Ok(2);
        }
    }
    let id = match catalog.add_ssh(label, &target, &session) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("error: {error}");
            return Ok(2);
        }
    };
    store_catalog(&catalog).map_err(|error| {
        std::io::Error::other(format!(
            "remote prepared, but machine was not saved: {error}"
        ))
    })?;
    if let Some(metadata) = metadata {
        crate::client::endpoint::SshMetadataCache::new(id.as_str(), &target, &session)?
            .store(&metadata);
    }
    println!("Saved SSH machine {id}. Remote server is ready.");
    println!("Open Herdr clients connect automatically.");
    Ok(0)
}

/// hangar machines are listed from the account, not added: this checks the machine is
/// listed and says where it is.
fn add_hangar(
    selector: &str,
    label: Option<String>,
    session: Option<String>,
) -> std::io::Result<i32> {
    use crate::client::locations::hangar as machines;
    if label.is_some() || session.is_some() {
        eprintln!("note: hangar machines are not saved, so --label and --remote-session are ignored; use `herdr machine rename` to rename one.");
    }
    let server = crate::hangar::default_server();
    let machine = match machines::resolve_machine(&server, selector) {
        Ok(machine) => machine,
        Err(error) => {
            eprintln!("error: {error}");
            return Ok(1);
        }
    };
    let report =
        crate::client::locations::sync::sync_with(std::slice::from_ref(&server), &|server| {
            machines::list_machines(server)
        });
    for error in &report.errors {
        eprintln!("warning: {error}");
    }
    let id = load_remotes()?
        .machine(&server, &machine.id)
        .map(|remote| remote.profile.id.to_string());
    println!(
        "hangar machines appear automatically; nothing to add. {} ({}) is listed{}.",
        machine.name,
        machine.state.as_str(),
        id.map(|id| format!(" as {id}")).unwrap_or_default()
    );
    Ok(0)
}

fn select_remote_session(sessions: &[String], target: &str) -> std::io::Result<String> {
    match sessions {
        [] => return Ok(crate::session::DEFAULT_SESSION_NAME.to_owned()),
        [session] => return Ok(session.clone()),
        _ => {}
    }

    let _raw_mode = RawModeGuard::enable()?;
    let mut output = std::io::stderr();
    let mut selected = 0;
    render_remote_session_picker(&mut output, target, sessions, selected, false)?;
    loop {
        let Event::Key(key) = crossterm::event::read()? else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        match key.code {
            KeyCode::Up => selected = selected.checked_sub(1).unwrap_or(sessions.len() - 1),
            KeyCode::Down => selected = (selected + 1) % sessions.len(),
            KeyCode::Enter => return Ok(sessions[selected].clone()),
            KeyCode::Esc => break,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
            _ => continue,
        }
        render_remote_session_picker(&mut output, target, sessions, selected, true)?;
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::Interrupted,
        "remote session selection cancelled",
    ))
}

struct RawModeGuard;

impl RawModeGuard {
    fn enable() -> std::io::Result<Self> {
        terminal::enable_raw_mode()?;
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
    }
}

fn render_remote_session_picker(
    output: &mut impl std::io::Write,
    target: &str,
    sessions: &[String],
    selected: usize,
    redraw: bool,
) -> std::io::Result<()> {
    if redraw {
        execute!(output, cursor::MoveUp((sessions.len() + 2) as u16))?;
    }
    execute!(output, terminal::Clear(terminal::ClearType::CurrentLine))?;
    write!(output, "Running sessions on {target}:\r\n")?;
    for (index, session) in sessions.iter().enumerate() {
        execute!(output, terminal::Clear(terminal::ClearType::CurrentLine))?;
        if index == selected {
            execute!(output, SetAttribute(Attribute::Bold))?;
            write!(output, "> {session}")?;
            execute!(output, SetAttribute(Attribute::Reset))?;
            write!(output, "\r\n")?;
        } else {
            write!(output, "  {session}\r\n")?;
        }
    }
    execute!(output, terminal::Clear(terminal::ClearType::CurrentLine))?;
    write!(output, "↑/↓ select · Enter confirm · Esc cancel\r\n")?;
    output.flush()
}

fn rename(args: &[String]) -> std::io::Result<i32> {
    let args = super::expand_equals_args(args, &["--label"]);
    let [raw_id, flag, label] = args.as_slice() else {
        eprintln!("usage: herdr machine rename <profile-id> --label <label>");
        return Ok(2);
    };
    if flag != "--label" {
        eprintln!("usage: herdr machine rename <profile-id> --label <label>");
        return Ok(2);
    }
    let id = match ProfileId::parse(raw_id.clone()) {
        Ok(id) => id,
        Err(error) => {
            eprintln!("error: {error}");
            return Ok(2);
        }
    };
    let remotes = load_remotes()?;
    if let Some(remote) = remotes.hangar_remote(&id) {
        return match crate::client::locations::edit_machine_prefs(
            &remote.binding,
            Some(label),
            None,
            None,
        ) {
            Ok(()) => {
                println!(
                    "Renamed hangar machine {} to {} in Herdr (hangar keeps its name).",
                    remote.binding.machine_name,
                    label.trim()
                );
                Ok(0)
            }
            Err(error) => {
                eprintln!("error: {error}");
                Ok(2)
            }
        };
    }
    let mut catalog = load_catalog()?;
    match catalog.rename_ssh(&id, label) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("machine profile {id} was not found");
            return Ok(1);
        }
        Err(error) => {
            eprintln!("error: {error}");
            return Ok(2);
        }
    }
    store_catalog(&catalog)?;
    println!("Renamed SSH machine {id}.");
    Ok(0)
}

const REMOVE_USAGE: &str = "usage: herdr machine remove <profile-id> [--delete-machine]";

/// Splits `--delete-machine` from the profile ID argument.
fn parse_remove_args(args: &[String]) -> (Vec<String>, bool) {
    let delete_machine = args.iter().any(|arg| arg == "--delete-machine");
    let rest = args
        .iter()
        .filter(|arg| *arg != "--delete-machine")
        .cloned()
        .collect::<Vec<_>>();
    (rest, delete_machine)
}

/// A CLI cannot show the confirmation dialog, so deleting a hangar machine needs the
/// explicit flag, and the flag is refused where there is no machine to delete.
fn check_remove(machine_name: Option<&str>, delete_machine: bool) -> Result<(), String> {
    match (machine_name, delete_machine) {
        (Some(name), false) => Err(format!(
            "{} Pass --delete-machine to delete it on hangar ({}), or run `herdr machine disable <profile-id>` to hide it.",
            crate::client::locations::hangar_remove_refusal(name),
            crate::client::locations::delete_consequences()
        )),
        (None, true) => Err(
            "--delete-machine applies only to hangar machines; this is an SSH machine".into(),
        ),
        _ => Ok(()),
    }
}

fn remove(args: &[String]) -> std::io::Result<i32> {
    let (args, delete_machine) = parse_remove_args(args);
    let Some(id) = one_profile_id(&args, REMOVE_USAGE)? else {
        return Ok(2);
    };
    let remotes = load_remotes()?;
    let hangar = remotes.hangar_remote(&id);
    let known = hangar.is_some() || remotes.ssh.iter().any(|profile| profile.id == id);
    if known {
        if let Err(error) = check_remove(
            hangar.map(|remote| remote.binding.machine_name.as_str()),
            delete_machine,
        ) {
            eprintln!("error: {error}");
            return Ok(2);
        }
    }
    if let Some(remote) = hangar {
        return match crate::client::locations::delete_remote(
            &remote.profile,
            &remote.options(),
            &mut |step| eprintln!("{step}"),
        ) {
            Ok(message) => {
                println!("{message}");
                Ok(0)
            }
            Err(error) => {
                eprintln!("error: {error}");
                Ok(1)
            }
        };
    }
    let mut catalog = load_catalog()?;
    let previous_selection = catalog.selected_profile.clone();
    let metadata_cache = catalog
        .ssh
        .iter()
        .find(|profile| profile.id == id)
        .map(|profile| {
            crate::client::endpoint::SshMetadataCache::new(
                id.as_str(),
                &profile.target,
                &profile.session,
            )
        })
        .transpose()?;
    if !catalog.remove_ssh(&id) {
        eprintln!("machine profile {id} was not found");
        return Ok(1);
    }
    store_catalog(&catalog)?;
    // The default directory belongs to the profile.
    if let Err(error) = crate::client::locations::operation_lock()
        .and_then(|_guard| crate::client::locations::remove_binding(&id))
    {
        eprintln!("warning: could not remove remote settings for {id}: {error}");
    }
    if let Some(cache) = metadata_cache {
        cache.invalidate();
    }
    if catalog.selected_profile != previous_selection {
        catalog.store_selection().map_err(std::io::Error::other)?;
    }
    println!("Removed SSH machine {id}.");
    Ok(0)
}

fn set_enabled(args: &[String], enabled: bool) -> std::io::Result<i32> {
    let action = if enabled { "enable" } else { "disable" };
    let usage = format!("usage: herdr machine {action} <profile-id>");
    let Some(id) = one_profile_id(args, &usage)? else {
        return Ok(2);
    };
    if let Some(remote) = load_remotes()?.hangar_remote(&id) {
        return match crate::client::locations::set_hidden(&remote.binding, !enabled) {
            Ok(()) => {
                println!(
                    "{} hangar machine {} {} the sidebar.",
                    if enabled { "Showing" } else { "Hid" },
                    remote.profile.label,
                    if enabled { "in" } else { "from" }
                );
                Ok(0)
            }
            Err(error) => {
                eprintln!("error: {error}");
                Ok(1)
            }
        };
    }
    let mut catalog = load_catalog()?;
    let previous_selection = catalog.selected_profile.clone();
    if !catalog.set_enabled(&id, enabled) {
        eprintln!("machine profile {id} was not found");
        return Ok(1);
    }
    store_catalog(&catalog)?;
    if catalog.selected_profile != previous_selection {
        catalog.store_selection().map_err(std::io::Error::other)?;
    }
    println!(
        "{} SSH machine {id}.",
        if enabled { "Enabled" } else { "Disabled" }
    );
    Ok(0)
}

fn one_profile_id(args: &[String], usage: &str) -> std::io::Result<Option<ProfileId>> {
    let [raw] = args else {
        eprintln!("{usage}");
        return Ok(None);
    };
    match ProfileId::parse(raw.clone()) {
        Ok(id) => Ok(Some(id)),
        Err(error) => {
            eprintln!("error: {error}");
            Ok(None)
        }
    }
}

fn load_remotes() -> std::io::Result<crate::client::locations::Remotes> {
    crate::client::locations::Remotes::load().map_err(std::io::Error::other)
}

fn load_catalog() -> std::io::Result<EndpointCatalog> {
    EndpointCatalog::load().map_err(std::io::Error::other)
}

fn store_catalog(catalog: &EndpointCatalog) -> std::io::Result<()> {
    catalog.store_profiles().map_err(std::io::Error::other)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_parser_preserves_values_across_argument_orders() {
        for (args, session) in [
            (vec!["--label", "coder", "workstation.coder"], None),
            (vec!["workstation.coder", "--label", "coder"], None),
            (
                vec![
                    "--remote-session",
                    "agents",
                    "workstation.coder",
                    "--label",
                    "coder",
                ],
                Some("agents"),
            ),
            (
                vec![
                    "--label=coder",
                    "--remote-session=agents",
                    "workstation.coder",
                ],
                Some("agents"),
            ),
        ] {
            let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert_eq!(
                parse_add_args(&args).unwrap(),
                AddArgs {
                    target: "workstation.coder".into(),
                    label: Some("coder".into()),
                    session: session.map(str::to_owned),
                    hangar: false,
                },
                "{args:?}"
            );
        }
    }

    #[test]
    fn add_parser_accepts_hangar_once() {
        let args = ["box", "--hangar"].map(str::to_owned);
        assert!(parse_add_args(&args).unwrap().hangar);
        let twice = ["box", "--hangar", "--hangar"].map(str::to_owned);
        assert!(parse_add_args(&twice).is_err());
    }

    #[test]
    fn add_parser_leaves_label_unset_without_flag() {
        let parsed = parse_add_args(&["workstation.coder".to_owned()]).unwrap();
        assert_eq!(parsed.label, None);
        assert_eq!(parsed.session, None);
    }

    #[test]
    fn default_label_uses_ssh_host_and_non_default_session() {
        for (target, session, label) in [
            ("workbox", "default", "workbox"),
            ("dev@workbox", "default", "workbox"),
            ("workbox", "agents", "workbox/agents"),
            ("ssh://workbox", "default", "workbox"),
            ("ssh://dev@workbox:2222", "default", "workbox"),
            ("ssh://dev@[::1]:2222", "agents", "::1/agents"),
            ("ssh://dev@workbox/", "default", "workbox"),
            ("ssh://dev@workbox:2222/", "agents", "workbox/agents"),
        ] {
            assert_eq!(default_label(target, session), label, "{target} {session}");
        }
    }

    #[test]
    fn default_label_must_be_unique_and_fit() {
        let mut catalog = EndpointCatalog::default();
        catalog.add_ssh("workbox", "workbox", "default").unwrap();

        assert!(check_default_label(&catalog, "workbox/agents").is_ok());
        let duplicate = check_default_label(&catalog, "workbox").unwrap_err();
        assert!(duplicate.contains("--label"), "{duplicate}");
        let long = check_default_label(&catalog, &"h".repeat(MAX_LABEL_BYTES + 1)).unwrap_err();
        assert!(long.contains("--label"), "{long}");
    }

    #[test]
    fn add_parser_rejects_incomplete_duplicate_and_extra_arguments() {
        for args in [
            vec![],
            vec!["--label", "coder"],
            vec!["workstation.coder", "--label"],
            vec!["workstation.coder", "--label", "coder", "--remote-session"],
            vec!["--label", "coder", "--label", "other", "workstation.coder"],
            vec![
                "workstation.coder",
                "--label",
                "coder",
                "--remote-session",
                "a",
                "--remote-session",
                "b",
            ],
            vec!["--label", "coder", "workstation.coder", "other-host"],
            vec!["--unknown", "workstation.coder", "--label", "coder"],
            vec!["--label", "--remote-session", "agents", "workstation.coder"],
        ] {
            let args = args.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert!(parse_add_args(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn removing_a_hangar_machine_requires_the_delete_flag_and_explains_hiding() {
        let refused = check_remove(Some("box"), false).unwrap_err();
        assert!(refused.contains("--delete-machine"), "{refused}");
        assert!(refused.contains("'box'"), "{refused}");
        assert!(refused.contains("permanently deleted"), "{refused}");
        assert!(refused.contains("herdr machine disable"), "{refused}");
        assert!(refused.contains("Hide from sidebar"), "{refused}");
        assert!(check_remove(Some("box"), true).is_ok());
        assert!(check_remove(None, false).is_ok());
        assert!(check_remove(None, true)
            .unwrap_err()
            .contains("only to hangar"));
        let (rest, flag) = parse_remove_args(&["--delete-machine".into(), "abc".into()]);
        assert!(flag);
        assert_eq!(rest, ["abc"]);
    }

    #[test]
    fn list_marks_hangar_machines_with_source_state_and_sync_status() {
        use crate::client::locations::sync::{CachedMachine, MachineCache, SyncStatus};
        use crate::client::locations::{LocationPreferences, Remotes};
        let ssh =
            crate::client::endpoint::SavedSshEndpoint::new("plain", "workbox", "default").unwrap();
        let mut cache = MachineCache::default();
        let entry = cache
            .servers
            .entry("https://hangar.test".into())
            .or_default();
        entry.status = SyncStatus::Unreachable;
        entry.machines.push(CachedMachine {
            id: "m_agqp6jaaa6kqkitog6zzqzdfhy".into(),
            name: "box".into(),
            state: crate::hangar::api::MachineState::Stopped,
            fence_until_ms: 0,
        });
        let mut prefs = LocationPreferences::default();
        prefs
            .edit_machine(
                "https://hangar.test",
                "m_agqp6jaaa6kqkitog6zzqzdfhy",
                |entry| entry.hidden = true,
            )
            .unwrap();
        let remotes = Remotes::build(vec![ssh.clone()], prefs, &cache, 0);
        let rows = list_rows(&remotes, Some(&ssh.id));
        let value = serde_json::to_value(&rows).unwrap();
        assert_eq!(value[0]["source"], "ssh");
        assert_eq!(value[0]["selected"], true);
        assert!(value[0].get("state").is_none());
        assert_eq!(value[1]["source"], "hangar");
        assert_eq!(value[1]["state"], "stopped");
        assert_eq!(value[1]["hidden"], true);
        assert_eq!(value[1]["sync"], "offline");
        assert_eq!(value[1]["enabled"], false);
        assert_eq!(value[1]["target"], "hangar-m_agqp6jaaa6kqkitog6zzqzdfhy");
    }

    #[test]
    fn profile_id_parser_rejects_target_text() {
        assert!(one_profile_id(&["build.example".into()], "usage")
            .unwrap()
            .is_none());
    }

    #[test]
    fn list_rows_do_not_have_credential_fields() {
        let encoded = serde_json::to_string(&MachineListRow {
            id: "0123456789abcdef0123456789abcdef".into(),
            label: "Build".into(),
            target: "dev@build".into(),
            session: "agents".into(),
            enabled: true,
            selected: false,
            source: "ssh",
            server: None,
            state: None,
            hidden: false,
            sync: None,
        })
        .unwrap();
        assert!(!encoded.contains("password"));
        assert!(!encoded.contains("key"));
    }
}
