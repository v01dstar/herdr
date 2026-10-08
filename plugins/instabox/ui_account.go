package main

import (
	"fmt"
	"time"

	tea "github.com/charmbracelet/bubbletea"
)

const accountNote = "Herdr and the instabox CLI share this sign-in (~/.config/instabox/credentials.json). Sign in opens your browser; over SSH or without a browser it shows a code to enter instead (GitHub only)."

func (m model) accountActions() []action {
	if m.acct.SignedIn() || (m.acct.Err != nil && !m.acct.SignedOut) {
		return []action{
			{kind: aSwitch, label: "Switch account…"},
			{kind: aSignOut, label: "Sign out…"},
		}
	}
	return []action{
		{kind: aSignIn, label: "Sign in with GitHub"},
		{kind: aSignInGoogle, label: "Sign in with Google"},
	}
}

func (m model) accountSummary() string {
	switch {
	case !m.checked:
		return "Checking your instabox sign-in…"
	case m.acct.SignedIn():
		return fmt.Sprintf("Signed in as @%s on %s.", m.acct.Login, m.acct.Server)
	case m.acct.SignedOut:
		return fmt.Sprintf("Not signed in to instabox (%s).", m.acct.Server)
	}
	return fmt.Sprintf("Signed in on %s, but the account could not be checked: %v", m.acct.Server, m.acct.Err)
}

func (m model) usageLines() []string {
	switch {
	case !m.acct.SignedIn():
		return []string{"Sign in to see your usage."}
	case m.usage == nil && m.usageErr != nil:
		return []string{"Could not load usage: " + m.usageErr.Error()}
	case m.usage == nil:
		return []string{"Loading usage…"}
	}
	u := m.usage
	storage := humanBytes(u.StoredBytes) + " stored"
	if max := int64(u.Limits.MaxStoredGiB) << 30; max > 0 {
		storage = fmt.Sprintf("%s of %d GiB stored (%d%%)", humanBytes(u.StoredBytes), u.Limits.MaxStoredGiB, u.StoredBytes*100/max)
	}
	lines := []string{
		"Storage   " + storage,
		fmt.Sprintf("Machines  %d of %d", u.Machines, u.Limits.MaxMachines),
		fmt.Sprintf("Snapshots    %d of %d", u.Snapshots, u.Limits.MaxSnapshots),
	}
	if u.ComputedAt != nil {
		lines = append(lines, "Measured "+u.ComputedAt.UTC().Format("2006-01-02 15:04")+" UTC (usage is measured hourly)")
	} else {
		lines = append(lines, "Not measured yet; usage is measured hourly.")
	}
	return lines
}

func (m model) runAccountAction() (tea.Model, tea.Cmd) {
	acts := m.accountActions()
	if m.accAc >= len(acts) {
		return m, nil
	}
	switch acts[m.accAc].kind {
	case aSignIn, aSwitch:
		return m.signIn("github")
	case aSignInGoogle:
		return m.signIn("google")
	case aSignOut:
		server := m.acct.Server
		m.dialog = &dialog{confirm: &confirm{
			title: "sign out of instabox",
			verb:  "sign out",
			body:  fmt.Sprintf("Sign out of instabox on %s? This revokes the sign-in on the server and removes ~/.config/instabox/credentials.json, which the instabox CLI shares, so `instabox` commands are signed out too. Herdr forgets the instabox machines and removes their SSH config; the machines keep running, and signing in again brings them back.", server),
			run: func(m *model) tea.Cmd {
				m.say(msgInfo, "Signing out…")
				return quick(true, func() (string, error) {
					// Clean up while still signed in, then sign out even if
					// part of it failed; the next sync retries the rest.
					cleanErr := cleanupLocal()
					if _, err := instabox("logout"); err != nil {
						return "", err
					}
					if cleanErr != nil {
						return "", fmt.Errorf("Signed out of instabox on %s, but: %w", server, cleanErr)
					}
					return fmt.Sprintf("Signed out of instabox on %s. The instabox CLI is signed out too.", server), nil
				})
			},
		}}
		return m, nil
	}
	return m, nil
}

// signIn hands the terminal to `instabox login`, which opens the browser or shows
// a device code. Switching accounts first clears the old account's machines.
func (m model) signIn(provider string) (tea.Model, tea.Cmd) {
	if m.acct.SignedIn() {
		if err := cleanupLocal(); err != nil {
			m.say(msgErr, "%v", err)
			return m, nil
		}
	}
	args := []string{"login", "--provider", provider}
	m.say(msgInfo, "Starting sign-in…")
	return m, interactive(instaboxBin(), args, func() (string, error) {
		acct := whoami()
		if !acct.SignedIn() {
			return "", fmt.Errorf("sign-in did not complete")
		}
		text, err := withInclude("Signed in to instabox as @" + acct.Login + ".")
		if err != nil {
			return "", err
		}
		// Bring the account's running machines into herdr now.
		_ = updateState(func(s *State) { s.LastSync = time.Time{} })
		return text, nil
	})
}
