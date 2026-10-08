package main

import (
	"fmt"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
)

func (m model) snapshotActions() []action {
	if m.img >= len(m.snapshots) {
		return nil
	}
	reason := ""
	if m.busyOn(m.snapshots[m.img].ID) != nil {
		reason = "Wait for the current operation on this snapshot to finish"
	}
	return []action{
		{kind: aNewFromSnapshot, label: "New machine from snapshot…"},
		{kind: aDeleteSnapshot, label: "Delete snapshot…", reason: reason},
	}
}

func (m model) runSnapshotAction() (tea.Model, tea.Cmd) {
	acts := m.snapshotActions()
	if m.imgAc >= len(acts) {
		return m, nil
	}
	a := acts[m.imgAc]
	if a.reason != "" {
		m.say(msgErr, "%s: %s.", strings.TrimSuffix(a.label, "…"), a.reason)
		return m, nil
	}
	im := m.snapshots[m.img]
	switch a.kind {
	case aNewFromSnapshot:
		return m.openAdd(im.ID)
	case aDeleteSnapshot:
		m.dialog = deleteSnapshotDialog(im)
	}
	return m, nil
}

func deleteSnapshotDialog(im Snapshot) *dialog {
	return confirmDialog("delete snapshot", "delete", fmt.Sprintf(
		"Delete snapshot '%s'? New machines can no longer be created from it. Machines already created from it are not affected; they keep their own disks. This cannot be undone.", im.Name),
		Job{Kind: jobDeleteSnapshot, Snapshot: im.ID, SnapshotName: im.Name})
}

// snapshotDetail is the right pane for the selected snapshot.
func (m model) snapshotDetail(im Snapshot) (string, []string) {
	var lines []string
	if d := strings.TrimSpace(im.Description); d != "" {
		lines = append(lines, d)
	}
	lines = append(lines, "created "+im.CreatedAt.Local().Format("2006-01-02 15:04"))
	source := "saved from a machine that no longer exists"
	for _, mc := range m.machines {
		if mc.ID == im.SourceMachineID {
			source = "saved from " + m.st.label(mc)
		}
	}
	lines = append(lines, source)
	if im.Template != nil {
		lines = append(lines, "template "+im.Template.Label())
	}
	lines = append(lines, fmt.Sprintf("root disk %.1f GiB", float64(im.RootSizeBytes)/(1<<30)))
	if im.HasData() {
		lines = append(lines, fmt.Sprintf("/data %.1f GiB", float64(im.DataSizeBytes)/(1<<30)))
	} else {
		lines = append(lines, "root disk only; machines from it get an empty /data")
	}
	lines = append(lines, humanBytes(im.ExclusiveBytes)+" stored only for this snapshot")
	if j := m.busyOn(im.ID); j != nil && j.Progress != "" {
		lines = append(lines, j.Progress)
	}
	return im.Name, lines
}

func (m model) snapshotsPlaceholder() (string, string) {
	switch {
	case !m.snapshotsLoaded:
		return "Loading…", "Loading your snapshots…"
	case m.snapshotsErr != nil && !m.acct.SignedIn():
		return "", "Sign in on the account tab to list your snapshots."
	case m.snapshotsErr != nil:
		return "", "Could not list snapshots: " + m.snapshotsErr.Error()
	case len(m.snapshots) == 0:
		return "No snapshots", "No snapshots yet. A snapshot saves an instabox machine's disks (software, settings and /data) so new machines can start from it. To save one, select an instabox machine on the remotes tab and choose Copy machine… → Save as snapshot."
	}
	return "", ""
}

func humanBytes(n int64) string {
	switch {
	case n >= 1<<30:
		return fmt.Sprintf("%.1f GiB", float64(n)/(1<<30))
	case n >= 1<<20:
		return fmt.Sprintf("%.1f MiB", float64(n)/(1<<20))
	case n >= 1<<10:
		return fmt.Sprintf("%.1f KiB", float64(n)/(1<<10))
	}
	return fmt.Sprintf("%d B", n)
}
