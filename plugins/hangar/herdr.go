package main

import (
	"encoding/json"
	"fmt"
	"os"
	"strings"
)

// TargetPrefix marks the herdr SSH machines this plugin manages. Profiles with
// any other target are the user's own SSH remotes.
const TargetPrefix = "herdr-hangar-"

// RemoteSession is the herdr session hangar machines run.
const RemoteSession = "herdr-remote"

// Profile is one saved herdr SSH machine (`herdr machine list --json`).
type Profile struct {
	ID      string `json:"id"`
	Label   string `json:"label"`
	Target  string `json:"target"`
	Session string `json:"session"`
	Enabled bool   `json:"enabled"`
}

// MachineID is the hangar machine a managed profile stands for.
func (p Profile) MachineID() (string, bool) { return strings.CutPrefix(p.Target, TargetPrefix) }

func herdrBin() string {
	if bin := os.Getenv("HERDR_BIN_PATH"); bin != "" {
		return bin
	}
	return "herdr"
}

func herdr(args ...string) (string, error) { return run(herdrBin(), args...) }

func listProfiles() ([]Profile, error) {
	out, err := herdr("machine", "list", "--json")
	if err != nil {
		return nil, err
	}
	var all []Profile
	if err := json.Unmarshal([]byte(out), &all); err != nil {
		return nil, fmt.Errorf("herdr machine list: %w", err)
	}
	return all, nil
}

// splitProfiles separates managed hangar profiles (by machine ID) from the
// user's own SSH remotes.
func splitProfiles(all []Profile) (managed map[string]Profile, ssh []Profile) {
	managed = map[string]Profile{}
	for _, p := range all {
		if id, ok := p.MachineID(); ok {
			managed[id] = p
		} else {
			ssh = append(ssh, p)
		}
	}
	return managed, ssh
}

// machineStatus runs herdr's noninteractive check of a saved machine and
// returns its status word ("reachable", "disabled", "auth", ...) and error.
func machineStatus(profileID string) (string, string, error) {
	out, err := herdr("machine", "status", profileID, "--json")
	if err != nil {
		return "", "", err
	}
	var rows []struct {
		Status string  `json:"status"`
		Error  *string `json:"error"`
	}
	if err := json.Unmarshal([]byte(out), &rows); err != nil || len(rows) == 0 {
		return "", "", fmt.Errorf("herdr machine status: unexpected output")
	}
	detail := ""
	if rows[0].Error != nil {
		detail = *rows[0].Error
	}
	return rows[0].Status, detail, nil
}

// notify shows a herdr notification, used when a background job finishes while
// the settings pane is closed.
func notify(title, body string) {
	_, _ = herdr("notification", "show", title, "--body", body)
}
