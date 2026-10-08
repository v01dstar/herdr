package main

import (
	"errors"
	"fmt"
	"os"
	"strings"
	"time"
)

// Reconciliation keeps herdr's saved machines in line with instabox: every machine
// that is running, not hidden and not fenced is in herdr and enabled; other
// machines are disabled (still listed in the sidebar) or, when hidden or
// deleted, removed. Signed out, every local trace of instabox is removed. instabox
// machines themselves are never changed here.

const (
	addRetry       = time.Minute
	staleSyncAfter = time.Minute
)

// syncResult reports what one reconciliation saw, for the settings view.
type syncResult struct {
	Account  Account
	Machines []Machine
	Profiles []Profile
	// Note is set when the machine list could not be refreshed ("signed out",
	// "offline"); Machines is then empty.
	Note string
	Err  error
}

// syncAll reconciles everything. Several processes may call it at once; they
// take turns.
func syncAll() syncResult {
	var r syncResult
	if err := withMachinesLock(func() error { r = syncLocked(); return nil }); err != nil {
		r.Err = err
	}
	return r
}

func syncLocked() syncResult {
	var r syncResult
	r.Account = whoami()
	_ = updateState(func(s *State) { s.LastSync = time.Now() })
	switch {
	case r.Account.SignedOut:
		r.Note = "signed out"
		if hasLocalTraces() {
			r.Err = cleanupLocalLocked()
		}
	case r.Account.Err != nil:
		r.Note = "offline"
		r.Err = r.Account.Err
	default:
		_ = updateState(func(s *State) { s.LastAccount = r.Account.Key() })
		r.Machines, r.Err = listMachines()
		if r.Err != nil {
			r.Note = "offline"
		} else {
			r.Err = reconcile(r.Machines)
		}
	}
	profiles, err := listProfiles()
	r.Profiles = profiles
	if r.Err == nil {
		r.Err = err
	}
	return r
}

// syncIfStale is the cheap entry for frequent triggers such as workspace events.
func syncIfStale() error {
	// Claim the sync atomically, so a burst of events runs it once.
	stale := false
	if err := updateState(func(s *State) {
		if time.Since(s.LastSync) >= staleSyncAfter {
			stale = true
			s.LastSync = time.Now()
		}
	}); err != nil || !stale {
		return err
	}
	return syncAll().Err
}

func reconcile(machines []Machine) error {
	all, err := listProfiles()
	if err != nil {
		return err
	}
	managed, _ := splitProfiles(all)
	st := readState()
	var errs []error
	for _, p := range duplicateProfiles(all) {
		if _, err := herdr("machine", "remove", p.ID); err != nil {
			errs = append(errs, err)
		}
	}
	seen := map[string]bool{}
	for _, m := range machines {
		seen[m.ID] = true
		p, has := managed[m.ID]
		if err := reconcileOne(m, p, has, st); err != nil {
			errs = append(errs, fmt.Errorf("%s: %w", st.label(m), err))
		}
	}
	for id, p := range managed {
		if seen[id] {
			continue
		}
		// Deleted on the server.
		if _, err := herdr("machine", "remove", p.ID); err != nil {
			errs = append(errs, err)
		}
		removeHostBlock(id)
		_ = updateState(func(s *State) { s.forget(id) })
	}
	// Drop what is kept about machines that no longer exist.
	_ = updateState(func(s *State) {
		var ids []string
		for id := range s.Hidden {
			ids = append(ids, id)
		}
		for id := range s.Labels {
			ids = append(ids, id)
		}
		for id := range s.Fence {
			ids = append(ids, id)
		}
		if id, ok := strings.CutPrefix(s.Default, "instabox:"); ok {
			ids = append(ids, id)
		}
		for _, id := range ids {
			if !seen[id] {
				s.forget(id)
			}
		}
	})
	return errors.Join(errs...)
}

func reconcileOne(m Machine, p Profile, has bool, st State) error {
	label := st.label(m)
	want := m.State == "running" && !st.Hidden[m.ID] && !st.fenced(m.ID)
	switch {
	case st.Hidden[m.ID]:
		if has {
			_, err := herdr("machine", "remove", p.ID)
			return err
		}
		return nil
	case want && !has:
		if time.Now().Before(st.RetryAdd[m.ID]) {
			return nil
		}
		return addMachine(m, label)
	case want && !p.Enabled:
		if err := ensureHostBlock(m); err != nil {
			return err
		}
		if _, err := herdr("machine", "enable", p.ID); err != nil {
			return err
		}
	case !want && has && p.Enabled:
		if _, err := herdr("machine", "disable", p.ID); err != nil {
			return err
		}
	}
	if has && p.Label != label {
		if _, err := herdr("machine", "rename", p.ID, "--label", label); err != nil {
			return err
		}
	}
	if m.State != "running" && st.fenced(m.ID) {
		// The fence has done its job once instabox no longer reports it running.
		_ = updateState(func(s *State) { delete(s.Fence, m.ID) })
	}
	return nil
}

// addMachine saves a running machine in herdr. `herdr machine add` also starts
// the remote Herdr session, which is how a freshly started machine gets one.
func addMachine(m Machine, label string) error {
	if _, err := withInclude(""); err != nil {
		return err
	}
	if err := writeHostBlock(m); err != nil {
		return err
	}
	_, err := herdr("machine", "add", m.Target(), "--label", label, "--remote-session", RemoteSession)
	_ = updateState(func(s *State) {
		if err != nil {
			s.RetryAdd[m.ID] = time.Now().Add(addRetry)
			s.AddError[m.ID] = err.Error()
		} else {
			delete(s.RetryAdd, m.ID)
			delete(s.AddError, m.ID)
		}
	})
	return err
}

// readdMachine replaces a machine's herdr profile, so herdr starts the remote
// session again (after Start, the old session is gone).
func readdMachine(m Machine) error {
	return withMachinesLock(func() error { return readdLocked(m) })
}

func readdLocked(m Machine) error {
	all, err := listProfiles()
	if err != nil {
		return err
	}
	for _, p := range all {
		if id, ok := p.MachineID(); ok && id == m.ID {
			if _, err := herdr("machine", "remove", p.ID); err != nil {
				return err
			}
		}
	}
	st := readState()
	if st.Hidden[m.ID] {
		return nil
	}
	return addMachine(m, st.label(m))
}

func ensureHostBlock(m Machine) error {
	if hasHostBlock(m.ID) {
		return nil
	}
	if _, err := withInclude(""); err != nil {
		return err
	}
	return writeHostBlock(m)
}

// disconnect disables a machine in herdr right away and fences it, so herdr
// stops reconnecting before the machine goes away.
func disconnect(id string) error {
	_ = updateState(func(s *State) { s.Fence[id] = time.Now().Add(fenceFor) })
	return withMachinesLock(func() error {
		all, err := listProfiles()
		if err != nil {
			return err
		}
		for _, p := range all {
			if mid, ok := p.MachineID(); ok && mid == id && p.Enabled {
				if _, err := herdr("machine", "disable", p.ID); err != nil {
					return err
				}
			}
		}
		return nil
	})
}

// forgetMachine removes a deleted machine from herdr and the plugin state.
func forgetMachine(id string) error {
	return withMachinesLock(func() error {
		all, err := listProfiles()
		if err != nil {
			return err
		}
		for _, p := range all {
			if mid, ok := p.MachineID(); ok && mid == id {
				if _, err := herdr("machine", "remove", p.ID); err != nil {
					return err
				}
			}
		}
		removeHostBlock(id)
		return updateState(func(s *State) { s.forget(id) })
	})
}

// hasLocalTraces reports whether a sign-out cleanup has anything to do.
func hasLocalTraces() bool {
	if sshIncluded() {
		return true
	}
	if entries, err := os.ReadDir(hostsDir()); err == nil && len(entries) > 0 {
		return true
	}
	all, err := listProfiles()
	if err != nil {
		return false
	}
	managed, _ := splitProfiles(all)
	return len(managed) > 0
}

// cleanupLocal removes the managed herdr machines, their Host blocks and the
// Include line. Hidden machines, names and the default are kept for the next
// sign-in; machines on the server are untouched.
func cleanupLocal() error { return withMachinesLock(cleanupLocalLocked) }

func cleanupLocalLocked() error {
	var errs []error
	if all, err := listProfiles(); err != nil {
		errs = append(errs, err)
	} else {
		for _, p := range all {
			if _, ok := p.MachineID(); !ok {
				continue
			}
			if _, err := herdr("machine", "remove", p.ID); err != nil {
				errs = append(errs, fmt.Errorf("remove %s from herdr: %w", p.Label, err))
			}
		}
	}
	if err := os.RemoveAll(hostsDir()); err != nil {
		errs = append(errs, err)
	}
	if err := removeInclude(); err != nil {
		errs = append(errs, fmt.Errorf("~/.ssh/config: %w", err))
	}
	return errors.Join(errs...)
}

// withInclude makes sure ~/.ssh/config includes the managed Host blocks, and
// mentions it in the status when it had to add the line.
func withInclude(status string) (string, error) {
	if sshIncluded() {
		return status, nil
	}
	if err := addInclude(); err != nil {
		return "", fmt.Errorf("could not update ~/.ssh/config: %w", err)
	}
	if status == "" {
		return "", nil
	}
	return status + " ~/.ssh/config now includes the instabox hosts.", nil
}
