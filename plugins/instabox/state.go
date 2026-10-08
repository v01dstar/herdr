package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"syscall"
	"time"
)

// State is the plugin's own durable data. The UI, background jobs and sync runs
// are separate processes, so every change goes through updateState under a lock.
type State struct {
	// LastAccount is the account seen most recently, so a sign-out made with
	// the instabox CLI can still be cleaned up.
	LastAccount string `json:"lastAccount,omitempty"`
	// Default is where New workspace opens: "" for Local, "instabox:<machine id>"
	// or "ssh:<profile id>".
	Default string `json:"default,omitempty"`
	// Hidden machines stay listed here but are not in herdr's sidebar.
	Hidden map[string]bool `json:"hidden,omitempty"`
	// Labels are herdr-only names that override the instabox name.
	Labels map[string]string `json:"labels,omitempty"`
	// Fence keeps a machine disconnected while it stops, suspends or is
	// deleted, until instabox no longer reports it running.
	Fence map[string]time.Time `json:"fence,omitempty"`
	// RetryAdd delays the next automatic `herdr machine add` after a failure.
	RetryAdd map[string]time.Time `json:"retryAdd,omitempty"`
	AddError map[string]string    `json:"addError,omitempty"`
	LastSync time.Time            `json:"lastSync,omitempty"`
}

const fenceFor = 120 * time.Second

func stateDir() string {
	if dir := os.Getenv("HERDR_PLUGIN_STATE_DIR"); dir != "" {
		return dir
	}
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".local", "state", "herdr-instabox")
}

func statePath() string { return filepath.Join(stateDir(), "state.json") }

func (s *State) init() {
	if s.Hidden == nil {
		s.Hidden = map[string]bool{}
	}
	if s.Labels == nil {
		s.Labels = map[string]string{}
	}
	if s.Fence == nil {
		s.Fence = map[string]time.Time{}
	}
	if s.RetryAdd == nil {
		s.RetryAdd = map[string]time.Time{}
	}
	if s.AddError == nil {
		s.AddError = map[string]string{}
	}
}

func readState() State {
	var s State
	if data, err := os.ReadFile(statePath()); err == nil {
		_ = json.Unmarshal(data, &s)
	}
	s.init()
	return s
}

// withLock runs f while holding an exclusive lock on the named file in the
// state directory, across processes. Locks are not reentrant.
func withLock(name string, f func() error) error {
	if err := os.MkdirAll(stateDir(), 0o700); err != nil {
		return err
	}
	lock, err := os.OpenFile(filepath.Join(stateDir(), name), os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		return err
	}
	defer lock.Close()
	if err := syscall.Flock(int(lock.Fd()), syscall.LOCK_EX); err != nil {
		return err
	}
	defer syscall.Flock(int(lock.Fd()), syscall.LOCK_UN)
	return f()
}

// withMachinesLock serializes every change to herdr's saved machines. Without
// it, overlapping syncs (startup, workspace events, the open pane) each see a
// machine missing and each add it.
func withMachinesLock(f func() error) error { return withLock("machines.lock", f) }

// updateState applies f to the state under an exclusive lock.
func updateState(f func(*State)) error {
	return withLock("state.lock", func() error { return writeState(f) })
}

func writeState(f func(*State)) error {
	s := readState()
	f(&s)
	data, err := json.MarshalIndent(s, "", "  ")
	if err != nil {
		return err
	}
	tmp := statePath() + ".tmp"
	if err := os.WriteFile(tmp, data, 0o600); err != nil {
		return err
	}
	return os.Rename(tmp, statePath())
}

func (s State) fenced(id string) bool {
	until, ok := s.Fence[id]
	return ok && time.Now().Before(until)
}

// forget drops everything kept about a machine that no longer exists.
func (s *State) forget(id string) {
	delete(s.Hidden, id)
	delete(s.Labels, id)
	delete(s.Fence, id)
	delete(s.RetryAdd, id)
	delete(s.AddError, id)
	if s.Default == "instabox:"+id {
		s.Default = ""
	}
}

func (s State) label(m Machine) string {
	if l := s.Labels[m.ID]; l != "" {
		return l
	}
	return m.Name
}

// uiAlivePath is touched while the settings pane is open, so a finishing job
// knows whether to show its result there or as a herdr notification.
func uiAlivePath() string { return filepath.Join(stateDir(), "ui.alive") }

func touchUIAlive() {
	now := time.Now()
	if err := os.Chtimes(uiAlivePath(), now, now); err != nil {
		_ = os.WriteFile(uiAlivePath(), nil, 0o600)
	}
}

func uiAlive() bool {
	info, err := os.Stat(uiAlivePath())
	return err == nil && time.Since(info.ModTime()) < 10*time.Second
}
