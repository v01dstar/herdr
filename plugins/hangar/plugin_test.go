package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// These tests drive the real code against the fake hangar and herdr CLIs in
// testdata/fake, with HOME and the plugin state in a temporary directory.

type fakeEnv struct {
	t    *testing.T
	dir  string
	home string
}

type fakeHangar struct {
	SignedIn bool             `json:"signedIn"`
	Machines []map[string]any `json:"machines"`
	Images   []map[string]any `json:"images"`
}

type fakeHerdr struct {
	Profiles      []Profile           `json:"profiles"`
	Notifications []map[string]string `json:"notifications"`
	Workspaces    []string            `json:"workspaces"`
}

func newFake(t *testing.T, machines ...map[string]any) *fakeEnv {
	t.Helper()
	dir := t.TempDir()
	fake, err := filepath.Abs("testdata/fake")
	if err != nil {
		t.Fatal(err)
	}
	home := filepath.Join(dir, "home")
	if err := os.MkdirAll(filepath.Join(home, ".ssh"), 0o700); err != nil {
		t.Fatal(err)
	}
	t.Setenv("FAKE_DIR", dir)
	t.Setenv("HOME", home)
	t.Setenv("HANGAR_BIN", filepath.Join(fake, "hangar"))
	t.Setenv("HERDR_BIN_PATH", filepath.Join(fake, "herdr"))
	t.Setenv("HERDR_PLUGIN_STATE_DIR", filepath.Join(dir, "state"))
	t.Setenv("FAKE_ADD_FAILS", "")
	f := &fakeEnv{t: t, dir: dir, home: home}
	f.writeHangar(fakeHangar{SignedIn: true, Machines: machines})
	return f
}

func fakeMachine(id, name, state string) map[string]any {
	return map[string]any{
		"id": id, "name": name, "state": state,
		"template": map[string]any{"id": "herdr", "version": "2026-10-05.1"},
		"spec":     map[string]any{"vcpus": 4, "memMiB": 8192, "persistentDiskGiB": 20},
		"storage":  map[string]any{"synced": true},
	}
}

func (f *fakeEnv) readJSON(name string, v any) {
	f.t.Helper()
	data, err := os.ReadFile(filepath.Join(f.dir, name))
	if os.IsNotExist(err) {
		return
	}
	if err != nil {
		f.t.Fatal(err)
	}
	if err := json.Unmarshal(data, v); err != nil {
		f.t.Fatal(err)
	}
}

func (f *fakeEnv) writeJSON(name string, v any) {
	f.t.Helper()
	data, _ := json.Marshal(v)
	if err := os.WriteFile(filepath.Join(f.dir, name), data, 0o600); err != nil {
		f.t.Fatal(err)
	}
}

func (f *fakeEnv) hangar() fakeHangar {
	var h fakeHangar
	f.readJSON("hangar.json", &h)
	return h
}

func (f *fakeEnv) writeHangar(h fakeHangar) { f.writeJSON("hangar.json", h) }

func (f *fakeEnv) herdr() fakeHerdr {
	var h fakeHerdr
	f.readJSON("herdr.json", &h)
	return h
}

func (f *fakeEnv) setState(id, state string) {
	h := f.hangar()
	for _, m := range h.Machines {
		if m["id"] == id {
			m["state"] = state
		}
	}
	f.writeHangar(h)
}

func (f *fakeEnv) profile(machineID string) (Profile, bool) {
	for _, p := range f.herdr().Profiles {
		if p.Target == TargetPrefix+machineID {
			return p, true
		}
	}
	return Profile{}, false
}

func (f *fakeEnv) sync() {
	f.t.Helper()
	if r := syncAll(); r.Err != nil {
		f.t.Fatalf("sync: %v", r.Err)
	}
}

func (f *fakeEnv) sshConfig() string {
	data, _ := os.ReadFile(filepath.Join(f.home, ".ssh", "config"))
	return string(data)
}

func TestSyncFollowsMachineState(t *testing.T) {
	f := newFake(t, fakeMachine("m_a", "alpha", "running"), fakeMachine("m_b", "beta", "suspended"))
	f.sync()
	p, ok := f.profile("m_a")
	if !ok || !p.Enabled || p.Label != "alpha" || p.Session != RemoteSession {
		t.Fatalf("running machine not added and enabled: %+v %v", p, ok)
	}
	if _, ok := f.profile("m_b"); ok {
		t.Fatal("a suspended machine cannot be added before it runs")
	}
	if !strings.Contains(f.sshConfig(), includeLine()) || !hasHostBlock("m_a") {
		t.Fatal("adding a machine sets up SSH")
	}

	f.setState("m_a", "suspended")
	f.sync()
	if p, _ := f.profile("m_a"); p.Enabled {
		t.Fatal("a machine that stopped running is disabled")
	}
	f.setState("m_a", "running")
	f.sync()
	if p, _ := f.profile("m_a"); !p.Enabled {
		t.Fatal("a running machine is enabled again")
	}
}

func TestSyncHiddenRenamedAndDeleted(t *testing.T) {
	f := newFake(t, fakeMachine("m_a", "alpha", "running"))
	f.sync()
	_ = updateState(func(s *State) { s.Hidden["m_a"] = true; s.Default = "hangar:m_a" })
	f.sync()
	if _, ok := f.profile("m_a"); ok {
		t.Fatal("a hidden machine leaves the sidebar")
	}
	_ = updateState(func(s *State) { delete(s.Hidden, "m_a"); s.Labels["m_a"] = "box" })
	f.sync()
	if p, ok := f.profile("m_a"); !ok || p.Label != "box" {
		t.Fatalf("shown again under its herdr name: %+v", p)
	}

	f.writeHangar(fakeHangar{SignedIn: true})
	f.sync()
	if _, ok := f.profile("m_a"); ok || hasHostBlock("m_a") {
		t.Fatal("a deleted machine is forgotten")
	}
	if st := readState(); st.Default != "" || st.Labels["m_a"] != "" {
		t.Fatalf("state for a deleted machine is dropped: %+v", st)
	}
}

func TestSignOutRemovesLocalTracesOnly(t *testing.T) {
	f := newFake(t, fakeMachine("m_a", "alpha", "running"))
	userConfig := "Host work\n  HostName work.example\n"
	_ = os.WriteFile(filepath.Join(f.home, ".ssh", "config"), []byte(userConfig), 0o600)
	h := f.herdr()
	h.Profiles = append(h.Profiles, Profile{ID: "own", Label: "work", Target: "work", Enabled: true})
	f.writeJSON("herdr.json", h)
	f.sync()
	_ = updateState(func(s *State) { s.Labels["m_a"] = "box" })

	hg := f.hangar()
	hg.SignedIn = false
	f.writeHangar(hg)
	f.sync()
	if _, ok := f.profile("m_a"); ok {
		t.Fatal("signed out, hangar machines leave herdr")
	}
	if got := f.sshConfig(); got != userConfig {
		t.Fatalf("~/.ssh/config is restored exactly, got %q", got)
	}
	if entries, _ := os.ReadDir(hostsDir()); len(entries) != 0 {
		t.Fatal("Host blocks are removed")
	}
	if len(f.herdr().Profiles) != 1 || len(f.hangar().Machines) != 1 {
		t.Fatal("the user's own SSH remote and the hangar machine itself stay")
	}

	hg.SignedIn = true
	f.writeHangar(hg)
	f.sync()
	if p, ok := f.profile("m_a"); !ok || p.Label != "box" {
		t.Fatalf("signing in brings the machine back with its name: %+v", p)
	}
}

func TestFailedAddWaitsBeforeRetrying(t *testing.T) {
	f := newFake(t, fakeMachine("m_a", "alpha", "running"))
	t.Setenv("FAKE_ADD_FAILS", "1")
	if r := syncAll(); r.Err == nil {
		t.Fatal("the failed add is reported")
	}
	st := readState()
	if st.AddError["m_a"] == "" || !st.RetryAdd["m_a"].After(time.Now()) {
		t.Fatalf("failure recorded with a retry time: %+v", st)
	}
	t.Setenv("FAKE_ADD_FAILS", "")
	f.sync()
	if _, ok := f.profile("m_a"); ok {
		t.Fatal("no retry before the retry time")
	}
	_ = updateState(func(s *State) { s.RetryAdd["m_a"] = time.Now().Add(-time.Second) })
	f.sync()
	if _, ok := f.profile("m_a"); !ok {
		t.Fatal("retried after the retry time")
	}
}

func runTestJob(t *testing.T, j Job) string {
	t.Helper()
	out, err := j.run(func(string, ...any) {})
	if err != nil {
		t.Fatalf("%s: %v", j.Kind, err)
	}
	return out
}

func TestLifecycleJobs(t *testing.T) {
	f := newFake(t, fakeMachine("m_a", "alpha", "running"))
	f.sync()

	runTestJob(t, Job{Kind: jobStop, MachineID: "m_a", MachineName: "alpha"})
	if p, _ := f.profile("m_a"); p.Enabled {
		t.Fatal("stop disables the machine in herdr")
	}
	f.sync()
	if p, _ := f.profile("m_a"); p.Enabled {
		t.Fatal("still disabled after a sync while stopped")
	}

	runTestJob(t, Job{Kind: jobStart, MachineID: "m_a", MachineName: "alpha"})
	if p, ok := f.profile("m_a"); !ok || !p.Enabled {
		t.Fatal("start connects the machine again")
	}
	if readState().fenced("m_a") {
		t.Fatal("start lifts the fence")
	}
	log, _ := os.ReadFile(filepath.Join(f.dir, "calls.log"))
	if strings.Count(string(log), "herdr machine add") < 2 {
		t.Fatal("start re-adds the machine so herdr starts its session")
	}

	runTestJob(t, Job{Kind: jobSuspend, MachineID: "m_a", MachineName: "alpha"})
	f.sync()
	if p, _ := f.profile("m_a"); p.Enabled {
		t.Fatal("suspend disables the machine")
	}
	runTestJob(t, Job{Kind: jobResume, MachineID: "m_a", MachineName: "alpha"})
	if p, _ := f.profile("m_a"); !p.Enabled {
		t.Fatal("resume enables the machine")
	}

	runTestJob(t, Job{Kind: jobDelete, MachineID: "m_a", MachineName: "alpha"})
	if _, ok := f.profile("m_a"); ok || len(f.hangar().Machines) != 0 {
		t.Fatal("delete removes the machine everywhere")
	}
	if out := runTestJob(t, Job{Kind: jobDelete, MachineID: "m_a", MachineName: "alpha"}); !strings.Contains(out, "already deleted") {
		t.Fatalf("deleting again reports it, got %q", out)
	}
}

func TestCreateCloneAndImages(t *testing.T) {
	f := newFake(t)
	runTestJob(t, Job{Kind: jobCreate, Name: "fresh", MachineName: "fresh"})
	h := f.hangar()
	if len(h.Machines) != 1 {
		t.Fatal("create makes a machine")
	}
	id := h.Machines[0]["id"].(string)
	if p, ok := f.profile(id); !ok || !p.Enabled {
		t.Fatal("a created machine is connected")
	}

	out := runTestJob(t, Job{Kind: jobClone, MachineID: id, MachineName: "fresh", Name: "fresh-clone"})
	if !strings.Contains(out, "stays stopped") {
		t.Fatalf("cloning a running machine stops it first: %q", out)
	}
	if len(f.hangar().Machines) != 2 {
		t.Fatal("clone makes a second machine")
	}

	runTestJob(t, Job{Kind: jobSaveImage, MachineID: id, MachineName: "fresh", Name: "base"})
	images, err := listImages()
	if err != nil || len(images) != 1 || images[0].Name != "base" {
		t.Fatalf("image saved: %+v %v", images, err)
	}
	runTestJob(t, Job{Kind: jobCreate, Name: "from-base", Image: images[0].ID, ImageName: "base", MachineName: "from-base"})
	runTestJob(t, Job{Kind: jobDeleteImage, Image: images[0].ID, ImageName: "base"})
	if images, _ := listImages(); len(images) != 0 {
		t.Fatal("image deleted")
	}
}

func TestNewWorkspaceUsesTheDefault(t *testing.T) {
	f := newFake(t, fakeMachine("m_a", "alpha", "running"), fakeMachine("m_b", "beta", "stopped"))
	f.sync()
	if err := newWorkspace(); err != nil {
		t.Fatal(err)
	}
	_ = updateState(func(s *State) { s.Default = "hangar:m_a" })
	_ = newWorkspace()
	_ = updateState(func(s *State) { s.Default = "hangar:m_b" })
	_ = newWorkspace()
	h := f.herdr()
	if strings.Join(h.Workspaces, ",") != "local,alpha" {
		t.Fatalf("workspaces created on Local then alpha, got %v", h.Workspaces)
	}
	if len(h.Notifications) != 2 || !strings.Contains(h.Notifications[0]["body"], "select it in the sidebar") ||
		!strings.Contains(h.Notifications[1]["body"], "isn't connected") {
		t.Fatalf("a remote workspace points to the sidebar; a disconnected default explains itself: %+v", h.Notifications)
	}
}

func TestHangarActionsByState(t *testing.T) {
	m := model{st: readStateForTest(), templates: []Template{{ID: "herdr", Version: "1", Capabilities: []string{"identity-reset"}}}}
	labels := func(state string) string {
		r := row{kind: rowHangar, machine: Machine{ID: "m", Name: "m", State: state, Template: &TemplateRef{ID: "herdr", Version: "1"}}}
		var out []string
		for _, a := range m.hangarActions(r, "") {
			s := a.label
			if a.reason != "" {
				s += "(dim)"
			}
			out = append(out, s)
		}
		return strings.Join(out, ",")
	}
	cases := map[string]string{
		"running":   "Suspend…,Stop…,Test connection(dim),Edit…,Use as default,Hide from sidebar,Copy machine…,Delete machine…",
		"stopped":   "Start,Test connection(dim),Edit…,Use as default,Hide from sidebar,Copy machine…,Delete machine…",
		"suspended": "Resume,Test connection(dim),Edit…,Use as default,Hide from sidebar,Copy machine…,Delete machine…",
		"starting":  "Suspend…(dim),Stop…(dim),Test connection(dim),Edit…,Use as default,Hide from sidebar,Copy machine…(dim),Delete machine…",
		"error":     "Start,Stop…,Test connection(dim),Edit…,Use as default,Hide from sidebar,Copy machine…,Delete machine…",
		"deleting":  "Test connection(dim),Edit…,Use as default,Hide from sidebar,Copy machine…(dim),Delete machine…(dim)",
	}
	for state, want := range cases {
		if got := labels(state); got != want {
			t.Errorf("%s:\n got %s\nwant %s", state, got, want)
		}
	}
	m.note = "offline"
	if got := labels("stopped"); !strings.HasPrefix(got, "Start(dim)") {
		t.Errorf("offline blocks server actions: %s", got)
	}
}

func readStateForTest() State {
	var s State
	s.init()
	return s
}

func TestIncludeRoundTripKeepsTheRestOfTheConfig(t *testing.T) {
	f := newFake(t)
	original := "Host work\n  HostName work.example\n"
	path := filepath.Join(f.home, ".ssh", "config")
	_ = os.WriteFile(path, []byte(original), 0o600)
	if _, err := withInclude("ok"); err != nil || !sshIncluded() {
		t.Fatalf("Include added: %v", err)
	}
	if status, _ := withInclude("ok"); status != "ok" {
		t.Fatalf("added only once, got %q", status)
	}
	if err := removeInclude(); err != nil {
		t.Fatal(err)
	}
	if got, _ := os.ReadFile(path); string(got) != original {
		t.Fatalf("got %q, want %q", got, original)
	}
	moved := "Host a\n" + includeComment + "\n" + includeLine() + "\nHost b\n"
	_ = os.WriteFile(path, []byte(moved), 0o600)
	_ = removeInclude()
	if got, _ := os.ReadFile(path); string(got) != "Host a\nHost b\n" {
		t.Fatalf("a moved Include is found too, got %q", got)
	}
}

func TestValidation(t *testing.T) {
	for name, ok := range map[string]bool{"a": true, "dev-1": true, "-x": false, "Up": false, strings.Repeat("a", 64): false} {
		if validName(name) != ok {
			t.Errorf("validName(%q) = %v", name, !ok)
		}
	}
	if _, problem := validateInvite(" hgi_ABCDEFGHIJ0123456789 "); problem != "" {
		t.Error(problem)
	}
	if _, problem := validateInvite("hgi_short"); problem == "" {
		t.Error("short invite accepted")
	}
}
