package main

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"
)

// certMargin is how long before expiry a certificate is refreshed.
const certMargin = 10 * time.Minute

func hostsDir() string { return filepath.Join(stateDir(), "ssh") }

func hostFile(id string) string { return filepath.Join(hostsDir(), id+".conf") }

func userSSHConfig() string {
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".ssh", "config")
}

func includeLine() string {
	return fmt.Sprintf("Include %q", filepath.Join(hostsDir(), "*.conf"))
}

func sshIncluded() bool {
	data, err := os.ReadFile(userSSHConfig())
	if err != nil {
		return false
	}
	want := includeLine()
	for _, line := range strings.Split(string(data), "\n") {
		if strings.TrimSpace(line) == want {
			return true
		}
	}
	return false
}

// addInclude prepends the Include line to ~/.ssh/config. It must come first:
// an Include after a Host line would only apply inside that Host block.
func addInclude() error {
	path := userSSHConfig()
	old, err := os.ReadFile(path)
	if err != nil && !os.IsNotExist(err) {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	if len(old) > 0 {
		if err := os.WriteFile(path+".herdr-instabox.bak", old, 0o600); err != nil {
			return err
		}
	}
	return os.WriteFile(path, append([]byte(includeHeader()), old...), 0o600)
}

const includeComment = "# Added by the herdr instabox plugin."

func includeHeader() string { return includeComment + "\n" + includeLine() + "\n\n" }

// removeInclude takes out what addInclude added and leaves the rest untouched.
func removeInclude() error {
	path := userSSHConfig()
	data, err := os.ReadFile(path)
	if os.IsNotExist(err) {
		return nil
	} else if err != nil {
		return err
	}
	text := string(data)
	if rest, ok := strings.CutPrefix(text, includeHeader()); ok {
		text = rest
	} else {
		var kept []string
		for _, line := range strings.Split(text, "\n") {
			if t := strings.TrimSpace(line); t != includeLine() && t != includeComment {
				kept = append(kept, line)
			}
		}
		text = strings.Join(kept, "\n")
	}
	if text == string(data) {
		return nil
	}
	return os.WriteFile(path, []byte(text), 0o600)
}

// writeHostBlock asks instabox for the machine's Host block (which also issues a
// certificate), renames the host to the managed alias and saves it. A Match exec
// line in front refreshes the short-lived certificate before every connection.
func writeHostBlock(m Machine) error {
	out, err := instabox("ssh-config", m.ID)
	if err != nil {
		return err
	}
	self, err := os.Executable()
	if err != nil {
		return err
	}
	var b strings.Builder
	fmt.Fprintf(&b, "# instabox machine %s (%s), managed by herdr-instabox\n", m.Name, m.ID)
	fmt.Fprintf(&b, "Match host %s exec \"%s ensure-cert %s %s\"\n",
		m.Target(), shellQuote(self), m.ID, shellQuote(instaboxBin()))
	for _, line := range strings.Split(out, "\n") {
		trimmed := strings.TrimSpace(line)
		switch {
		case trimmed == "" || strings.HasPrefix(trimmed, "#"):
		case strings.HasPrefix(trimmed, "Host "):
			fmt.Fprintf(&b, "Host %s\n", m.Target())
		default:
			b.WriteString(line + "\n")
		}
	}
	if err := os.MkdirAll(hostsDir(), 0o700); err != nil {
		return err
	}
	return os.WriteFile(hostFile(m.ID), []byte(b.String()), 0o600)
}

func hasHostBlock(id string) bool {
	_, err := os.Stat(hostFile(id))
	return err == nil
}

func removeHostBlock(id string) {
	_ = os.Remove(hostFile(id))
}

func shellQuote(s string) string {
	return "'" + strings.ReplaceAll(s, "'", `'\''`) + "'"
}

// ensureCert runs from ssh's Match exec before each connection. It refreshes the
// certificate only near expiry and always succeeds: a failed refresh surfaces as
// an SSH authentication error, which herdr already reports.
func ensureCert(id, instaboxPath string) {
	cert := filepath.Join(instaboxConfigDir(), "ssh", id+"-cert.pub")
	if until, ok := certValidUntil(cert); ok && time.Until(until) > certMargin {
		return
	}
	cmd := exec.Command(instaboxPath, "ssh-cert", id)
	_ = cmd.Run()
}

// instaboxConfigDir is where the instabox CLI keeps its sign-in and certificates.
func instaboxConfigDir() string {
	if x := os.Getenv("XDG_CONFIG_HOME"); x != "" {
		return filepath.Join(x, "instabox")
	}
	home, _ := os.UserHomeDir()
	return filepath.Join(home, ".config", "instabox")
}

func certValidUntil(path string) (time.Time, bool) {
	out, err := exec.Command("ssh-keygen", "-L", "-f", path).Output()
	if err != nil {
		return time.Time{}, false
	}
	for _, line := range strings.Split(string(out), "\n") {
		_, rest, ok := strings.Cut(strings.TrimSpace(line), "Valid: from ")
		if !ok {
			continue
		}
		_, to, ok := strings.Cut(rest, " to ")
		if !ok {
			return time.Time{}, false
		}
		until, err := time.ParseInLocation("2006-01-02T15:04:05", strings.TrimSpace(to), time.Local)
		return until, err == nil
	}
	return time.Time{}, false
}
