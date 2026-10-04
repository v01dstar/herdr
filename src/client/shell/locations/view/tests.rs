use super::super::tests::{hangar_dialog, hangar_dialog_with, shell, MACHINE};
use super::*;
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
    // Everything after the Machine group of a plain hangar machine.
    let rest = |test: &str| {
        let mut rest = vec![
            "Machine status".to_owned(),
            test.to_owned(),
            "Edit…".into(),
            "Use as default".into(),
            "Hide from sidebar".into(),
            "Save as image…".into(),
            "Fork…".into(),
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
                "Start session",
                "Edit…",
                "Use as default",
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
                "Start session",
                "Edit…",
                "Use as default",
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
            for item in &mut all[6..8] {
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
            for item in &mut all[7..9] {
                *item = format!("{item}!{}", wait(Starting));
            }
            all
        }),
        (facts(RemoteKind::Hangar, Some(Suspending)), {
            let mut all = vec![format!("Resume!{}", wait(Suspending))];
            all.extend(rest(stopped_test));
            for item in &mut all[6..8] {
                *item = format!("{item}!{}", wait(Suspending));
            }
            all
        }),
        (facts(RemoteKind::Hangar, Some(Deleting)), {
            let mut all = rest(stopped_test);
            for item in &mut all[5..8] {
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
                "Machine status",
                "Test connection!Hidden from the sidebar; Show in sidebar first",
                "Edit…",
                "Use as default!Already the default for new workspaces",
                "Show in sidebar",
                "Save as image…!Template too old for images",
                "Fork…!Template too old to fork",
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
                format!("Machine status!{SIGN_IN_FIRST}"),
                stopped_test.into(),
                "Edit…".into(),
                "Use as default".into(),
                "Hide from sidebar".into(),
                format!("Save as image…!{SIGN_IN_FIRST}"),
                format!("Fork…!{SIGN_IN_FIRST}"),
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
                format!("Machine status!{OFFLINE}"),
                "Test connection".into(),
                "Edit…".into(),
                "Use as default".into(),
                "Hide from sidebar".into(),
                format!("Save as image…!{OFFLINE}"),
                format!("Fork…!{OFFLINE}"),
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
    // Groups: SSH remotes have no Machine or Copy group; local only Connection.
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
            ActionGroup::Machine,
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
    assert_eq!(dialog.glyph(ListRow::Local), ("●", Tone::Good));
    assert_eq!(dialog.suffix(ListRow::Local), "default");
    assert_eq!(dialog.glyph(ListRow::Remote(1)), ("●", Tone::Good));
    assert_eq!(dialog.glyph(ListRow::Remote(0)), ("?", Tone::Muted));
    dialog.view.connected.insert(dialog.profiles[0].id.clone());
    assert_eq!(dialog.glyph(ListRow::Remote(0)), ("●", Tone::Good));
    for (state, glyph) in [
        (MachineState::Suspended, "◐"),
        (MachineState::Stopped, "○"),
        (MachineState::Error, "!"),
        (MachineState::Starting, "…"),
        (MachineState::Unknown, "?"),
    ] {
        assert_eq!(
            hangar_dialog_with(state, false).glyph(ListRow::Remote(1)).0,
            glyph
        );
    }
    let hidden = hangar_dialog_with(MachineState::Running, true);
    assert_eq!(hidden.suffix(ListRow::Remote(1)), "hidden");
    let id = dialog.profiles[1].id.clone();
    dialog.sync_notes.insert(id, "offline");
    assert_eq!(dialog.glyph(ListRow::Remote(1)), ("?", Tone::Muted));
    assert_eq!(dialog.suffix(ListRow::Remote(1)), "offline");
}

fn key(state: &mut ClientShellState, code: KeyCode) {
    assert!(state.route_location_key(
        &crate::input::TerminalKey::from(
            KeyEvent::new(code, crossterm::event::KeyModifiers::NONE,)
        ),
        &mut ClientShellInput::default(),
    ));
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

fn settings_section(state: &ClientShellState) -> Option<ClientSettingsSection> {
    match &state.overlay {
        Some(ClientShellOverlay::Settings(settings)) => Some(settings.section),
        _ => None,
    }
}

#[test]
fn keyboard_moves_focus_between_panes_and_switches_sub_views() {
    let mut state = remotes_shell();
    key(&mut state, KeyCode::Down);
    assert_eq!(
        current(&state).selected_row(),
        ListRow::Remote(0),
        "Remote A"
    );
    key(&mut state, KeyCode::Up);
    assert_eq!(current(&state).profile().unwrap().label, "box");
    // Enter on a remote focuses its actions; ↑/↓ then move between actions.
    key(&mut state, KeyCode::Enter);
    assert_eq!(current(&state).view.focus, Focus::Actions);
    assert_eq!(
        current(&state).selected_action().unwrap().action,
        RemoteAction::Suspend
    );
    key(&mut state, KeyCode::Down);
    assert_eq!(current(&state).view.action, Some(RemoteAction::Stop));
    assert_eq!(current(&state).profile().unwrap().label, "box");
    // Esc returns to the list; Tab toggles.
    key(&mut state, KeyCode::Esc);
    assert_eq!(current(&state).view.focus, Focus::List);
    assert!(state.overlay.is_some());
    key(&mut state, KeyCode::Tab);
    assert_eq!(current(&state).view.focus, Focus::Actions);
    key(&mut state, KeyCode::BackTab);
    assert_eq!(current(&state).view.focus, Focus::List);
    // Add remote has no actions: Tab stays on the list.
    current_mut(&mut state).select_row(ListRow::Add);
    key(&mut state, KeyCode::Tab);
    assert_eq!(current(&state).view.focus, Focus::List);
    // 1/2/3 switch sub-views.
    key(&mut state, KeyCode::Char('2'));
    assert_eq!(current(&state).view.tab, RemotesTab::Images);
    assert!(state.locations.images.is_some(), "images are listed");
    key(&mut state, KeyCode::Char('3'));
    assert_eq!(current(&state).view.tab, RemotesTab::Account);
    assert!(state.locations.usage.is_some(), "usage is read");
    key(&mut state, KeyCode::Char('1'));
    assert_eq!(current(&state).view.tab, RemotesTab::Remotes);
    // Esc on the list closes Settings.
    key(&mut state, KeyCode::Esc);
    assert!(state.overlay.is_none());
}

#[test]
fn left_and_right_always_switch_settings_tabs() {
    for (tab, focus, busy, code, expected) in [
        (
            RemotesTab::Remotes,
            Focus::List,
            false,
            KeyCode::Right,
            ClientSettingsSection::Theme,
        ),
        (
            RemotesTab::Remotes,
            Focus::Actions,
            false,
            KeyCode::Right,
            ClientSettingsSection::Theme,
        ),
        (
            RemotesTab::Images,
            Focus::Actions,
            false,
            KeyCode::Left,
            ClientSettingsSection::Integrations,
        ),
        (
            RemotesTab::Account,
            Focus::List,
            false,
            KeyCode::Right,
            ClientSettingsSection::Theme,
        ),
        (
            RemotesTab::Remotes,
            Focus::Actions,
            true,
            KeyCode::Left,
            ClientSettingsSection::Integrations,
        ),
    ] {
        let mut state = remotes_shell();
        let dialog = current_mut(&mut state);
        dialog.view.tab = tab;
        dialog.view.focus = focus;
        dialog.busy = busy;
        key(&mut state, code);
        assert_eq!(
            settings_section(&state),
            Some(expected),
            "{tab:?} {focus:?} busy={busy}"
        );
    }
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
    dialog.view.action = Some(RemoteAction::SaveImage);
    key(&mut state, KeyCode::Enter);
    let dialog = current(&state);
    assert_eq!(
        dialog.message,
        "Save as image: Template too old for images."
    );
    assert!(matches!(dialog.kind, LocationDialogKind::Manage));
    assert!(state.locations.job.is_none());
    let text = screen(&state, 120, 40);
    assert!(text.contains("↳ Template too old for images"), "{text}");
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
    // Kept by identity: Machine status is offered in both states.
    current_mut(&mut state).select_action(RemoteAction::Status);
    deliver(&mut state, MachineState::Stopped);
    assert_eq!(
        current(&state).selected_action().unwrap().action,
        RemoteAction::Status
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
    key(&mut state, KeyCode::Tab);
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
    assert!(text.contains("Save as image…"), "{text}");
    current_mut(&mut state).view.images =
        Some(Err("Sign in on the Account tab to list your images.".into()));
    assert!(screen(&state, 120, 40).contains("Sign in on the Account tab"));
    key(&mut state, KeyCode::Tab);
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
    click(&mut state, RemotesHit::Tab(RemotesTab::Account));
    assert_eq!(current(&state).view.tab, RemotesTab::Account);
    click(&mut state, RemotesHit::Tab(RemotesTab::Images));
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

/// A dialog with details, a forked machine and usage, as Settings shows them.
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
        },
    );
    dialog.view.focus = Focus::Actions;
    dialog.view.action = Some(RemoteAction::Suspend);
    state
}

#[test]
fn the_remotes_view_renders_list_details_and_grouped_actions() {
    let state = rich_shell();
    let text = screen(&state, 120, 40);
    println!("{text}");
    for part in [
        "Remotes · Images · Account",
        "@octo",
        "  ● Local",
        "default",
        "▸ ● box",
        "  ? Remote A",
        "  + Add remote",
        "box · hangar · running",
        "hangar.test · herdr@2026-10-03.2",
        "2 vCPU · 4 GiB RAM · 8 GiB root · 20 GiB /data",
        "Machine",
        "Connection",
        "Copy",
        "Remove",
        "▸ Suspend…",
        "Save as image…",
        "Delete machine…",
        "↑↓ select  ↵ run  tab/esc list",
    ] {
        assert!(text.contains(part), "{part}\n{text}");
    }
    // The settings tabs stay.
    assert!(text.contains("Remotes"), "{text}");
}

#[test]
fn ssh_and_local_details_show_only_their_actions() {
    let mut state = rich_shell();
    current_mut(&mut state).location = 1;
    let text = screen(&state, 120, 40);
    assert!(text.contains("Remote A · ssh · not connected"), "{text}");
    assert!(text.contains("demo-a · session work"), "{text}");
    assert!(text.contains("Remove remote"), "{text}");
    assert!(!text.contains("Machine status"), "{text}");
    assert!(!text.contains("Save as image"), "{text}");
    current_mut(&mut state).location = 0;
    let text = screen(&state, 120, 40);
    assert!(text.contains("Local · this computer"), "{text}");
    assert!(text.contains("Use as default"), "{text}");
    assert!(!text.contains("Test connection"), "{text}");
}

#[test]
fn small_terminals_still_show_the_selected_action() {
    let mut state = rich_shell();
    current_mut(&mut state).view.action = Some(RemoteAction::Remove);
    let text = screen(&state, 60, 18);
    println!("{text}");
    assert!(text.contains("▸ Delete machine…"), "{text}");
    assert!(
        text.contains("box · hangar · running"),
        "the header stays\n{text}"
    );
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
fn snapshots_of_the_images_and_account_views() {
    let mut state = rich_shell();
    with_images(
        &mut state,
        vec![
            image("im_new", "agents", "2026-10-03T09:00:00Z"),
            image("im_old", "base", "2026-10-01T09:00:00Z"),
        ],
    );
    current_mut(&mut state).view.focus = Focus::List;
    println!("{}", screen(&state, 120, 40));
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
    println!("{text}");
    assert!(
        text.contains("Storage   3.0 GiB of 20.0 GiB stored (15%)"),
        "{text}"
    );
}

#[test]
fn bytes_and_dates_read_naturally() {
    assert_eq!(bytes(0), "0 B");
    assert_eq!(bytes(4 << 30), "4.0 GiB");
    assert_eq!(bytes(1536 << 20), "1.5 GiB");
    assert_eq!(date("2026-10-03T08:00:00Z"), "2026-10-03");
}
