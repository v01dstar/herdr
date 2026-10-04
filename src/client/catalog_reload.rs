use super::*;

pub(super) fn watch_profiles(
    event_tx: tokio::sync::mpsc::Sender<ClientLoopEvent>,
    should_quit: Arc<AtomicBool>,
) {
    // One bounded read per second per client, independent of rendering and pane count.
    std::thread::spawn(move || {
        let mut previous = None;
        while !should_quit.load(Ordering::Acquire) {
            // Saved SSH profiles plus visible hangar machines from the synced list.
            let current = crate::client::locations::effective_profiles();
            if previous.as_ref() != Some(&current) {
                previous = Some(current.clone());
                if event_tx
                    .blocking_send(ClientLoopEvent::EndpointCatalog(current))
                    .is_err()
                {
                    break;
                }
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

// Only called between surface handoffs: removing a source must not invalidate an in-flight
// rollback. Connection attempts are independent and fenced by supervisor generations.
pub(super) fn apply_profiles(
    state: &mut ClientState,
    endpoints: &mut endpoint::EndpointRegistry,
    commands: &mut endpoint_commands::EndpointCommands,
    supervisors: &mut endpoint::EndpointSupervisors,
    catalog: &mut endpoint::EndpointCatalog,
    profiles: Vec<endpoint::SavedSshEndpoint>,
    now: std::time::Instant,
) -> bool {
    if catalog.ssh == profiles {
        return false;
    }
    let previous_size = state
        .shell
        .as_ref()
        .map(|shell| shell.surface_size(state.reported_size.0, state.reported_size.1));
    let retired = supervisors.reconcile_profiles(&profiles, now);
    let active_removed = retired.contains(endpoints.active_id());
    for endpoint_id in retired {
        endpoints.disconnect(&endpoint_id);
        let cancelled = commands.disconnect(&endpoint_id);
        #[cfg(unix)]
        state.forget_endpoint_graphics(&endpoint_id);
        if let Some(shell) = state.shell.as_mut() {
            for request_id in cancelled {
                shell.cancel_endpoint_request(&request_id);
            }
            shell.retire_endpoint(&endpoint_id);
            // A hangar machine deleted elsewhere: say so instead of leaving a silent gap.
            // It is gone from the list, so nothing reconnects to it.
            if let endpoint::ClientEndpointId::Ssh(profile_id) = &endpoint_id {
                if !profiles.iter().any(|profile| &profile.id == profile_id) {
                    if let Some(name) =
                        crate::client::locations::sync::removed_machine_name(profile_id)
                    {
                        shell.notify_endpoint(format!("{name}: machine deleted on server"));
                    }
                }
            }
        }
    }
    catalog.ssh = profiles;
    if active_removed {
        endpoints.select_unavailable_local();
        catalog.select_local();
        state.freeze_presentation();
        if let Some(shell) = state.shell.as_mut() {
            shell.select_unavailable_local();
        }
    } else if catalog.selected_profile.as_ref().is_some_and(|selected| {
        !catalog
            .ssh
            .iter()
            .any(|profile| &profile.id == selected && profile.enabled)
    }) {
        catalog.select_local();
    }
    if let Some(shell) = state.shell.as_mut() {
        shell.set_endpoint_catalog(&catalog.ssh);
        if endpoints.active_surface_available()
            && previous_size
                != Some(shell.surface_size(state.reported_size.0, state.reported_size.1))
        {
            shell.invalidate_pane_surface();
            endpoints.send(&client_shell_resize_message(
                shell,
                state.reported_size.0,
                state.reported_size.1,
                state.reported_cell_size.0,
                state.reported_cell_size.1,
                state.pixel_geometry_exact,
            ));
        }
    }
    active_removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use endpoint::{ClientEndpointId, EndpointCatalog, EndpointRegistry, EndpointSupervisors};
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    struct Transport(Arc<AtomicUsize>);

    impl endpoint::EndpointTransport for Transport {
        fn send(&mut self, _: &ClientMessage) -> io::Result<()> {
            Ok(())
        }

        fn disconnect(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn state() -> ClientState {
        ClientState::test_new()
    }

    #[test]
    fn live_catalog_add_and_rename_keep_local_connection_and_selection() {
        let now = Instant::now();
        let mut state = state();
        let disconnected = Arc::new(AtomicUsize::new(0));
        let mut endpoints =
            EndpointRegistry::new(Transport(disconnected.clone()), 1, Default::default());
        let mut supervisors = EndpointSupervisors::new(&[], now);
        let mut commands = endpoint_commands::EndpointCommands::default();
        let mut catalog = EndpointCatalog::default();
        let mut profile = endpoint::SavedSshEndpoint::new("Build", "build", "main").unwrap();
        for label in ["Build", "Renamed"] {
            profile.label = label.into();
            assert!(!apply_profiles(
                &mut state,
                &mut endpoints,
                &mut commands,
                &mut supervisors,
                &mut catalog,
                vec![profile.clone()],
                now
            ));
            assert_eq!(endpoints.active_id(), &ClientEndpointId::Local);
            assert!(endpoints.active_surface_available());
            assert_eq!(
                endpoints
                    .connection(&ClientEndpointId::Local)
                    .unwrap()
                    .generation,
                1
            );
            assert_eq!(catalog.selected_profile, None);
            assert_eq!(
                state
                    .shell
                    .as_ref()
                    .unwrap()
                    .endpoint_label(&ClientEndpointId::Ssh(profile.id.clone())),
                label
            );
        }
        assert_eq!(disconnected.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn live_catalog_remove_or_disable_active_machine_selects_local_without_input() {
        for local_online in [false, true] {
            for disable in [false, true] {
                let now = Instant::now();
                let mut state = state();
                let mut catalog = EndpointCatalog::default();
                let id = catalog.add_ssh("Build", "build", "main").unwrap();
                let remote = ClientEndpointId::Ssh(id.clone());
                catalog.select_ssh(&id);
                state
                    .shell
                    .as_mut()
                    .unwrap()
                    .set_endpoint_catalog(&catalog.ssh);
                let mut supervisors = EndpointSupervisors::new(&catalog.ssh, now);
                let mut endpoints = EndpointRegistry::empty();
                let local_disconnects = Arc::new(AtomicUsize::new(0));
                if local_online {
                    endpoints.insert(
                        ClientEndpointId::Local,
                        Transport(local_disconnects.clone()),
                        1,
                        Default::default(),
                        false,
                    );
                }
                let remote_disconnects = Arc::new(AtomicUsize::new(0));
                endpoints.insert(
                    remote.clone(),
                    Transport(remote_disconnects.clone()),
                    2,
                    Default::default(),
                    true,
                );
                endpoints.set_active(&remote);
                endpoints.unfreeze_input();
                let mut commands = endpoint_commands::EndpointCommands::default();
                let profiles = if disable {
                    let mut profiles = catalog.ssh.clone();
                    profiles[0].enabled = false;
                    profiles
                } else {
                    Vec::new()
                };
                assert!(apply_profiles(
                    &mut state,
                    &mut endpoints,
                    &mut commands,
                    &mut supervisors,
                    &mut catalog,
                    profiles,
                    now
                ));
                assert_eq!(endpoints.active_id(), &ClientEndpointId::Local);
                assert!(!endpoints.active_surface_available());
                assert!(endpoints.connection(&remote).is_none());
                assert_eq!(
                    endpoints.connection(&ClientEndpointId::Local).is_some(),
                    local_online
                );
                assert!(state
                    .shell
                    .as_ref()
                    .unwrap()
                    .endpoint_is_active(&ClientEndpointId::Local));
                assert!(!state.shell.as_ref().unwrap().has_presented_surface());
                assert!(state.presentation_frozen);
                assert_eq!(catalog.selected_profile, None);
                assert_eq!(remote_disconnects.load(Ordering::Relaxed), 1);
                assert_eq!(local_disconnects.load(Ordering::Relaxed), 0);
                assert!(!supervisors.record_status(
                    &remote,
                    2,
                    endpoint::ClientEndpointStatus::Online,
                    now
                ));
            }
        }
    }

    #[test]
    fn a_machine_deleted_on_the_server_disconnects_with_a_notice_and_no_reconnect() {
        use crate::client::locations::sync::{derived_profile_id, sync_with, MachineCache};
        let _guard = crate::config::test_config_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let old = std::env::var_os("XDG_STATE_HOME");
        let base =
            std::env::temp_dir().join(format!("herdr-deleted-machine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::env::set_var("XDG_STATE_HOME", &base);
        let result = std::panic::catch_unwind(|| {
            const SERVER: &str = "https://hangar.test";
            const ID: &str = "m_agqp6jaaa6kqkitog6zzqzdfhy";
            let machine = |state: &str| {
                serde_json::from_value::<crate::hangar::api::Machine>(serde_json::json!({
                    "id": ID, "name": "box", "state": state
                }))
                .unwrap()
            };
            sync_with(&[SERVER.to_owned()], &|_| Ok(vec![machine("running")]));
            let profiles = crate::client::locations::effective_profiles().unwrap();
            assert_eq!(profiles.len(), 1);
            let remote = ClientEndpointId::Ssh(profiles[0].id.clone());
            assert_eq!(profiles[0].id, derived_profile_id(SERVER, ID));
            let now = Instant::now();
            let mut state = state();
            let mut catalog = EndpointCatalog::default();
            catalog.ssh = profiles.clone();
            state
                .shell
                .as_mut()
                .unwrap()
                .set_endpoint_catalog(&catalog.ssh);
            let mut supervisors = EndpointSupervisors::new(&catalog.ssh, now);
            let mut endpoints = EndpointRegistry::empty();
            let disconnects = Arc::new(AtomicUsize::new(0));
            endpoints.insert(
                remote.clone(),
                Transport(disconnects.clone()),
                2,
                Default::default(),
                true,
            );
            let mut commands = endpoint_commands::EndpointCommands::default();
            // A failed fetch never removes it.
            sync_with(&[SERVER.to_owned()], &|_| {
                Err(crate::hangar::api::HangarError::NotSignedIn)
            });
            let unchanged = crate::client::locations::effective_profiles().unwrap();
            assert_eq!(unchanged, profiles);
            // A stopped machine stays listed but stops connecting.
            sync_with(&[SERVER.to_owned()], &|_| Ok(vec![machine("stopped")]));
            let stopped = crate::client::locations::effective_profiles().unwrap();
            assert_eq!(stopped.len(), 1);
            assert!(!stopped[0].enabled);
            // Deleted on the server: gone from the list, so its endpoint is retired.
            sync_with(&[SERVER.to_owned()], &|_| Ok(Vec::new()));
            let after = crate::client::locations::effective_profiles().unwrap();
            assert!(after.is_empty());
            apply_profiles(
                &mut state,
                &mut endpoints,
                &mut commands,
                &mut supervisors,
                &mut catalog,
                after,
                now,
            );
            assert!(endpoints.connection(&remote).is_none());
            assert_eq!(disconnects.load(Ordering::Relaxed), 1);
            assert_eq!(
                state.shell.as_ref().unwrap().endpoint_notice(),
                Some("box: machine deleted on server")
            );
            assert!(!supervisors.record_status(
                &remote,
                2,
                endpoint::ClientEndpointStatus::Online,
                now
            ));
            assert!(MachineCache::load().unwrap().servers[SERVER]
                .machines
                .is_empty());
        });
        match old {
            Some(value) => std::env::set_var("XDG_STATE_HOME", value),
            None => std::env::remove_var("XDG_STATE_HOME"),
        }
        let _ = std::fs::remove_dir_all(&base);
        result.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
    }
}
