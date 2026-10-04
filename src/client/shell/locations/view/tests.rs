use super::super::tests::{hangar_dialog, hangar_dialog_with, shell, MACHINE};
use super::*;
use crate::client::shell::render::display_width;
use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};

fn facts(kind: RemoteKind, state: Option<MachineState>) -> RemoteFacts {
    RemoteFacts {
        kind,
        state,
        enabled: state == Some(MachineState::Running) || kind != RemoteKind::Hangar,
        hidden: false,
        default: false,
        signed_out: false,
        offline: false,
        snapshots: None,
    }
}

/// `label` for each action, with `!reason` when dimmed.
fn summary(facts: &RemoteFacts) -> Vec<String> {
    remote_actions(facts)
        .iter()
        .map(|entry| match entry.reason {
            Some(reason) => format!("{}!{reason}", entry.label),
            None => entry.label.to_owned(),
        })
        .collect()
}

#[test]
fn actions_follow_the_remote_type_hide_by_state_and_dim_with_a_reason() {
    use MachineState::*;
    let wait = |state| transitional(state).unwrap();
    let strings = |items: &[&str]| {
        items
            .iter()
            .map(|item| item.to_string())
            .collect::<Vec<_>>()
    };
    // Everything after the lifecycle group of a plain hangar machine.
    let rest = |test: &str| {
        let mut rest = vec![
            test.to_owned(),
            "Edit…".into(),
            "Use as default".into(),
            "Hide from sidebar".into(),
            "Copy machine…".into(),
            "Delete machine…".into(),
        ];
        rest.retain(|item| !item.is_empty());
        rest
    };
    let hangar = |lifecycle: &[&str], test: &str| {
        let mut all = strings(lifecycle);
        all.extend(rest(test));
        all
    };
    let stopped_test = "Test connection!Start the machine first";
    let table: Vec<(RemoteFacts, Vec<String>)> = vec![
        (facts(RemoteKind::Local, None), strings(&["Use as default"])),
        (
            RemoteFacts {
                default: true,
                ..facts(RemoteKind::Local, None)
            },
            strings(&["Use as default!Already the default for new workspaces"]),
        ),
        (
            facts(RemoteKind::Ssh, None),
            strings(&[
                "Test connection",
                "Edit…",
                "Use as default",
                "Start session",
                "Remove remote",
            ]),
        ),
        (
            RemoteFacts {
                enabled: false,
                ..facts(RemoteKind::Ssh, None)
            },
            strings(&[
                "Test connection!Disabled; Start session enables it",
                "Edit…",
                "Use as default",
                "Start session",
                "Remove remote",
            ]),
        ),
        // Lifecycle actions show only where they make sense.
        (
            facts(RemoteKind::Hangar, Some(Running)),
            hangar(&["Suspend…", "Stop…"], "Test connection"),
        ),
        (
            facts(RemoteKind::Hangar, Some(Stopped)),
            hangar(&["Start"], stopped_test),
        ),
        (
            facts(RemoteKind::Hangar, Some(Suspended)),
            hangar(&["Resume"], stopped_test),
        ),
        (
            facts(RemoteKind::Hangar, Some(Error)),
            hangar(&["Start", "Stop…"], stopped_test),
        ),
        // Unknown state: the server decides.
        (
            facts(RemoteKind::Hangar, None),
            hangar(&["Start", "Stop…"], stopped_test),
        ),
        // Transitions show what applies next, dimmed until it finishes.
        (facts(RemoteKind::Hangar, Some(Stopping)), {
            let mut all = vec![format!("Start!{}", wait(Stopping))];
            all.extend(rest(stopped_test));
            for item in &mut all[5..6] {
                *item = format!("{item}!{}", wait(Stopping));
            }
            all
        }),
        (facts(RemoteKind::Hangar, Some(Starting)), {
            let mut all = vec![
                format!("Suspend…!{}", wait(Starting)),
                format!("Stop…!{}", wait(Starting)),
            ];
            all.extend(rest(stopped_test));
            for item in &mut all[6..7] {
                *item = format!("{item}!{}", wait(Starting));
            }
            all
        }),
        (facts(RemoteKind::Hangar, Some(Suspending)), {
            let mut all = vec![format!("Resume!{}", wait(Suspending))];
            all.extend(rest(stopped_test));
            for item in &mut all[5..6] {
                *item = format!("{item}!{}", wait(Suspending));
            }
            all
        }),
        (facts(RemoteKind::Hangar, Some(Deleting)), {
            let mut all = rest(stopped_test);
            for item in &mut all[4..6] {
                *item = format!("{item}!{}", wait(Deleting));
            }
            all
        }),
        // Blocked for another reason: dimmed with the reason.
        (
            RemoteFacts {
                snapshots: Some(false),
                hidden: true,
                enabled: false,
                default: true,
                ..facts(RemoteKind::Hangar, Some(Running))
            },
            strings(&[
                "Suspend…",
                "Stop…",
                "Test connection!Hidden from the sidebar; Show in sidebar first",
                "Edit…",
                "Use as default!Already the default for new workspaces",
                "Show in sidebar",
                "Copy machine…!Template too old to copy",
                "Delete machine…",
            ]),
        ),
        (
            RemoteFacts {
                signed_out: true,
                ..facts(RemoteKind::Hangar, Some(Stopped))
            },
            vec![
                format!("Start!{SIGN_IN_FIRST}"),
                stopped_test.into(),
                "Edit…".into(),
                "Use as default".into(),
                "Hide from sidebar".into(),
                format!("Copy machine…!{SIGN_IN_FIRST}"),
                format!("Delete machine…!{SIGN_IN_FIRST}"),
            ],
        ),
        (
            RemoteFacts {
                offline: true,
                ..facts(RemoteKind::Hangar, Some(Running))
            },
            vec![
                format!("Suspend…!{OFFLINE}"),
                format!("Stop…!{OFFLINE}"),
                "Test connection".into(),
                "Edit…".into(),
                "Use as default".into(),
                "Hide from sidebar".into(),
                format!("Copy machine…!{OFFLINE}"),
                format!("Delete machine…!{OFFLINE}"),
            ],
        ),
    ];
    for (facts, expected) in table {
        assert_eq!(summary(&facts), expected, "{facts:?}");
    }
    // Start and Resume are never both offered.
    for state in [
        Creating, Starting, Running, Stopping, Stopped, Suspending, Suspended, Resuming, Deleting,
        Deleted, Error, Unknown,
    ] {
        let labels = summary(&facts(RemoteKind::Hangar, Some(state)));
        let starts = labels
            .iter()
            .filter(|label| label.starts_with("Start") || label.starts_with("Resume"))
            .count();
        assert!(starts <= 1, "{state:?}: {labels:?}");
    }
    // Machine status is gone: the header shows the state and the view syncs itself.
    for state in [Running, Stopped, Suspended, Error, Unknown] {
        let labels = summary(&facts(RemoteKind::Hangar, Some(state)));
        assert!(
            labels.iter().all(|label| !label.contains("status")),
            "{labels:?}"
        );
    }
    // Groups: SSH remotes have no lifecycle or copy group; local only connection.
    let groups = |facts: &RemoteFacts| {
        let mut groups = remote_actions(facts)
            .iter()
            .map(|entry| entry.group)
            .collect::<Vec<_>>();
        groups.dedup();
        groups
    };
    assert_eq!(
        groups(&facts(RemoteKind::Hangar, Some(Running))),
        [
            ActionGroup::Lifecycle,
            ActionGroup::Connection,
            ActionGroup::Copy,
            ActionGroup::Remove
        ]
    );
    assert_eq!(
        groups(&facts(RemoteKind::Ssh, None)),
        [ActionGroup::Connection, ActionGroup::Remove]
    );
    assert_eq!(
        groups(&facts(RemoteKind::Local, None)),
        [ActionGroup::Connection]
    );
}

#[test]
fn the_list_shows_local_then_hangar_machines_then_ssh_remotes_then_add() {
    let mut dialog = hangar_dialog();
    // Profiles: Remote A (SSH), then box (hangar).
    assert_eq!(
        dialog.list_rows(),
        [
            ListRow::Local,
            ListRow::Remote(1),
            ListRow::Remote(0),
            ListRow::Add
        ]
    );
    assert_eq!(dialog.selected_row(), ListRow::Remote(1));
    let word = |dialog: &LocationDialog, row| dialog.list_state(row);
    assert_eq!(word(&dialog, ListRow::Local), None);
    assert_eq!(dialog.suffix(ListRow::Local), "default");
    assert_eq!(
        word(&dialog, ListRow::Remote(1)),
        Some(("running".into(), Tone::Good))
    );
    assert_eq!(
        word(&dialog, ListRow::Remote(0)),
        Some(("offline".into(), Tone::Muted))
    );
    dialog.view.connected.insert(dialog.profiles[0].id.clone());
    assert_eq!(
        word(&dialog, ListRow::Remote(0)),
        Some(("connected".into(), Tone::Good))
    );
    for (state, expected, tone) in [
        (MachineState::Suspended, "suspended", Tone::Muted),
        (MachineState::Stopped, "stopped", Tone::Muted),
        (MachineState::Error, "error", Tone::Bad),
        (MachineState::Starting, "starting…", Tone::Warn),
        (MachineState::Stopping, "stopping…", Tone::Warn),
        (MachineState::Unknown, "unknown", Tone::Muted),
    ] {
        assert_eq!(
            word(&hangar_dialog_with(state, false), ListRow::Remote(1)),
            Some((expected.into(), tone)),
            "{state:?}"
        );
    }
    let hidden = hangar_dialog_with(MachineState::Running, true);
    assert_eq!(hidden.suffix(ListRow::Remote(1)), "hidden");
    let id = dialog.profiles[1].id.clone();
    dialog.sync_notes.insert(id, "offline");
    assert_eq!(
        word(&dialog, ListRow::Remote(1)),
        Some(("offline".into(), Tone::Muted))
    );
    assert_eq!(dialog.suffix(ListRow::Remote(1)), "");
}

fn key(state: &mut ClientShellState, code: KeyCode) {
    press(state, code, crossterm::event::KeyModifiers::NONE);
}

/// A key as Settings gets it, on whichever tab is open.
fn press(state: &mut ClientShellState, code: KeyCode, modifiers: crossterm::event::KeyModifiers) {
    let key = crate::input::TerminalKey::from(KeyEvent::new(code, modifiers));
    let outcome = &mut ClientShellInput::default();
    let handled = if matches!(state.overlay, Some(ClientShellOverlay::Settings(_))) {
        state.route_settings_key(&key, outcome)
    } else {
        state.route_location_key(&key, outcome)
    };
    assert!(handled, "{code:?}");
}

fn remotes_shell() -> ClientShellState {
    let mut state = shell();
    // The fixture's remotes exist only in memory.
    state.locations.binding_validator = Some(|_, _| Ok(()));
    state.overlay = Some(ClientShellOverlay::Locations(hangar_dialog()));
    state
}

fn current(state: &ClientShellState) -> &LocationDialog {
    let Some(ClientShellOverlay::Locations(dialog)) = &state.overlay else {
        panic!("remotes dialog");
    };
    dialog
}

fn current_mut(state: &mut ClientShellState) -> &mut LocationDialog {
    let Some(ClientShellOverlay::Locations(dialog)) = state.overlay.as_mut() else {
        panic!("remotes dialog");
    };
    dialog
}

/// The settings tab shown, from the settings overlay or the remotes view.
fn settings_section(state: &ClientShellState) -> Option<ClientSettingsSection> {
    match &state.overlay {
        Some(ClientShellOverlay::Settings(settings)) => Some(settings.section),
        Some(ClientShellOverlay::Locations(dialog)) if dialog.remotes_open() => {
            Some(dialog.view.tab.section())
        }
        _ => None,
    }
}

#[test]
fn keyboard_moves_between_list_and_actions_with_enter_and_esc() {
    let mut state = remotes_shell();
    key(&mut state, KeyCode::Down);
    assert_eq!(
        current(&state).selected_row(),
        ListRow::Remote(0),
        "Remote A"
    );
    key(&mut state, KeyCode::Char('k'));
    assert_eq!(current(&state).profile().unwrap().label, "box");
    // Enter on a remote focuses its actions; ↑/↓ (j/k) then move between actions.
    key(&mut state, KeyCode::Enter);
    assert_eq!(current(&state).view.focus, Focus::Actions);
    assert_eq!(
        current(&state).selected_action().unwrap().action,
        RemoteAction::Suspend
    );
    key(&mut state, KeyCode::Char('j'));
    assert_eq!(current(&state).view.action, Some(RemoteAction::Stop));
    assert_eq!(current(&state).profile().unwrap().label, "box");
    // Esc returns to the list.
    key(&mut state, KeyCode::Esc);
    assert_eq!(current(&state).view.focus, Focus::List);
    assert!(state.overlay.is_some());
    // Add remote has no actions: Enter opens it instead.
    current_mut(&mut state).select_row(ListRow::Add);
    key(&mut state, KeyCode::Enter);
    assert!(matches!(current(&state).kind, LocationDialogKind::Add(_)));
    key(&mut state, KeyCode::Esc);
    assert!(matches!(current(&state).kind, LocationDialogKind::Manage));
    assert_eq!(current(&state).selected_row(), ListRow::Add);
    // Digits no longer switch anything.
    key(&mut state, KeyCode::Char('2'));
    assert_eq!(current(&state).view.tab, RemotesTab::Remotes);
    // Esc on the list closes Settings.
    key(&mut state, KeyCode::Esc);
    assert!(state.overlay.is_none());
}

#[test]
fn every_tab_key_cycles_through_all_eight_settings_tabs_and_wraps() {
    use crossterm::event::KeyModifiers as M;
    let all = ClientSettingsSection::ALL;
    assert_eq!(
        all.iter()
            .map(|section| section.label())
            .collect::<Vec<_>>(),
        [
            "theme",
            "indicators",
            "sound",
            "toasts",
            "integrations",
            "remotes",
            "images",
            "account"
        ]
    );
    let keys = [
        (KeyCode::Right, M::NONE, 1),
        (KeyCode::Tab, M::NONE, 1),
        (KeyCode::Char('l'), M::NONE, 1),
        (KeyCode::Left, M::NONE, -1),
        (KeyCode::BackTab, M::SHIFT, -1),
        (KeyCode::Char('h'), M::NONE, -1),
    ];
    for start in all {
        let views: Vec<(Focus, bool)> = match start.remotes_tab() {
            Some(_) => vec![
                (Focus::List, false),
                (Focus::Actions, false),
                (Focus::List, true),
                (Focus::Actions, true),
            ],
            None => vec![(Focus::List, false)],
        };
        for (focus, busy) in views {
            for (code, modifiers, delta) in keys {
                let mut state = remotes_shell();
                match start.remotes_tab() {
                    Some(tab) => {
                        let dialog = current_mut(&mut state);
                        dialog.view.tab = tab;
                        dialog.view.focus = focus;
                        dialog.busy = busy;
                    }
                    None => {
                        state.close_location();
                        state.open_settings_overlay();
                        state.select_settings_section(*start, &mut ClientShellInput::default());
                    }
                }
                assert_eq!(settings_section(&state), Some(*start));
                press(&mut state, code, modifiers);
                assert_eq!(
                    settings_section(&state),
                    Some(start.step(delta)),
                    "{start:?} {focus:?} busy={busy} {code:?}"
                );
            }
        }
    }
    // Holding → never sticks: two full rounds from theme, through every tab.
    let mut state = remotes_shell();
    state.close_location();
    state.open_settings_overlay();
    let mut seen = Vec::new();
    for _ in 0..all.len() * 2 {
        key(&mut state, KeyCode::Right);
        seen.push(settings_section(&state).unwrap());
    }
    let expected: Vec<_> = (1..=all.len() * 2)
        .map(|offset| all[offset % all.len()])
        .collect();
    assert_eq!(seen, expected);
    // While a remote operation started there still runs, the remotes view cannot
    // open: cycling passes over its tabs instead of closing Settings.
    let (_send, receive) = mpsc::channel();
    state.locations.job = Some((state.locations.epoch, receive));
    state.close_location();
    state.open_settings_overlay();
    state.select_settings_section(
        ClientSettingsSection::Integrations,
        &mut ClientShellInput::default(),
    );
    key(&mut state, KeyCode::Right);
    assert_eq!(settings_section(&state), Some(ClientSettingsSection::Theme));
    key(&mut state, KeyCode::Left);
    assert_eq!(
        settings_section(&state),
        Some(ClientSettingsSection::Integrations)
    );
}

#[test]
fn opening_the_images_and_account_tabs_fetches_what_they_show() {
    let mut state = remotes_shell();
    key(&mut state, KeyCode::Right);
    assert_eq!(current(&state).view.tab, RemotesTab::Images);
    assert_eq!(current(&state).view.focus, Focus::List);
    assert!(state.locations.images.is_some(), "images are listed");
    key(&mut state, KeyCode::Right);
    assert_eq!(current(&state).view.tab, RemotesTab::Account);
    assert!(state.locations.usage.is_some(), "usage is read");
    // From the settings overlay straight to the images tab: the view opens on it.
    state.close_location();
    state.locations.images = None;
    state.open_settings_overlay();
    state.select_settings_section(
        ClientSettingsSection::Images,
        &mut ClientShellInput::default(),
    );
    assert_eq!(current(&state).view.tab, RemotesTab::Images);
    assert!(state.locations.images.is_some());
    assert!(state.locations.sync.is_some(), "the remotes sync too");
    state.close_location();
}

#[test]
fn a_dimmed_action_says_why_and_runs_nothing() {
    let mut state = remotes_shell();
    let dialog = current_mut(&mut state);
    dialog.view.details.insert(
        ("https://hangar.test".into(), MACHINE.into()),
        MachineDetails {
            snapshots: Some(false),
            ..Default::default()
        },
    );
    dialog.view.focus = Focus::Actions;
    dialog.view.action = Some(RemoteAction::Copy);
    key(&mut state, KeyCode::Enter);
    let dialog = current(&state);
    assert_eq!(dialog.message, "Copy machine: Template too old to copy.");
    assert!(matches!(dialog.kind, LocationDialogKind::Manage));
    assert!(state.locations.job.is_none());
    let text = screen(&state, 120, 40);
    assert!(text.contains("↳ Template too old to copy"), "{text}");
    // A stopping machine: Start is shown, dimmed until the stop finishes.
    let mut state = remotes_shell();
    *current_mut(&mut state) = hangar_dialog_with(MachineState::Stopping, false);
    current_mut(&mut state).view.focus = Focus::Actions;
    current_mut(&mut state).view.action = Some(RemoteAction::Start);
    let text = screen(&state, 120, 40);
    assert!(
        text.contains("↳ Wait until the machine has stopped"),
        "{text}"
    );
    assert!(!text.contains("Suspend…"), "{text}");
}

#[test]
fn the_selected_action_stays_valid_when_a_refresh_changes_the_actions() {
    let deliver = |state: &mut ClientShellState, machine: MachineState| {
        let (send, receive) = mpsc::channel();
        state.locations.sync = Some((state.locations.epoch, receive));
        state.locations.last_sync = Some(std::time::Instant::now());
        send.send(Ok(super::super::tests::hangar_snapshot(machine, false)))
            .unwrap();
        state.tick_locations(&mut ClientShellInput::default());
    };
    let mut state = remotes_shell();
    current_mut(&mut state).view.focus = Focus::Actions;
    // Kept by identity: Edit… is offered in both states, at another place.
    current_mut(&mut state).select_action(RemoteAction::Edit);
    deliver(&mut state, MachineState::Stopped);
    assert_eq!(
        current(&state).selected_action().unwrap().action,
        RemoteAction::Edit
    );
    // Gone: the action now at its place is selected (Suspend… → Start).
    deliver(&mut state, MachineState::Running);
    current_mut(&mut state).select_action(RemoteAction::Suspend);
    deliver(&mut state, MachineState::Stopped);
    let selected = current(&state).selected_action().unwrap();
    assert_eq!(
        (selected.action, selected.label),
        (RemoteAction::Start, "Start")
    );
    // Suspended: the same action reads Resume.
    deliver(&mut state, MachineState::Suspended);
    assert_eq!(current(&state).selected_action().unwrap().label, "Resume");
    // Stop… (second) on a running machine clamps into the shorter stopped list.
    deliver(&mut state, MachineState::Running);
    current_mut(&mut state).select_action(RemoteAction::Stop);
    deliver(&mut state, MachineState::Stopped);
    let actions = current(&state).actions();
    let selected = current(&state).selected_action().unwrap();
    assert!(actions.contains(&selected));
    assert_eq!(actions.iter().position(|entry| entry == &selected), Some(1));
    // Rendering and Enter use the same, valid selection.
    let text = screen(&state, 120, 40);
    println!("{text}");
    assert!(text.contains(&format!("▸ {}", selected.label)));
}

#[test]
fn a_confirmation_returns_to_the_same_remote_and_action() {
    let mut state = remotes_shell();
    current_mut(&mut state).view.focus = Focus::Actions;
    current_mut(&mut state).view.action = Some(RemoteAction::Stop);
    key(&mut state, KeyCode::Enter);
    assert!(matches!(current(&state).kind, LocationDialogKind::Stop));
    assert!(screen(&state, 120, 40).contains("esc back"));
    key(&mut state, KeyCode::Esc);
    let dialog = current(&state);
    assert!(matches!(dialog.kind, LocationDialogKind::Manage));
    assert_eq!(dialog.profile().unwrap().label, "box");
    assert_eq!(dialog.view.focus, Focus::Actions);
    assert_eq!(dialog.view.action, Some(RemoteAction::Stop));
    assert!(dialog.message.is_empty());
    // A finished confirmation returns too, never staying armed.
    current_mut(&mut state).view.action = Some(RemoteAction::Suspend);
    key(&mut state, KeyCode::Enter);
    assert!(matches!(current(&state).kind, LocationDialogKind::Suspend));
    current_mut(&mut state).busy = true;
    let (send, receive) = mpsc::channel();
    state.locations.job = Some((state.locations.epoch, receive));
    send.send(Ok(JobResult::Message("Remote suspended".into())))
        .unwrap();
    state.tick_locations(&mut ClientShellInput::default());
    let dialog = current(&state);
    assert!(matches!(dialog.kind, LocationDialogKind::Manage));
    assert!(!dialog.busy);
    assert_eq!(dialog.message, "Remote suspended");
    assert_eq!(dialog.view.focus, Focus::Actions);
    assert_eq!(dialog.view.action, Some(RemoteAction::Suspend));
    assert_eq!(dialog.view.tab, RemotesTab::Remotes);
}

fn image(id: &str, name: &str, created: &str) -> Image {
    let mut value = crate::hangar::api::fake::image(id, name, MACHINE);
    value["createdAt"] = created.into();
    value["description"] = "Agents and toolchains preinstalled".into();
    value["exclusiveBytes"] = (512u64 << 20).into();
    serde_json::from_value(value).unwrap()
}

fn with_images(state: &mut ClientShellState, images: Vec<Image>) {
    let dialog = current_mut(state);
    dialog.view.tab = RemotesTab::Images;
    dialog.view.images = Some(Ok(ImageList {
        server: "https://hangar.test".into(),
        images,
    }));
}

#[test]
fn images_view_lists_details_and_returns_from_delete_to_the_same_image() {
    let mut state = remotes_shell();
    with_images(
        &mut state,
        vec![
            image("im_new", "agents", "2026-10-03T09:00:00Z"),
            image("im_old", "base", "2026-10-01T09:00:00Z"),
        ],
    );
    key(&mut state, KeyCode::Down);
    assert_eq!(current(&state).selected_image().unwrap().name, "base");
    let text = screen(&state, 120, 40);
    for part in [
        "agents",
        "2026-10-03",
        "Agents and toolchains preinstalled",
        "saved from box",
        "template herdr@2026-10-03.2",
        "root disk 4.0 GiB",
        "512.0 MiB stored only for this image",
        "New machine from image…",
        "Delete image…",
    ] {
        assert!(text.contains(part), "{part}\n{text}");
    }
    key(&mut state, KeyCode::Enter);
    assert_eq!(current(&state).view.focus, Focus::Actions);
    key(&mut state, KeyCode::Down);
    key(&mut state, KeyCode::Enter);
    let LocationDialogKind::DeleteImage(request) = &current(&state).kind else {
        panic!("delete image confirmation");
    };
    assert_eq!(request.id, "im_old");
    assert_eq!(request.server, "https://hangar.test");
    key(&mut state, KeyCode::Esc);
    let dialog = current(&state);
    assert!(matches!(dialog.kind, LocationDialogKind::Manage));
    assert_eq!(dialog.view.tab, RemotesTab::Images);
    assert_eq!(dialog.selected_image().unwrap().id, "im_old");
    assert_eq!(dialog.view.image_action, ImageAction::Delete);
    assert_eq!(dialog.view.focus, Focus::Actions);
}

#[test]
fn new_machine_from_image_opens_add_remote_with_the_image_as_source() {
    let mut state = remotes_shell();
    with_images(
        &mut state,
        vec![image("im_new", "agents", "2026-10-03T09:00:00Z")],
    );
    current_mut(&mut state).view.focus = Focus::Actions;
    key(&mut state, KeyCode::Enter);
    let dialog = current(&state);
    let LocationDialogKind::Add(form) = &dialog.kind else {
        panic!("add remote");
    };
    assert_eq!(
        form.source,
        MachineSource::Image {
            id: "im_new".into(),
            name: "agents".into()
        }
    );
    assert_eq!(dialog.selected, add::NAME_FIELD);
    assert_eq!(dialog.view.tab, RemotesTab::Images, "the view is kept");
    state.close_location();
}

#[test]
fn images_view_explains_how_to_save_an_image_when_there_is_none() {
    let mut state = remotes_shell();
    with_images(&mut state, Vec::new());
    let text = screen(&state, 120, 40);
    assert!(text.contains("No images yet."), "{text}");
    assert!(
        text.contains("remotes tab and choose Copy machine… → Save as"),
        "{text}"
    );
    current_mut(&mut state).view.images =
        Some(Err("Sign in on the account tab to list your images.".into()));
    assert!(screen(&state, 120, 40).contains("Sign in on the account tab"));
    key(&mut state, KeyCode::Enter);
    assert_eq!(
        current(&state).view.focus,
        Focus::List,
        "no image to act on"
    );
}

#[test]
fn stale_image_and_usage_results_are_ignored() {
    let mut state = remotes_shell();
    current_mut(&mut state).view.tab = RemotesTab::Images;
    let old = state.locations.epoch.wrapping_sub(1);
    let (send, receive) = mpsc::channel();
    state.locations.images = Some((old, receive));
    send.send(Ok(ImageList {
        server: "https://old.test".into(),
        images: vec![image("im_old", "stale", "2026-10-01T00:00:00Z")],
    }))
    .unwrap();
    let (send_usage, receive_usage) = mpsc::channel();
    state.locations.usage = Some((old, receive_usage));
    send_usage.send(Ok(Usage::default())).unwrap();
    state.tick_locations(&mut ClientShellInput::default());
    assert!(current(&state).view.images.is_none());
    assert!(current(&state).view.usage.is_none());
    // The open sub-view asks again.
    assert!(state.locations.images.is_some());
    // A current result is applied.
    let (send, receive) = mpsc::channel();
    state.locations.images = Some((state.locations.epoch, receive));
    send.send(Ok(ImageList {
        server: "https://hangar.test".into(),
        images: vec![image("im_new", "agents", "2026-10-03T00:00:00Z")],
    }))
    .unwrap();
    state.tick_locations(&mut ClientShellInput::default());
    assert_eq!(current(&state).images()[0].name, "agents");
}

fn click(state: &mut ClientShellState, hit: RemotesHit) {
    state.compose(120, 40).unwrap();
    let (rect, _) = *state
        .hits
        .remotes
        .iter()
        .find(|(_, candidate)| *candidate == hit)
        .unwrap_or_else(|| panic!("{hit:?} is not shown"));
    click_at(state, rect);
}

fn click_at(state: &mut ClientShellState, rect: Rect) {
    state.handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        },
        &mut ClientShellInput::default(),
    );
}

#[test]
fn clicks_select_list_items_run_actions_and_switch_sub_views() {
    let mut state = remotes_shell();
    click(&mut state, RemotesHit::Row(ListRow::Remote(0)));
    assert_eq!(current(&state).profile().unwrap().label, "Remote A");
    assert_eq!(current(&state).view.focus, Focus::List);
    click(&mut state, RemotesHit::Row(ListRow::Local));
    assert_eq!(current(&state).selected_row(), ListRow::Local);
    click(&mut state, RemotesHit::Row(ListRow::Remote(1)));
    click(&mut state, RemotesHit::Action(RemoteAction::Stop));
    assert!(matches!(current(&state).kind, LocationDialogKind::Stop));
    // The close button of the confirmation goes back.
    state.compose(120, 40).unwrap();
    let cancel = state.hits.overlay_cancel;
    state.handle_mouse(
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: cancel.x,
            row: cancel.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        },
        &mut ClientShellInput::default(),
    );
    assert!(matches!(current(&state).kind, LocationDialogKind::Manage));
    assert_eq!(current(&state).view.action, Some(RemoteAction::Stop));
    click_tab(&mut state, ClientSettingsSection::Account);
    assert_eq!(current(&state).view.tab, RemotesTab::Account);
    click_tab(&mut state, ClientSettingsSection::Images);
    assert_eq!(current(&state).view.tab, RemotesTab::Images);
    with_images(
        &mut state,
        vec![
            image("im_new", "agents", "2026-10-03T09:00:00Z"),
            image("im_old", "base", "2026-10-01T09:00:00Z"),
        ],
    );
    click(&mut state, RemotesHit::Image(1));
    assert_eq!(current(&state).selected_image().unwrap().name, "base");
    click(&mut state, RemotesHit::ImageAction(ImageAction::Delete));
    assert!(matches!(
        current(&state).kind,
        LocationDialogKind::DeleteImage(_)
    ));
    // A settings tab leads out of the view, and back in on the tab clicked.
    key(&mut state, KeyCode::Esc);
    click_tab(&mut state, ClientSettingsSection::Sound);
    assert_eq!(settings_section(&state), Some(ClientSettingsSection::Sound));
    click_tab(&mut state, ClientSettingsSection::Account);
    assert_eq!(
        settings_section(&state),
        Some(ClientSettingsSection::Account)
    );
    state.close_location();
}

fn click_tab(state: &mut ClientShellState, section: ClientSettingsSection) {
    state.compose(120, 40).unwrap();
    let (rect, _) = *state
        .hits
        .settings_tabs
        .iter()
        .find(|(_, candidate)| *candidate == section)
        .unwrap_or_else(|| panic!("{section:?} tab is not shown"));
    click_at(state, rect);
}

/// The rendered dialog as text, trailing spaces trimmed.
fn screen(state: &ClientShellState, width: u16, height: u16) -> String {
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    crate::client::shell::render::render_locations(
        &mut buffer,
        current(state),
        &state.config.palette,
    )
    .expect("rendered");
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A dialog with details, a machine and usage, as Settings shows them.
fn rich_shell() -> ClientShellState {
    let mut state = remotes_shell();
    let dialog = current_mut(&mut state);
    dialog.account = Some(AccountStatus::SignedIn {
        server: "https://hangar.test".into(),
        login: "octo".into(),
    });
    dialog.view.details.insert(
        ("https://hangar.test".into(), MACHINE.into()),
        MachineDetails {
            template: Some("herdr@2026-10-03.2".into()),
            spec: Some(crate::hangar::api::MachineSpec {
                vcpus: 2,
                mem_mib: 4096,
                persistent_disk_gib: 20,
                root_disk_gib: Some(8),
            }),
            image_id: None,
            forked_from: None,
            snapshots: Some(true),
            last_error: None,
        },
    );
    dialog.view.focus = Focus::Actions;
    dialog.view.action = Some(RemoteAction::Suspend);
    state
}

/// The lines of the details pane (right of the separator), trimmed.
fn details_pane(text: &str) -> Vec<String> {
    // Popup border, list, details, popup border.
    text.lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('│').collect();
            (parts.len() >= 4).then(|| parts[2].trim().to_owned())
        })
        .collect()
}

#[test]
fn the_remotes_tab_renders_list_details_and_one_column_of_actions() {
    let state = rich_shell();
    let text = screen(&state, 120, 40);
    println!("{text}");
    for part in [
        "theme  indicators  sound  toasts  integrations  remotes  images  account",
        "@octo",
        "    Local",
        "default",
        "▸ ⇄ box",
        "running",
        "  ⇄ Remote A",
        "offline",
        "  + Add remote",
        "box · hangar · running",
        "herdr@2026-10-03.2 · 2 vCPU · 4 GiB RAM · 8 GiB",
        "root · 20 GiB /data",
        "Delete machine…",
        "▸ Suspend…",
        "↑↓ select  ↵ run  esc back to list  ←→ tab",
    ] {
        assert!(text.contains(part), "{part}\n{text}");
    }
    for gone in [
        "Remotes · Images · Account",
        "Machine status",
        "Machine ",
        "Connection",
        "1-3",
        "●",
        "◐",
        "○",
    ] {
        assert!(!text.contains(gone), "{gone}\n{text}");
    }
    // One column, groups separated by one blank line.
    let pane = details_pane(&text);
    let start = pane.iter().position(|line| line == "▸ Suspend…").unwrap();
    assert_eq!(
        pane[start..start + 12],
        [
            "▸ Suspend…",
            "Stop…",
            "",
            "Test connection",
            "Edit…",
            "Use as default",
            "Hide from sidebar",
            "",
            "Copy machine…",
            "",
            "Delete machine…",
            "",
        ]
    );
}

#[test]
fn ssh_local_and_stopped_details_show_only_their_actions() {
    let mut state = rich_shell();
    current_mut(&mut state).location = 1;
    let text = screen(&state, 120, 40);
    let pane = details_pane(&text);
    assert!(text.contains("Remote A · ssh · offline"), "{text}");
    assert!(text.contains("demo-a · session work"), "{text}");
    let start = pane
        .iter()
        .position(|line| line.ends_with("Test connection"))
        .unwrap();
    assert_eq!(
        pane[start + 1..start + 6],
        [
            "Edit…",
            "Use as default",
            "Start session",
            "",
            "Remove remote"
        ]
    );
    assert!(!text.contains("Copy machine"), "{text}");
    current_mut(&mut state).location = 0;
    let text = screen(&state, 120, 40);
    assert!(text.contains("Local · this computer"), "{text}");
    assert!(text.contains("Use as default"), "{text}");
    assert!(!text.contains("Test connection"), "{text}");
    // A stopped machine: Start, then the rest; no Suspend… or Stop….
    let mut state = rich_shell();
    let details = current(&state).view.details.clone();
    *current_mut(&mut state) = hangar_dialog_with(MachineState::Stopped, false);
    current_mut(&mut state).view.details = details;
    let text = screen(&state, 120, 40);
    let pane = details_pane(&text);
    let start = pane.iter().position(|line| line == "Start").unwrap();
    assert_eq!(pane[start + 1], "");
    assert_eq!(pane[start + 2], "Test connection");
    assert!(
        !text.contains("Suspend…") && !text.contains("Stop…"),
        "{text}"
    );
    assert!(text.contains("box · hangar · stopped"), "{text}");
}

#[test]
fn a_failed_machine_shows_its_last_error_under_the_header() {
    let mut state = remotes_shell();
    *current_mut(&mut state) = hangar_dialog_with(MachineState::Error, false);
    current_mut(&mut state).view.details.insert(
        ("https://hangar.test".into(), MACHINE.into()),
        MachineDetails {
            last_error: Some("disk full".into()),
            ..Default::default()
        },
    );
    let text = screen(&state, 120, 40);
    assert!(text.contains("box · hangar · error"), "{text}");
    assert!(text.contains("last error: disk full"), "{text}");
}

#[test]
fn small_terminals_scroll_the_actions_to_the_selection() {
    let mut state = rich_shell();
    current_mut(&mut state).view.action = Some(RemoteAction::Remove);
    let text = screen(&state, 60, 18);
    println!("{text}");
    assert!(text.contains("▸ Delete machine…"), "{text}");
    assert!(
        text.contains("box · hangar · running"),
        "the header stays\n{text}"
    );
    // The active tab stays visible in the narrow tab strip.
    assert!(text.contains("remotes"), "{text}");
    // Too small for the popup: nothing is drawn, nothing panics.
    let area = Rect::new(0, 0, 20, 6);
    let mut buffer = Buffer::empty(area);
    assert!(crate::client::shell::render::render_locations(
        &mut buffer,
        current(&state),
        &state.config.palette
    )
    .is_none());
    state.compose(40, 12).unwrap();
}

#[test]
fn the_tab_strip_fits_the_popup_and_keeps_the_active_tab_visible_when_narrow() {
    use crate::client::shell::render::layout_settings_tabs;
    // At the settings popup width (76, so 74 inside) every tab fits, padded.
    let (slots, left, right) = layout_settings_tabs(74, ClientSettingsSection::Theme, false);
    assert_eq!(slots.len(), 8);
    assert!(!left && !right);
    assert_eq!(slots[0].text, " theme ");
    // With the integrations badge they still all fit, packed.
    let (slots, left, right) = layout_settings_tabs(74, ClientSettingsSection::Account, true);
    assert_eq!(slots.len(), 8);
    assert!(!left && !right);
    for width in 8..=74u16 {
        for badge in [false, true] {
            for active in ClientSettingsSection::ALL {
                let (slots, left, right) = layout_settings_tabs(width, *active, badge);
                let shown = slots
                    .iter()
                    .find(|slot| slot.section == *active)
                    .unwrap_or_else(|| panic!("{active:?} hidden at {width}"));
                let end = slots
                    .iter()
                    .map(|slot| slot.x + display_width(&slot.text))
                    .max()
                    .unwrap();
                // The last tab ends before the `›` marker when there is one.
                assert!(
                    end <= width.saturating_sub(if right { 2 } else { 0 })
                        || display_width(&shown.text) + 4 > width,
                    "{active:?} at {width}: {slots:?}"
                );
                assert_eq!(left, slots[0].section != ClientSettingsSection::ALL[0]);
                assert_eq!(
                    right,
                    slots.last().unwrap().section != *ClientSettingsSection::ALL.last().unwrap()
                );
            }
        }
    }
    // Rendered narrow: the active tab is drawn and clickable, with markers for the rest.
    let mut state = remotes_shell();
    current_mut(&mut state).view.tab = RemotesTab::Account;
    let text = screen(&state, 44, 20);
    println!("{text}");
    assert!(text.contains("account"), "{text}");
    assert!(text.contains('‹'), "{text}");
    state.compose(44, 20).unwrap();
    assert!(state
        .hits
        .settings_tabs
        .iter()
        .any(|(_, section)| *section == ClientSettingsSection::Account));
}

#[test]
fn render_snapshots() {
    // Remotes: a running hangar machine (actions focused).
    let mut state = rich_shell();
    println!(
        "remotes, running hangar machine (120x40):\n{}",
        screen(&state, 120, 40)
    );
    // A stopped hangar machine.
    let details = current(&state).view.details.clone();
    let account = current(&state).account.clone();
    *current_mut(&mut state) = hangar_dialog_with(MachineState::Stopped, false);
    current_mut(&mut state).view.details = details.clone();
    current_mut(&mut state).account = account.clone();
    println!(
        "remotes, stopped hangar machine (120x40):\n{}",
        screen(&state, 120, 40)
    );
    // SSH and local (list focused).
    let mut state = rich_shell();
    current_mut(&mut state).view.focus = Focus::List;
    current_mut(&mut state).location = 1;
    println!("remotes, SSH remote (120x40):\n{}", screen(&state, 120, 40));
    current_mut(&mut state).location = 0;
    println!("remotes, local (120x40):\n{}", screen(&state, 120, 40));
    // Small terminal.
    let mut state = rich_shell();
    current_mut(&mut state).view.action = Some(RemoteAction::Copy);
    println!("remotes, 60x18:\n{}", screen(&state, 60, 18));
    // Images.
    let mut state = rich_shell();
    with_images(
        &mut state,
        vec![
            image("im_new", "agents", "2026-10-03T09:00:00Z"),
            image("im_old", "base", "2026-10-01T09:00:00Z"),
        ],
    );
    current_mut(&mut state).view.focus = Focus::List;
    println!("images (120x40):\n{}", screen(&state, 120, 40));
    // Account.
    let dialog = current_mut(&mut state);
    dialog.view.tab = RemotesTab::Account;
    dialog.view.usage = Some(Ok(Usage {
        computed_at: Some("2026-10-03T08:00:00Z".into()),
        stored_bytes: 3 << 30,
        machines: 2,
        images: 2,
        limits: crate::hangar::api::UsageLimits {
            max_machines: 5,
            max_images: 10,
            max_stored_gib: 20,
        },
        ..Default::default()
    }));
    let text = screen(&state, 120, 40);
    println!("account (120x40):\n{text}");
    assert!(
        text.contains("Storage   3.0 GiB of 20.0 GiB stored (15%)"),
        "{text}"
    );
    assert!(text.contains("Switch account…"), "{text}");
}

#[test]
fn the_remote_icon_is_one_cell_and_names_truncate_cleanly() {
    use crate::client::shell::endpoints::REMOTE_ICON;
    assert_eq!(display_width(REMOTE_ICON), 1);
    let mut state = rich_shell();
    current_mut(&mut state).profiles[1].label = "a-very-long-hangar-machine-name".into();
    let text = screen(&state, 76, 30);
    let row = text
        .lines()
        .find(|line| line.contains("▸ ⇄ a-very"))
        .unwrap_or_else(|| panic!("{text}"));
    // The name is cut before the state word, which stays whole.
    assert!(row.contains("running"), "{row}");
    // The list keeps its width: the separator is where the other rows have it.
    let local = text.lines().find(|line| line.contains("Local ")).unwrap();
    let left = |line: &str| display_width(line.split('│').nth(1).unwrap());
    assert_eq!(left(row), left(local), "{row}\n{local}");
}

#[test]
fn bytes_and_dates_read_naturally() {
    assert_eq!(bytes(0), "0 B");
    assert_eq!(bytes(4 << 30), "4.0 GiB");
    assert_eq!(bytes(1536 << 20), "1.5 GiB");
    assert_eq!(date("2026-10-03T08:00:00Z"), "2026-10-03");
}

/// Copy machine… on box, opened from its action.
fn copy_chooser_shell() -> ClientShellState {
    let mut state = rich_shell();
    state.locations.save_checker = Some(|_| {
        Err(crate::hangar::api::HangarError::Invalid(
            "offline in tests".into(),
        ))
    });
    current_mut(&mut state).view.action = Some(RemoteAction::Copy);
    key(&mut state, KeyCode::Enter);
    state
}

fn copy_of(state: &ClientShellState) -> &super::super::CopyRequest {
    let LocationDialogKind::Copy(request) = &current(state).kind else {
        panic!("copy machine");
    };
    request
}

#[test]
fn copy_machine_chooses_between_clone_and_image_then_shows_that_form_and_goes_back() {
    use super::super::CopyChoice;
    use crate::client::locations::hangar::SnapshotUse;
    let mut state = copy_chooser_shell();
    let request = copy_of(&state);
    assert!(!request.chosen);
    assert_eq!(request.choice, CopyChoice::Clone, "clone first");
    assert_eq!(current(&state).title(), "copy box");
    assert!(current(&state).labels().is_empty());
    assert!(
        state.locations.image_check.is_none(),
        "nothing is checked yet"
    );
    // ←/→ (and h/l, tab) switch.
    for (code, expected) in [
        (KeyCode::Right, CopyChoice::Image),
        (KeyCode::Left, CopyChoice::Clone),
        (KeyCode::Char('l'), CopyChoice::Image),
        (KeyCode::Tab, CopyChoice::Clone),
    ] {
        key(&mut state, code);
        assert_eq!(copy_of(&state).choice, expected, "{code:?}");
    }
    // ↵ continues to the clone form, which checks the machine.
    key(&mut state, KeyCode::Enter);
    assert!(copy_of(&state).chosen);
    assert_eq!(current(&state).labels(), ["Name"]);
    assert_eq!(current(&state).fields[0].as_str(), "box-clone");
    assert!(matches!(
        state.locations.image_check,
        Some((_, SnapshotUse::Fork, _))
    ));
    let text = screen(&state, 120, 40);
    assert!(text.contains("clone box"), "{text}");
    assert!(text.contains("Name: box-clone"), "{text}");
    assert!(text.contains("esc back"), "{text}");
    // ←/→ edit the name here instead of switching.
    key(&mut state, KeyCode::Left);
    assert!(copy_of(&state).chosen);
    // Esc goes back to the chooser with the choice kept.
    key(&mut state, KeyCode::Esc);
    assert!(!copy_of(&state).chosen);
    assert_eq!(copy_of(&state).choice, CopyChoice::Clone);
    assert!(state.locations.image_check.is_none());
    assert!(current(&state).fields.is_empty());
    // The image form: name and description.
    key(&mut state, KeyCode::Right);
    key(&mut state, KeyCode::Enter);
    assert!(copy_of(&state).chosen);
    assert_eq!(current(&state).title(), "save box as image");
    assert_eq!(current(&state).labels(), ["Image name", "Description"]);
    assert!(matches!(
        state.locations.image_check,
        Some((_, SnapshotUse::Image, _))
    ));
    key(&mut state, KeyCode::Esc);
    assert_eq!(copy_of(&state).choice, CopyChoice::Image);
    // Esc in the chooser returns to the remotes tab with the selection kept.
    key(&mut state, KeyCode::Esc);
    let dialog = current(&state);
    assert!(matches!(dialog.kind, LocationDialogKind::Manage));
    assert_eq!(dialog.profile().unwrap().label, "box");
    assert_eq!(dialog.view.focus, Focus::Actions);
    assert_eq!(dialog.view.action, Some(RemoteAction::Copy));
    assert_eq!(dialog.view.tab, RemotesTab::Remotes);
}

#[test]
fn copy_machine_columns_are_clicked_to_choose_and_again_to_continue() {
    use super::super::CopyChoice;
    let column = |state: &mut ClientShellState, index: usize| {
        state.compose(120, 40).unwrap();
        let (rect, _) = *state
            .hits
            .settings_choices
            .iter()
            .find(|(_, choice)| *choice == index)
            .unwrap_or_else(|| panic!("column {index} is not shown"));
        click_at(state, rect);
    };
    let mut state = copy_chooser_shell();
    column(&mut state, 1);
    assert_eq!(copy_of(&state).choice, CopyChoice::Image);
    assert!(!copy_of(&state).chosen);
    column(&mut state, 0);
    assert_eq!(copy_of(&state).choice, CopyChoice::Clone);
    column(&mut state, 0);
    assert!(copy_of(&state).chosen, "a second click continues");
    // The cancel button of the form goes back to the chooser, and then to the tab.
    state.compose(120, 40).unwrap();
    let cancel = state.hits.overlay_cancel;
    click_at(&mut state, cancel);
    assert!(!copy_of(&state).chosen);
    state.compose(120, 40).unwrap();
    let primary = state.hits.overlay_primary;
    click_at(&mut state, primary);
    assert!(copy_of(&state).chosen, "↵ continue");
    state.compose(120, 40).unwrap();
    let cancel = state.hits.overlay_cancel;
    click_at(&mut state, cancel);
    state.compose(120, 40).unwrap();
    let cancel = state.hits.overlay_cancel;
    click_at(&mut state, cancel);
    assert!(matches!(current(&state).kind, LocationDialogKind::Manage));
}

#[test]
fn the_copy_chooser_compares_clone_and_image_side_by_side() {
    use crate::client::shell::render::{CHECK, COPY_TABLE, CROSS};
    assert_eq!(display_width(CHECK), 1);
    assert_eq!(display_width(CROSS), 1);
    // Clone copies everything; an image only the root disk.
    assert!(COPY_TABLE.iter().all(|(_, clone, _)| *clone));
    assert_eq!(
        COPY_TABLE
            .iter()
            .filter(|(_, _, image)| !image)
            .map(|(row, _, _)| *row)
            .collect::<Vec<_>>(),
        ["Repos & home files", "Logins (gh, claude)"]
    );
    let mut state = copy_chooser_shell();
    let text = screen(&state, 120, 40);
    println!("copy chooser (120x40):\n{text}");
    for part in [
        "copy box",
        "▸ Clone now",
        "Save as image",
        "a second machine",
        "exactly like this one",
        "a starting point for",
        "new machines",
        "Clone",
        "Image",
        "Installed software",
        "System settings",
        "Repos & home files",
        "Logins (gh, claude)",
        "new machine",
        "reusable image",
        "↵ continue",
        "←→ switch",
        "esc cancel",
    ] {
        assert!(text.contains(part), "{part}\n{text}");
    }
    // The chooser stays clean: the stop is explained in the second step.
    assert!(!text.contains("stopped"), "{text}");
    let row = |text: &str, label: &str| {
        text.lines()
            .find(|line| line.contains(label))
            .unwrap_or_else(|| panic!("{label}\n{text}"))
            .to_owned()
    };
    for (label, clone, image) in COPY_TABLE {
        let line = row(&text, label);
        let marks: Vec<char> = line.chars().filter(|c| *c == '✓' || *c == '✗').collect();
        let expected: Vec<char> = [clone, image]
            .iter()
            .map(|included| if *included { '✓' } else { '✗' })
            .collect();
        assert_eq!(marks, expected, "{line}");
    }
    // ✓ is green, ✗ dim.
    let area = Rect::new(0, 0, 120, 40);
    let mut buffer = Buffer::empty(area);
    crate::client::shell::render::render_locations(
        &mut buffer,
        current(&state),
        &state.config.palette,
    )
    .unwrap();
    let palette = &state.config.palette;
    for y in 0..40 {
        for x in 0..120 {
            match buffer[(x, y)].symbol() {
                "✓" => assert_eq!(buffer[(x, y)].fg, palette.green),
                "✗" => assert_eq!(buffer[(x, y)].fg, palette.overlay0),
                _ => {}
            }
        }
    }
    key(&mut state, KeyCode::Right);
    let text = screen(&state, 120, 40);
    assert!(text.contains("▸ Save as image"), "{text}");
    // Small terminals stack the choices and keep the table readable.
    let text = screen(&state, 44, 20);
    println!("copy chooser (44x20):\n{text}");
    for part in [
        "Clone now",
        "▸ Save as image",
        "Installed software",
        "✓",
        "✗",
    ] {
        assert!(text.contains(part), "{part}\n{text}");
    }
    assert!(text.contains("↵ continue"), "{text}");
    state.compose(44, 20).unwrap();
    // Very small: nothing panics.
    for (width, height) in [(30, 12), (26, 9), (20, 6)] {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        let _ = crate::client::shell::render::render_locations(
            &mut buffer,
            current(&state),
            &state.config.palette,
        );
        state.compose(width, height).unwrap();
    }
}

#[test]
fn copy_forms_render_with_their_state_handling() {
    use crate::client::locations::hangar::{SaveCheck, SavePlan};
    let mut state = copy_chooser_shell();
    key(&mut state, KeyCode::Enter);
    let epoch = state.locations.epoch;
    let (send, receive) = mpsc::channel();
    state.locations.image_check = Some((
        epoch,
        crate::client::locations::hangar::SnapshotUse::Fork,
        receive,
    ));
    send.send(Ok(SaveCheck {
        plan: Some(SavePlan::Stop),
        note: "box is running. Only a stopped machine can be cloned. Stop machine and clone stops all sessions and jobs on it (as Stop machine… does), waits until it is stopped, then clones it.".into(),
    }))
    .unwrap();
    state.tick_locations(&mut ClientShellInput::default());
    let text = screen(&state, 120, 40);
    println!("clone form (120x40):\n{text}");
    assert!(text.contains("↵ stop machine and clone"), "{text}");
    assert!(text.contains("This machine is stopped first"), "{text}");
    key(&mut state, KeyCode::Esc);
    key(&mut state, KeyCode::Right);
    key(&mut state, KeyCode::Enter);
    let text = screen(&state, 120, 40);
    println!("image form, checking (120x40):\n{text}");
    assert!(text.contains("Image name:"), "{text}");
    assert!(text.contains("Description:"), "{text}");
    assert!(text.contains("Checking box…"), "{text}");
    println!("image form (60x18):\n{}", screen(&state, 60, 18));
}
