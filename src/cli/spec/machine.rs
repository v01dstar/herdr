use clap::{Arg, Command};

use super::{flag, json_flag, option};

pub(super) fn command() -> Command {
    Command::new("machine")
        .about("Manage saved SSH machines and list hangar machines")
        .subcommand(
            Command::new("list")
                .about("List saved SSH machines and hangar machines (refreshed from hangar)")
                .arg(json_flag()),
        )
        .subcommand(
            Command::new("status")
                .about("Check saved machines without prompting for authentication")
                .arg(Arg::new("machine").value_name("LABEL_OR_ID"))
                .arg(json_flag()),
        )
        .subcommand(
            Command::new("reconnect")
                .about("Authenticate a saved machine in this terminal and verify connectivity")
                .arg(Arg::new("machine").value_name("LABEL_OR_ID").required(true)),
        )
        .subcommand(
            Command::new("add")
                .about("Prepare the remote Herdr server and save an SSH machine")
                .arg(
                    Arg::new("ssh-target")
                        .value_name("SSH_TARGET")
                        .required(true),
                )
                .arg(
                    option("label", "LABEL").help(
                        "Set the machine label shown in the sidebar (defaults to the SSH host, or host/session)",
                    ),
                )
                .arg(
                    option("remote-session", "NAME")
                        .help("Select a session explicitly (default without an interactive terminal)"),
                )
                .arg(flag("hangar").help(
                    "hangar machines appear automatically; with this flag Herdr only checks that the named machine is listed",
                )),
        )
        .subcommand(
            profile_command(
                "rename",
                "Rename a saved SSH machine, or set the name Herdr shows for a hangar machine",
            ).arg(
                option("label", "LABEL")
                    .required(true)
                    .help("Set the machine label shown in the sidebar"),
            ),
        )
        .subcommand(
            profile_command("remove", "Remove a saved SSH machine, or delete a hangar machine").arg(flag("delete-machine").help(
                "Required for a hangar machine: permanently delete it in hangar, with its disks and snapshots",
            )),
        )
        .subcommand(profile_command(
            "enable",
            "Enable a saved SSH machine, or show a hidden hangar machine in the sidebar",
        ))
        .subcommand(profile_command(
            "disable",
            "Disable a saved SSH machine, or hide a hangar machine from the sidebar",
        ))
}

fn profile_command(name: &'static str, about: &'static str) -> Command {
    Command::new(name).about(about).arg(
        Arg::new("profile-id")
            .value_name("PROFILE_ID")
            .required(true),
    )
}
