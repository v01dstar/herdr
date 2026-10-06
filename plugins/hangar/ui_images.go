package main

import (
	"fmt"
	"strings"

	tea "github.com/charmbracelet/bubbletea"
)

func (m model) imageActions() []action {
	if m.img >= len(m.images) {
		return nil
	}
	reason := ""
	if m.busyOn(m.images[m.img].ID) != nil {
		reason = "Wait for the current operation on this image to finish"
	}
	return []action{
		{kind: aNewFromImage, label: "New machine from image…"},
		{kind: aDeleteImage, label: "Delete image…", reason: reason},
	}
}

func (m model) runImageAction() (tea.Model, tea.Cmd) {
	acts := m.imageActions()
	if m.imgAc >= len(acts) {
		return m, nil
	}
	a := acts[m.imgAc]
	if a.reason != "" {
		m.say(msgErr, "%s: %s.", strings.TrimSuffix(a.label, "…"), a.reason)
		return m, nil
	}
	im := m.images[m.img]
	switch a.kind {
	case aNewFromImage:
		return m.openAdd(im.ID)
	case aDeleteImage:
		m.dialog = deleteImageDialog(im)
	}
	return m, nil
}

func deleteImageDialog(im Image) *dialog {
	return confirmDialog("delete image", "delete", fmt.Sprintf(
		"Delete image '%s'? New machines can no longer be created from it. Machines already created from it are not affected; they keep their own disks. This cannot be undone.", im.Name),
		Job{Kind: jobDeleteImage, Image: im.ID, ImageName: im.Name})
}

// imageDetail is the right pane for the selected image.
func (m model) imageDetail(im Image) (string, []string) {
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
	lines = append(lines, humanBytes(im.ExclusiveBytes)+" stored only for this image")
	if j := m.busyOn(im.ID); j != nil && j.Progress != "" {
		lines = append(lines, j.Progress)
	}
	return im.Name, lines
}

func (m model) imagesPlaceholder() (string, string) {
	switch {
	case !m.imagesLoaded:
		return "Loading…", "Loading your images…"
	case m.imagesErr != nil && !m.acct.SignedIn():
		return "", "Sign in on the account tab to list your images."
	case m.imagesErr != nil:
		return "", "Could not list images: " + m.imagesErr.Error()
	case len(m.images) == 0:
		return "No images", "No images yet. An image saves a hangar machine's root disk (installed software and system settings) so new machines can start from it. To save one, select a hangar machine on the remotes tab and choose Copy machine… → Save as image."
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
