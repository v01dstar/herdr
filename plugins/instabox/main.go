// herdr-instabox is a herdr plugin that manages instabox machines and keeps them in
// herdr's saved SSH machines, so they appear in the sidebar like any remote.
//
//	herdr-instabox [ui]                   the remotes/snapshots/account settings pane
//	herdr-instabox open [tab]             open the settings pane (plugin action)
//	herdr-instabox new-workspace          new workspace on the default machine
//	herdr-instabox sync [--if-stale]      reconcile herdr with instabox (hooks)
//	herdr-instabox job ID                 run a background job (internal)
//	herdr-instabox ensure-cert ID BIN     refresh a certificate (ssh Match exec)
package main

import (
	"fmt"
	"os"
	"os/exec"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
)

func main() {
	cmd := "ui"
	if len(os.Args) > 1 {
		cmd = os.Args[1]
	}
	arg := func(i int) string {
		if len(os.Args) > i {
			return os.Args[i]
		}
		return ""
	}
	var err error
	switch cmd {
	case "ui":
		_, err = tea.NewProgram(newModel(arg(2)), tea.WithAltScreen(), tea.WithMouseCellMotion()).Run()
	case "open":
		err = openPane(arg(2))
	case "new-workspace":
		err = newWorkspace()
	case "sync":
		if arg(2) == "--if-stale" {
			err = syncIfStale()
		} else {
			err = syncAll().Err
		}
	case "job":
		err = runJob(arg(2))
	case "ensure-cert":
		ensureCert(arg(2), arg(3))
	default:
		err = fmt.Errorf("unknown command %q", cmd)
	}
	if err != nil {
		fmt.Fprintln(os.Stderr, "herdr-instabox:", err)
		os.Exit(1)
	}
}

func pluginID() string {
	if id := os.Getenv("HERDR_PLUGIN_ID"); id != "" {
		return id
	}
	return "v01dstar.instabox"
}

func openPane(tab string) error {
	entry := "ui"
	if tab == "account" || tab == "snapshots" {
		entry = tab
	}
	cmd := exec.Command(herdrBin(), "plugin", "pane", "open",
		"--plugin", pluginID(), "--entrypoint", entry, "--focus")
	cmd.Stdout, cmd.Stderr = os.Stdout, os.Stderr
	return cmd.Run()
}

// newWorkspace opens a workspace on the default machine. It never starts a
// machine; when the default is not connected it explains why instead.
func newWorkspace() error {
	st := readState()
	target, label, problem := defaultTarget(st)
	if problem != "" {
		notify("New workspace", problem)
		return nil
	}
	args := []string{"workspace", "create", "--focus"}
	if target != "" {
		args = append([]string{"--machine", target}, args...)
	}
	where := "this computer"
	if label != "" {
		where = label
	}
	if _, err := herdr(args...); err != nil {
		notify("New workspace", fmt.Sprintf("Could not create a workspace on %s: %v", where, err))
		return nil
	}
	if target != "" {
		// Plugins cannot switch the client to another machine.
		notify("New workspace", fmt.Sprintf("Workspace created on %s; select it in the sidebar.", where))
	}
	return nil
}

// defaultTarget resolves the default location to a herdr machine profile ID
// ("" for Local), or explains why it cannot be used now.
func defaultTarget(st State) (target, label, problem string) {
	if st.Default == "" {
		return "", "", ""
	}
	all, err := listProfiles()
	if err != nil {
		return "", "", err.Error()
	}
	managed, ssh := splitProfiles(all)
	if id, ok := strings.CutPrefix(st.Default, "instabox:"); ok {
		label = st.Labels[id]
		p, has := managed[id]
		if label == "" && has {
			label = p.Label
		}
		if label == "" {
			label = id
		}
		switch {
		case st.Hidden[id]:
			return "", label, fmt.Sprintf("Default machine %s is hidden from the sidebar — show it in instabox settings, or choose another default.", label)
		case !has || !p.Enabled:
			return "", label, fmt.Sprintf("Default machine %s isn't connected — start it in instabox settings, or choose another default.", label)
		}
		return p.ID, label, ""
	}
	if id, ok := strings.CutPrefix(st.Default, "ssh:"); ok {
		for _, p := range ssh {
			if p.ID == id {
				if !p.Enabled {
					return "", p.Label, fmt.Sprintf("Default machine %s isn't connected — enable it in instabox settings, or choose another default.", p.Label)
				}
				return p.ID, p.Label, ""
			}
		}
	}
	return "", "", ""
}
