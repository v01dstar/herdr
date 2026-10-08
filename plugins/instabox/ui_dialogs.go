package main

import (
	"fmt"
	"strings"

	"github.com/charmbracelet/bubbles/textinput"
	tea "github.com/charmbracelet/bubbletea"
)

// A dialog is one of: a confirmation, a form, or the Copy machine chooser.
type dialog struct {
	confirm *confirm
	form    *form
	chooser *chooser
}

type confirm struct {
	title, verb, body string
	job               *Job
	run               func(m *model) tea.Cmd
}

func confirmDialog(title, verb, body string, j Job) *dialog {
	return &dialog{confirm: &confirm{title: title, verb: verb, body: body, job: &j}}
}

type chooser struct {
	machine Machine
	label   string
	pick    int // 0 clone, 1 snapshot
}

type formKind int

const (
	formAdd formKind = iota
	formSSH
	formEdit
	formClone
	formSnapshot
)

type choice struct {
	options []string
	values  []string
	sel     int
	open    bool
	hover   int
}

type field struct {
	label  string
	input  textinput.Model
	choice *choice
}

type form struct {
	kind    formKind
	title   string
	primary string
	fields  []field
	focus   int // len(fields) is the primary button
	text    string
	err     string
	check   string // Copy: result of checking the machine
	plan    string // Copy: "", "save", "stop", "resume-stop", "none"
	machine Machine
	label   string
	row     row
}

func newForm(kind formKind, title, primary string) *form {
	return &form{kind: kind, title: title, primary: primary}
}

func (f *form) addText(label, value, placeholder string) {
	in := textinput.New()
	in.Prompt = ""
	in.Placeholder = placeholder
	in.SetValue(value)
	in.CharLimit = 1000
	f.fields = append(f.fields, field{label: label, input: in})
	f.syncFocus()
}

func (f *form) addChoice(label string, options, values []string, sel int) {
	f.fields = append(f.fields, field{label: label, choice: &choice{options: options, values: values, sel: sel}})
	f.syncFocus()
}

func (f *form) value(i int) string {
	if i >= len(f.fields) {
		return ""
	}
	if c := f.fields[i].choice; c != nil {
		return c.values[c.sel]
	}
	return strings.TrimSpace(f.fields[i].input.Value())
}

func (f *form) syncFocus() {
	for i := range f.fields {
		if f.fields[i].choice != nil {
			continue
		}
		if i == f.focus {
			f.fields[i].input.Focus()
		} else {
			f.fields[i].input.Blur()
		}
	}
}

func (f *form) openChoice() *choice {
	for i := range f.fields {
		if c := f.fields[i].choice; c != nil && c.open {
			return c
		}
	}
	return nil
}

// refreshPrimary labels the Copy button after the machine check.
func (f *form) refreshPrimary() {
	verb := "clone"
	if f.kind == formSnapshot {
		verb = "save"
	}
	switch f.plan {
	case "stop", "resume-stop":
		f.primary = "stop machine and " + verb
	default:
		f.primary = verb
	}
}

// ---- opening dialogs ----

func (m model) openAdd(snapshotID string) (tea.Model, tea.Cmd) {
	f := newForm(formAdd, "add remote", "create and connect")
	f.addChoice("Provider", []string{"instabox", "SSH"}, []string{"instabox", "ssh"}, 0)
	f.addText("Name", "", "my-machine")
	options, values, sel := []string{"Latest herdr template"}, []string{""}, 0
	for _, im := range m.snapshots {
		from := "a deleted machine"
		for _, mc := range m.machines {
			if mc.ID == im.SourceMachineID {
				from = m.st.label(mc)
			}
		}
		options = append(options, fmt.Sprintf("%s · %s · from %s", im.Name, im.CreatedAt.Local().Format("2006-01-02"), from))
		values = append(values, im.ID)
		if im.ID == snapshotID {
			sel = len(values) - 1
		}
	}
	f.addChoice("Source", options, values, sel)
	f.focus = 1
	f.syncFocus()
	m.dialog = &dialog{form: f}
	m.tab = tabRemotes
	return m, nil
}

func (m model) addHint(f *form) string {
	switch {
	case !m.acct.SignedIn():
		return "Not signed in to instabox. Press sign in to continue in your browser."
	case f.value(2) != "":
		if sn, ok := m.snapshotByID(f.value(2)); ok && !sn.HasData() {
			return fmt.Sprintf("Creates an instabox machine from snapshot %s: the packages and system configuration saved in it, with a new, empty /data (no home directory files, logins or workspace).", sn.Name)
		}
		return fmt.Sprintf("Creates an instabox machine from snapshot %s: its software, settings, and the repos, home files and logins on /data.", m.snapshotName(f.value(2)))
	}
	return "Creates an instabox machine from the herdr template with its default size and a persistent disk. Closing Herdr keeps it running; stop it from instabox settings. Your existing instabox machines are listed on the remotes tab on their own."
}

func sshForm() *form {
	f := newForm(formSSH, "add remote", "add")
	f.addChoice("Provider", []string{"instabox", "SSH"}, []string{"instabox", "ssh"}, 1)
	f.addText("Name", "", "shown in the sidebar")
	f.addText("SSH target", "", "an SSH alias or user@host")
	f.addText("Herdr session", "", "default")
	f.text = "Use an existing SSH alias. instabox machines appear on their own; create one with Provider: instabox. Herdr checks the remote and asks before installing anything."
	f.focus = 2
	f.syncFocus()
	return f
}

func (m model) openEdit(r row) (tea.Model, tea.Cmd) {
	f := newForm(formEdit, "remote settings", "save")
	f.row = r
	if r.kind == rowInstabox {
		f.addText("Name", m.st.label(r.machine), r.machine.Name)
		f.text = "This remote is an instabox machine; the plugin manages its SSH target and session. The name is shown only in Herdr (empty restores the instabox name)."
	} else {
		f.addText("Name", r.profile.Label, "")
		f.text = fmt.Sprintf("SSH target %s · session %s. To change them, remove the remote and add it again.", r.profile.Target, orDefault(r.profile.Session, "default"))
	}
	m.dialog = &dialog{form: f}
	return m, nil
}

func orDefault(s, d string) string {
	if s == "" {
		return d
	}
	return s
}

const (
	cloneContents    = "A clone copies everything: installed software, system settings, repos and home files on /data, and logins (gh, claude). It becomes a second machine exactly like this one."
	snapshotContents = "A snapshot saves the root disk and /data: installed software, system settings, repos, home files and logins (gh, claude). New machines start from it as copies of this one."
)

func (m model) openCopyForm(c *chooser) (tea.Model, tea.Cmd) {
	var f *form
	if c.pick == 0 {
		f = newForm(formClone, "clone "+c.label, "clone")
		name := c.machine.Name + "-clone"
		if len(name) > 63 {
			name = name[:63]
		}
		f.addText("Name", name, "")
		f.text = cloneContents
	} else {
		f = newForm(formSnapshot, "save "+c.label+" as snapshot", "save")
		f.addText("Snapshot name", "", "")
		f.addText("Description", "", "optional")
		f.text = snapshotContents
	}
	f.machine, f.label = c.machine, c.label
	f.check = "Checking " + c.label + "…"
	f.plan = ""
	f.refreshPrimary()
	m.dialog = &dialog{form: f}
	return m, checkForCopy(c.machine.ID, c.label, c.pick == 1, m.templates)
}

// checkForCopy decides what Copy has to do first, as herdr's check worker does.
func checkForCopy(id, label string, snapshot bool, templates []Template) tea.Cmd {
	return func() tea.Msg {
		verb, does, stopVerb := "cloned", "clones it", "Stop machine and clone"
		if snapshot {
			verb, does, stopVerb = "saved as a snapshot", "saves the snapshot", "Stop machine and save"
		}
		mc, err := getMachine(id)
		if err != nil {
			return checkMsg{id, "none", fmt.Sprintf("Could not check %s: %v Close and reopen Copy machine… to retry.", label, err)}
		}
		if t, ok := templateFor(templates, mc.Template); ok && !t.Has("identity-reset") {
			return checkMsg{id, "none", fmt.Sprintf("%s uses template %s, which is too old to be %s. Create a new machine from the latest template, set it up there, and copy that one.", label, mc.Template.Label(), verb)}
		}
		switch mc.State {
		case "stopped":
			if mc.Storage.Synced {
				return checkMsg{id, "save", ""}
			}
			return checkMsg{id, "stop", fmt.Sprintf("%s has changes that are not uploaded yet. %s stops it again to upload them, then %s.", label, stopVerb, does)}
		case "running", "error", "failed":
			return checkMsg{id, "stop", fmt.Sprintf("%s is running. Only a stopped machine can be %s. %s stops all sessions and jobs on it (as Stop… does), waits until it is stopped, then %s.", label, verb, stopVerb, does)}
		case "suspended":
			return checkMsg{id, "resume-stop", fmt.Sprintf("%s is suspended. Only a stopped machine can be %s. %s resumes it, stops all sessions and jobs on it, waits until it is stopped, then %s.", label, verb, stopVerb, does)}
		}
		return checkMsg{id, "none", fmt.Sprintf("%s is %s. Wait until it is stopped or running, then open Copy machine… again.", label, mc.State)}
	}
}

// ---- keys ----

func (m model) dialogKey(k tea.KeyMsg) (tea.Model, tea.Cmd) {
	d := m.dialog
	switch {
	case d.confirm != nil:
		switch k.String() {
		case "enter", "y":
			return m.acceptConfirm()
		case "esc", "n", "q":
			m.dialog = nil
		}
		return m, nil
	case d.chooser != nil:
		switch k.String() {
		case "left", "right", "h", "l", "tab", "shift+tab":
			d.chooser.pick = 1 - d.chooser.pick
		case "enter", " ":
			return m.openCopyForm(d.chooser)
		case "esc", "q":
			m.dialog = nil
		}
		return m, nil
	}
	return m.formKey(k)
}

func (m model) acceptConfirm() (tea.Model, tea.Cmd) {
	c := m.dialog.confirm
	m.dialog = nil
	if c.job != nil {
		return m.launch(*c.job)
	}
	return m, c.run(&m)
}

func (m model) formKey(k tea.KeyMsg) (tea.Model, tea.Cmd) {
	f := m.dialog.form
	if c := f.openChoice(); c != nil {
		switch k.String() {
		case "up", "k":
			c.hover = max(0, c.hover-1)
		case "down", "j":
			c.hover = min(len(c.options)-1, c.hover+1)
		case "enter", " ":
			return m.pickChoice(c, c.hover)
		case "esc":
			c.open = false
		}
		return m, nil
	}
	n := len(f.fields) + 1
	switch k.String() {
	case "esc":
		if f.kind == formClone || f.kind == formSnapshot {
			m.dialog = &dialog{chooser: &chooser{machine: f.machine, label: f.label, pick: map[bool]int{true: 1}[f.kind == formSnapshot]}}
		} else {
			m.dialog = nil
		}
		return m, nil
	case "tab", "down":
		f.focus = (f.focus + 1) % n
		f.syncFocus()
		return m, nil
	case "shift+tab", "up":
		f.focus = (f.focus + n - 1) % n
		f.syncFocus()
		return m, nil
	case "enter":
		if f.focus < len(f.fields) && f.fields[f.focus].choice != nil {
			c := f.fields[f.focus].choice
			c.open, c.hover = true, c.sel
			return m, nil
		}
		return m.submit()
	}
	if f.focus < len(f.fields) {
		fl := &f.fields[f.focus]
		if fl.choice != nil {
			if s := k.String(); s == " " || s == "right" {
				fl.choice.open, fl.choice.hover = true, fl.choice.sel
			}
			return m, nil
		}
		var cmd tea.Cmd
		fl.input, cmd = fl.input.Update(k)
		f.err = ""
		return m, cmd
	}
	return m, nil
}

func (m model) pickChoice(c *choice, i int) (tea.Model, tea.Cmd) {
	c.sel, c.open = i, false
	f := m.dialog.form
	// Switching the provider swaps the instabox form for the SSH one.
	if f.kind == formAdd && c == f.fields[0].choice && c.values[i] == "ssh" {
		m.dialog = &dialog{form: sshForm()}
	} else if f.kind == formSSH && c == f.fields[0].choice && c.values[i] == "instabox" {
		return m.openAdd("")
	}
	return m, nil
}

// ---- submit ----

func (m model) submit() (tea.Model, tea.Cmd) {
	f := m.dialog.form
	nameRule := func(what string) string {
		return what + " names use 1–63 lowercase letters, digits or '-', starting with a letter or digit."
	}
	switch f.kind {
	case formAdd:
		if !m.acct.SignedIn() {
			m.dialog = nil
			return m.signIn("")
		}
		name := f.value(1)
		switch {
		case name == "":
			f.err = "Enter a name for the new machine."
			return m, nil
		case !validName(name):
			f.err = nameRule("Machine")
			return m, nil
		}
		snapshot := f.value(2)
		if snapshot != "" && m.snapshotName(snapshot) == snapshot {
			f.err = "The chosen snapshot is no longer listed. Choose another source."
			return m, nil
		}
		m.dialog = nil
		return m.launch(Job{Kind: jobCreate, Name: name, Snapshot: snapshot, SnapshotName: m.snapshotName(snapshot), MachineName: name})

	case formSSH:
		label, target, session := f.value(1), f.value(2), f.value(3)
		switch {
		case target == "":
			f.err = "Enter an SSH target."
			return m, nil
		case strings.HasPrefix(target, TargetPrefix) || strings.HasPrefix(target, "instabox-m_"):
			f.err = "instabox machines appear on their own once you sign in; create one with Provider: instabox."
			return m, nil
		}
		args := []string{"machine", "add", target}
		if label != "" {
			args = append(args, "--label", label)
		}
		if session != "" {
			args = append(args, "--remote-session", session)
		}
		m.dialog = nil
		return m, interactive(herdrBin(), args, func() (string, error) {
			return "Added " + orDefault(label, target) + ".", nil
		})

	case formEdit:
		name := f.value(0)
		r := f.row
		m.dialog = nil
		if r.kind == rowInstabox {
			mc := r.machine
			return m, quick(true, func() (string, error) {
				if err := updateState(func(s *State) {
					if name == "" || name == mc.Name {
						delete(s.Labels, mc.ID)
					} else {
						s.Labels[mc.ID] = name
					}
				}); err != nil {
					return "", err
				}
				return "Saved.", reconcileMachine(mc)
			})
		}
		p := *r.profile
		if name == "" || name == p.Label {
			return m, nil
		}
		return m, quick(true, func() (string, error) {
			_, err := herdr("machine", "rename", p.ID, "--label", name)
			return "Saved.", err
		})

	case formClone, formSnapshot:
		if f.plan == "" || f.plan == "none" {
			return m, nil
		}
		name := f.value(0)
		if !validName(name) {
			f.err = nameRule(map[bool]string{true: "Machine", false: "Snapshot"}[f.kind == formClone])
			return m, nil
		}
		if f.kind == formClone {
			m.dialog = nil
			return m.launch(Job{Kind: jobClone, MachineID: f.machine.ID, MachineName: f.label, Name: name})
		}
		desc := f.value(1)
		if len([]rune(desc)) > 1000 {
			f.err = "The description can be at most 1000 characters."
			return m, nil
		}
		m.dialog = nil
		return m.launch(Job{Kind: jobSaveSnapshot, MachineID: f.machine.ID, MachineName: f.label, Name: name, Description: desc})
	}
	return m, nil
}
