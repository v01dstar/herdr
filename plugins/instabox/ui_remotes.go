package main

import (
	"fmt"
	"sort"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
)

type rowKind int

const (
	rowLocal rowKind = iota
	rowInstabox
	rowSSH
	rowAdd
)

type row struct {
	kind    rowKind
	machine Machine
	profile *Profile // the herdr profile, when there is one
}

type actKind int

const (
	aDefault actKind = iota
	aTest
	aEdit
	aStartSession
	aRemoveRemote
	aStart
	aResume
	aSuspend
	aStop
	aHide
	aShow
	aCopy
	aDelete
	aNewFromSnapshot
	aDeleteSnapshot
	aSignIn
	aSwitch
	aSignOut
	aSignInGoogle
)

type action struct {
	kind   actKind
	label  string
	group  int
	reason string // set when the action applies but is blocked now
}

// rows lists Local, instabox machines by name, the user's own SSH remotes and the
// Add row, like herdr's remotes settings.
func (m model) rows() []row {
	managed, ssh := splitProfiles(m.profiles)
	rows := []row{{kind: rowLocal}}
	machines := append([]Machine(nil), m.machines...)
	sort.SliceStable(machines, func(i, j int) bool {
		a, b := m.st.label(machines[i]), m.st.label(machines[j])
		if a != b {
			return a < b
		}
		return machines[i].ID < machines[j].ID
	})
	for _, mc := range machines {
		r := row{kind: rowInstabox, machine: mc}
		if p, ok := managed[mc.ID]; ok {
			r.profile = &p
		}
		rows = append(rows, r)
	}
	for i := range ssh {
		rows = append(rows, row{kind: rowSSH, profile: &ssh[i]})
	}
	return append(rows, row{kind: rowAdd})
}

func (r row) key() string {
	switch r.kind {
	case rowInstabox:
		return "instabox:" + r.machine.ID
	case rowSSH:
		return "ssh:" + r.profile.ID
	case rowAdd:
		return "add"
	}
	return "local"
}

func (m model) selectedRow() (row, bool) {
	rows := m.rows()
	if m.sel < len(rows) {
		return rows[m.sel], true
	}
	return row{}, false
}

func (m model) isDefault(r row) bool {
	switch r.kind {
	case rowLocal:
		return m.st.Default == "" || !m.defaultValid()
	case rowInstabox:
		return m.st.Default == "instabox:"+r.machine.ID
	case rowSSH:
		return m.st.Default == "ssh:"+r.profile.ID
	}
	return false
}

func (m model) defaultValid() bool {
	for _, r := range m.rows() {
		if r.kind != rowLocal && r.kind != rowAdd && m.isDefault(r) {
			return true
		}
	}
	return false
}

var waitReasons = map[string]string{
	"creating":   "Wait until the machine is created",
	"starting":   "Wait until the machine has started",
	"stopping":   "Wait until the machine has stopped",
	"suspending": "Wait until the machine is suspended",
	"resuming":   "Wait until the machine has resumed",
	"deleting":   "The machine is being deleted",
	"deleted":    "The machine was deleted",
}

func (m model) remoteActions() []action {
	r, ok := m.selectedRow()
	if !ok {
		return nil
	}
	defaultReason := ""
	if m.isDefault(r) {
		defaultReason = "Already the default for new workspaces"
	}
	switch r.kind {
	case rowLocal:
		return []action{{kind: aDefault, label: "Use as default", reason: defaultReason}}
	case rowSSH:
		test := action{kind: aTest, label: "Test connection"}
		if !r.profile.Enabled {
			test.reason = "Disabled; Start session enables it"
		}
		return []action{
			test,
			{kind: aEdit, label: "Edit…"},
			{kind: aDefault, label: "Use as default", reason: defaultReason},
			{kind: aStartSession, label: "Start session"},
			{kind: aRemoveRemote, label: "Remove remote", group: 1},
		}
	case rowInstabox:
		return m.instaboxActions(r, defaultReason)
	}
	return nil
}

func (m model) instaboxActions(r row, defaultReason string) []action {
	mc := r.machine
	server := func(reason string) string {
		switch {
		case m.note == "signed out":
			return "Sign in on the account tab first"
		case m.note != "":
			return "instabox is unreachable; showing the last synced state"
		case m.busyOn(mc.ID) != nil:
			return "Wait for the current operation on this machine to finish"
		}
		return reason
	}
	wait := waitReasons[mc.State]
	var acts []action
	add := func(kind actKind, label string, group int, reason string) {
		acts = append(acts, action{kind: kind, label: label, group: group, reason: reason})
	}
	switch mc.State {
	case "running":
		add(aSuspend, "Suspend…", 0, server(""))
		add(aStop, "Stop…", 0, server(""))
	case "stopped":
		add(aStart, "Start", 0, server(""))
	case "suspended":
		add(aResume, "Resume", 0, server(""))
	case "creating", "starting", "resuming":
		add(aSuspend, "Suspend…", 0, server(wait))
		add(aStop, "Stop…", 0, server(wait))
	case "stopping":
		add(aStart, "Start", 0, server(wait))
	case "suspending":
		add(aResume, "Resume", 0, server(wait))
	case "deleting", "deleted":
	default: // error, failed, unknown
		add(aStart, "Start", 0, server(""))
		add(aStop, "Stop…", 0, server(""))
	}

	test := ""
	switch {
	case m.st.Hidden[mc.ID]:
		test = "Hidden from the sidebar; Show in sidebar first"
	case mc.State == "running" && (r.profile == nil || !r.profile.Enabled):
		test = "Not connected yet; try again shortly"
		if e := m.st.AddError[mc.ID]; e != "" {
			test = "Not connected: " + e
		}
	case mc.State != "running":
		test = "Start the machine first"
	}
	add(aTest, "Test connection", 1, test)
	add(aEdit, "Edit…", 1, "")
	add(aDefault, "Use as default", 1, defaultReason)
	if m.st.Hidden[mc.ID] {
		add(aShow, "Show in sidebar", 1, "")
	} else {
		add(aHide, "Hide from sidebar", 1, "")
	}

	copyReason := wait
	if t, ok := templateFor(m.templates, mc.Template); ok && !t.Has("identity-reset") {
		copyReason = "Template too old to copy"
	}
	add(aCopy, "Copy machine…", 2, server(copyReason))
	deleteReason := ""
	if mc.State == "deleting" || mc.State == "deleted" {
		deleteReason = wait
	}
	add(aDelete, "Delete machine…", 3, server(deleteReason))
	return acts
}

func (m model) runRemoteAction() (tea.Model, tea.Cmd) {
	acts := m.remoteActions()
	if m.act >= len(acts) {
		return m, nil
	}
	a := acts[m.act]
	if a.reason != "" {
		m.say(msgErr, "%s: %s.", strings.TrimSuffix(a.label, "…"), a.reason)
		return m, nil
	}
	r, _ := m.selectedRow()
	mc := r.machine
	label := m.st.label(mc)
	switch a.kind {
	case aDefault:
		value, where := "", "this computer"
		switch r.kind {
		case rowInstabox:
			value, where = "instabox:"+mc.ID, label+stateSuffix(mc.State)
		case rowSSH:
			value, where = "ssh:"+r.profile.ID, r.profile.Label
		}
		return m, quick(false, func() (string, error) {
			err := updateState(func(s *State) { s.Default = value })
			return "New workspace defaults to " + where + ".", err
		})

	case aTest:
		p := r.profile
		m.say(msgInfo, "Testing %s…", p.Label)
		return m, quick(false, func() (string, error) { return testConnection(*p) })

	case aStartSession:
		p := *r.profile
		m.say(msgInfo, "Starting %s…", p.Label)
		return m, quick(true, func() (string, error) {
			if !p.Enabled {
				if _, err := herdr("machine", "enable", p.ID); err != nil {
					return "", err
				}
			}
			status, detail, err := machineStatus(p.ID)
			if err != nil {
				return "", err
			}
			if status != "reachable" {
				return "", fmt.Errorf("%s is %s%s. Run `herdr --remote %s` in a terminal to set it up", p.Label, status, colonDetail(detail), p.Target)
			}
			return "Remote is ready; automatic connection enabled.", nil
		})

	case aRemoveRemote:
		p := *r.profile
		return m, quick(true, func() (string, error) {
			if _, err := herdr("machine", "remove", p.ID); err != nil {
				return "", err
			}
			err := updateState(func(s *State) {
				if s.Default == "ssh:"+p.ID {
					s.Default = ""
				}
			})
			return "Removed " + p.Label + ".", err
		})

	case aEdit:
		return m.openEdit(r)

	case aStart:
		return m.launch(Job{Kind: jobStart, MachineID: mc.ID, MachineName: label})
	case aResume:
		return m.launch(Job{Kind: jobResume, MachineID: mc.ID, MachineName: label})
	case aSuspend:
		m.dialog = confirmDialog("suspend machine", "suspend", fmt.Sprintf(
			"Suspend instabox machine %s? Its memory is saved to a snapshot, so running programs and Herdr sessions continue after Resume. (Stop… shuts everything down instead; only files on disk remain.) It does not reconnect until it is resumed.", label),
			Job{Kind: jobSuspend, MachineID: mc.ID, MachineName: label})
		return m, nil
	case aStop:
		m.dialog = confirmDialog("stop machine", "stop", fmt.Sprintf(
			"Stop instabox machine %s? All sessions and jobs on this machine stop. Files on its persistent disk remain; running processes do not survive (Suspend… keeps them). It does not reconnect until it is started again.", label),
			Job{Kind: jobStop, MachineID: mc.ID, MachineName: label})
		return m, nil
	case aDelete:
		m.dialog = confirmDialog("delete machine", "delete", fmt.Sprintf(
			"Delete instabox machine '%s'? The machine, its disks and snapshots are permanently deleted, with every file and process on it. This cannot be undone. Herdr closes its workspaces and stops listing it.", label),
			Job{Kind: jobDelete, MachineID: mc.ID, MachineName: label})
		return m, nil

	case aHide, aShow:
		hide := a.kind == aHide
		return m, quick(true, func() (string, error) {
			if err := updateState(func(s *State) {
				if hide {
					s.Hidden[mc.ID] = true
				} else {
					delete(s.Hidden, mc.ID)
				}
			}); err != nil {
				return "", err
			}
			if err := reconcileMachine(mc); err != nil {
				return "", err
			}
			if hide {
				return label + " is hidden from the sidebar and not connected. It stays listed here; Show in sidebar brings it back.", nil
			}
			return label + " is shown in the sidebar again and connects while it is running.", nil
		})

	case aCopy:
		m.dialog = &dialog{chooser: &chooser{machine: mc, label: label}}
		return m, nil
	}
	return m, nil
}

func testConnection(p Profile) (string, error) {
	status, detail, err := machineStatus(p.ID)
	if err != nil {
		return "", err
	}
	if status == "reachable" {
		return "SSH and Herdr session are ready.", nil
	}
	return "", fmt.Errorf("%s: %s%s", p.Label, status, colonDetail(detail))
}

func colonDetail(s string) string {
	if s == "" {
		return ""
	}
	return ": " + s
}

func stateSuffix(state string) string {
	if state == "running" || state == "" {
		return ""
	}
	return " (" + state + ")"
}

// stateWord is the coloured state shown on an instabox row.
func (m model) stateWord(r row) (string, string) {
	mc := r.machine
	switch {
	case m.note != "":
		return m.note, "muted"
	case mc.State == "running" && m.st.fenced(mc.ID) && !m.st.Hidden[mc.ID]:
		return "stopping…", "warn"
	case mc.State == "running":
		return "running", "ok"
	case mc.State == "error" || mc.State == "failed":
		return "error", "err"
	case waitReasons[mc.State] != "" && mc.State != "deleted":
		return mc.State + "…", "warn"
	case mc.State == "" || mc.State == "unknown":
		return "unknown", "muted"
	}
	return mc.State, "muted"
}

// detailLines is the right pane's header and info for the selected row.
func (m model) detailLines(r row) (string, []string) {
	switch r.kind {
	case rowLocal:
		lines := []string{}
		if m.isDefault(r) {
			lines = append(lines, "New workspaces open here by default.")
		}
		return "Local · this computer", lines
	case rowAdd:
		return "Add remote", []string{"Create an instabox machine, from the herdr template or one of your snapshots, or add an SSH remote. Press ↵ or click to start."}
	case rowSSH:
		p := r.profile
		state := "disabled"
		if p.Enabled {
			state = "enabled"
		}
		info := p.Target
		if p.Session != "" {
			info += " · session " + p.Session
		}
		return p.Label + " · ssh · " + state, []string{info}
	}
	mc := r.machine
	state := mc.State
	if word, _ := m.stateWord(r); word == "stopping…" {
		state = "stopping"
	} else if state == "" || state == "unknown" {
		state = "state unknown"
	}
	parts := []string{}
	if mc.Template != nil {
		parts = append(parts, mc.Template.Label())
	}
	parts = append(parts, fmt.Sprintf("%d vCPU", mc.Spec.VCPUs), memText(mc.Spec.MemMiB))
	if mc.Spec.RootDiskGiB > 0 {
		parts = append(parts, fmt.Sprintf("%d GiB root", mc.Spec.RootDiskGiB))
	}
	parts = append(parts, fmt.Sprintf("%d GiB /data", mc.Spec.PersistentDiskGiB))
	if mc.Snapshot != nil {
		parts = append(parts, "from snapshot "+m.snapshotName(mc.Snapshot.ID))
	}
	if m.note != "" {
		parts = append(parts, m.note+": last synced state")
	}
	if (mc.State == "error" || mc.State == "failed") && mc.LastError != nil && mc.LastError.Message != "" {
		parts = append(parts, "last error: "+mc.LastError.Message)
	}
	lines := []string{strings.Join(parts, " · ")}
	if m.st.Hidden[mc.ID] {
		lines = append(lines, "Hidden from the sidebar.")
	}
	if j := m.busyOn(mc.ID); j != nil && j.Progress != "" {
		lines = append(lines, j.Progress)
	}
	return m.st.label(mc) + " · instabox · " + state, lines
}

func (m model) snapshotByID(id string) (Snapshot, bool) {
	for _, im := range m.snapshots {
		if im.ID == id {
			return im, true
		}
	}
	return Snapshot{}, false
}

func (m model) snapshotName(id string) string {
	if im, ok := m.snapshotByID(id); ok {
		return im.Name
	}
	return id
}

func memText(mib int) string {
	if mib%1024 == 0 {
		return fmt.Sprintf("%d GiB RAM", mib/1024)
	}
	return fmt.Sprintf("%d MiB RAM", mib)
}
