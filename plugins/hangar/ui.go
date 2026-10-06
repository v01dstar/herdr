package main

import (
	"fmt"
	"os/exec"
	"strings"
	"time"

	tea "github.com/charmbracelet/bubbletea"
)

// The settings pane mirrors herdr's own Settings → remotes / images / account:
// a tab strip, a list with an action column, a message strip and dialogs.

const (
	tabRemotes = iota
	tabImages
	tabAccount
)

var tabNames = []string{"remotes", "images", "account"}

const syncEvery = 15 * time.Second

type msgKind int

const (
	msgInfo msgKind = iota
	msgOK
	msgErr
)

type (
	syncedMsg    syncResult
	templatesMsg struct {
		templates []Template
		err       error
	}
	imagesMsg struct {
		images []Image
		err    error
	}
	usageMsg struct {
		usage Usage
		err   error
	}
	doneMsg struct {
		text string
		err  error
		// resync asks for a fresh sync after the operation.
		resync bool
	}
	tickMsg  time.Time
	checkMsg struct {
		machineID string
		plan      string
		note      string
	}
)

type model struct {
	w, h  int
	tab   int
	focus int    // remotes and images: 0 list, 1 actions
	sel   int    // remotes row
	selID string // the selected row's identity, kept across list refreshes
	act   int    // remotes action
	img   int    // images row
	imgAc int    // images action
	accAc int    // account action

	acct      Account
	checked   bool
	machines  []Machine
	profiles  []Profile
	note      string // "signed out" / "offline" when the list is stale
	syncing   bool
	lastSync  time.Time
	templates []Template

	images       []Image
	imagesErr    error
	imagesLoaded bool
	usage        *Usage
	usageErr     error

	st   State
	jobs []Job

	message string
	kind    msgKind

	dialog *dialog
	hit    *hits
}

func newModel(tab string) model {
	m := model{hit: &hits{}, message: "Loading your hangar machines…", st: readState()}
	switch tab {
	case "images":
		m.tab = tabImages
	case "account":
		m.tab = tabAccount
	}
	return m
}

func (m model) Init() tea.Cmd {
	touchUIAlive()
	return tea.Batch(doSync, loadTemplates, loadImages, loadUsage, tick())
}

func tick() tea.Cmd {
	return tea.Tick(time.Second, func(t time.Time) tea.Msg { return tickMsg(t) })
}

func doSync() tea.Msg { return syncedMsg(syncAll()) }

func loadTemplates() tea.Msg {
	t, err := listTemplates()
	return templatesMsg{t, err}
}

func loadImages() tea.Msg {
	im, err := listImages()
	return imagesMsg{im, err}
}

func loadUsage() tea.Msg {
	u, err := getUsage()
	return usageMsg{u, err}
}

func (m *model) say(kind msgKind, format string, args ...any) {
	m.message, m.kind = fmt.Sprintf(format, args...), kind
}

func (m model) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		resized := m.w != 0
		m.w, m.h = msg.Width, msg.Height
		if resized {
			// herdr opens an overlay at the pane's size and then shrinks it to
			// fit inside its frame; the terminal reflows what was drawn at the
			// old size, so start from a clean screen.
			return m, tea.ClearScreen
		}
		return m, nil

	case tickMsg:
		touchUIAlive()
		m.st = readState()
		var cmds []tea.Cmd
		cmds = append(cmds, tick())
		// Show finished background jobs once, then refresh what they changed.
		for _, j := range listJobs() {
			if j.Status != "running" && !j.Seen {
				markJobSeen(j.ID)
				if j.Status == "failed" {
					m.say(msgErr, "%s", j.Result)
				} else {
					m.say(msgOK, "%s", j.Result)
				}
				cmds = append(cmds, doSync, loadImages, loadUsage)
			}
		}
		m.jobs = listJobs()
		if !m.syncing && time.Since(m.lastSync) > syncEvery {
			m.syncing = true
			cmds = append(cmds, doSync)
		}
		return m, tea.Batch(cmds...)

	case syncedMsg:
		m.syncing, m.lastSync, m.checked = false, time.Now(), true
		m.acct = msg.Account
		if msg.Note == "" {
			m.machines = msg.Machines
		} else if msg.Note == "signed out" {
			m.machines = nil
		}
		if msg.Note != "" && msg.Note != m.note && len(m.machines) > 0 {
			m.say(msgInfo, "hangar machines are %s; showing the last synced list.", msg.Note)
		} else if msg.Note == "" && strings.HasPrefix(m.message, "hangar machines are ") {
			m.message = ""
		}
		m.note = msg.Note
		m.profiles = msg.Profiles
		m.st = readState()
		if m.message == "Loading your hangar machines…" {
			m.message = ""
		}
		if msg.Err != nil && msg.Note == "" {
			m.say(msgErr, "%v", msg.Err)
		}
		if !m.acct.SignedIn() && m.acct.SignedOut && m.tab == tabRemotes && len(m.machines) == 0 && m.sel == 0 {
			// Nothing to manage yet: start where something can be done.
			m.tab = tabAccount
		}
		m.clamp()
		return m, nil

	case templatesMsg:
		if msg.err == nil {
			m.templates = msg.templates
		}
		return m, nil

	case imagesMsg:
		m.images, m.imagesErr, m.imagesLoaded = msg.images, msg.err, true
		m.clamp()
		return m, nil

	case usageMsg:
		if msg.err == nil {
			u := msg.usage
			m.usage, m.usageErr = &u, nil
		} else {
			m.usage, m.usageErr = nil, msg.err
		}
		return m, nil

	case doneMsg:
		if msg.err != nil {
			m.say(msgErr, "%v", msg.err)
		} else if msg.text != "" {
			m.say(msgOK, "%s", msg.text)
		}
		m.st = readState()
		m.jobs = listJobs()
		if msg.resync {
			m.syncing = true
			return m, tea.Batch(doSync, loadImages, loadUsage)
		}
		return m, nil

	case checkMsg:
		if d := m.dialog; d != nil && d.form != nil && d.form.machine.ID == msg.machineID {
			d.form.plan, d.form.check = msg.plan, msg.note
			d.form.refreshPrimary()
		}
		return m, nil

	case tea.KeyMsg:
		if m.dialog != nil {
			return m.dialogKey(msg)
		}
		return m.key(msg)

	case tea.MouseMsg:
		return m.mouse(msg)
	}
	return m, nil
}

func (m *model) clamp() {
	clampTo := func(v, n int) int { return max(0, min(v, n-1)) }
	// Keep the same remote selected when rows come and go.
	if m.selID != "" {
		for i, r := range m.rows() {
			if r.key() == m.selID {
				m.sel = i
			}
		}
	}
	defer func() {
		if r, ok := m.selectedRow(); ok {
			m.selID = r.key()
		}
	}()
	m.sel = clampTo(m.sel, len(m.rows()))
	m.act = clampTo(m.act, len(m.remoteActions()))
	m.img = clampTo(m.img, len(m.images))
	m.imgAc = clampTo(m.imgAc, len(m.imageActions()))
	m.accAc = clampTo(m.accAc, len(m.accountActions()))
}

func (m model) key(k tea.KeyMsg) (tea.Model, tea.Cmd) {
	switch k.String() {
	case "ctrl+c", "q":
		return m, tea.Quit
	case "esc":
		if m.tab != tabAccount && m.focus == 1 {
			m.focus = 0
			return m, nil
		}
		return m, tea.Quit
	case "left", "h", "shift+tab":
		return m.switchTab(-1)
	case "right", "l", "tab":
		return m.switchTab(1)
	case "up", "k":
		m.move(-1)
	case "down", "j":
		m.move(1)
	case "r":
		m.syncing = true
		return m, tea.Batch(doSync, loadImages, loadUsage, loadTemplates)
	case "enter", " ":
		return m.enter()
	}
	return m, nil
}

func (m model) switchTab(d int) (tea.Model, tea.Cmd) {
	m.tab = (m.tab + d + len(tabNames)) % len(tabNames)
	m.focus = 0
	m.clamp()
	switch m.tab {
	case tabImages:
		return m, loadImages
	case tabAccount:
		return m, loadUsage
	}
	return m, nil
}

func (m *model) move(d int) {
	switch {
	case m.tab == tabRemotes && m.focus == 0:
		m.sel += d
		m.act, m.selID = 0, ""
	case m.tab == tabRemotes:
		m.act += d
	case m.tab == tabImages && m.focus == 0:
		m.img += d
		m.imgAc = 0
	case m.tab == tabImages:
		m.imgAc += d
	default:
		m.accAc += d
	}
	m.clamp()
}

func (m model) enter() (tea.Model, tea.Cmd) {
	switch m.tab {
	case tabRemotes:
		rows := m.rows()
		if m.focus == 0 {
			if m.sel < len(rows) && rows[m.sel].kind == rowAdd {
				return m.openAdd("")
			}
			if len(m.remoteActions()) > 0 {
				m.focus = 1
			}
			return m, nil
		}
		return m.runRemoteAction()
	case tabImages:
		if m.focus == 0 {
			if len(m.images) > 0 {
				m.focus = 1
			}
			return m, nil
		}
		return m.runImageAction()
	}
	return m.runAccountAction()
}

// busyOn reports a running job for a machine (or image), so the same target
// never gets two operations at once.
func (m model) busyOn(id string) *Job {
	for i := range m.jobs {
		j := &m.jobs[i]
		if j.Status == "running" && (j.MachineID == id || j.Image == id) {
			return j
		}
	}
	return nil
}

// launch starts a background job and reports it.
func (m model) launch(j Job) (tea.Model, tea.Cmd) {
	started, err := startJob(j)
	if err != nil {
		m.say(msgErr, "%v", err)
		return m, nil
	}
	m.jobs = append(m.jobs, started)
	m.say(msgInfo, "Working… Esc closes this pane; the operation continues.")
	return m, nil
}

// quick runs a short operation off the UI thread.
func quick(resync bool, f func() (string, error)) tea.Cmd {
	return func() tea.Msg {
		text, err := f()
		return doneMsg{text: text, err: err, resync: resync}
	}
}

// interactive hands the terminal to a command that may prompt, then waits for
// Enter so its output can be read before the pane comes back. after runs once
// the command succeeds and supplies the message.
func interactive(name string, args []string, after func() (string, error)) tea.Cmd {
	script := `"$@"; s=$?; printf '\n[press Enter to return] '; read _; exit $s`
	cmd := exec.Command("sh", append([]string{"-c", script, "sh", name}, args...)...)
	return tea.ExecProcess(cmd, func(err error) tea.Msg {
		if err != nil {
			return doneMsg{err: fmt.Errorf("%s did not complete", strings.TrimSuffix(lastPath(name), ".exe")), resync: true}
		}
		text, err := after()
		return doneMsg{text: text, err: err, resync: true}
	})
}

func lastPath(p string) string {
	if i := strings.LastIndex(p, "/"); i >= 0 {
		return p[i+1:]
	}
	return p
}
