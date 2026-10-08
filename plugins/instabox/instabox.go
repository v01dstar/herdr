package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"time"
)

// The instabox CLI is the plugin's only path to the control plane: it owns the
// sign-in (~/.config/instabox/credentials.json), certificates and the API.

type Spec struct {
	VCPUs             int `json:"vcpus"`
	MemMiB            int `json:"memMiB"`
	PersistentDiskGiB int `json:"persistentDiskGiB"`
	RootDiskGiB       int `json:"rootDiskGiB"`
}

type TemplateRef struct {
	ID      string `json:"id"`
	Version string `json:"version"`
}

func (t *TemplateRef) Label() string {
	if t == nil {
		return ""
	}
	return t.ID + "@" + t.Version
}

type Machine struct {
	ID           string       `json:"id"`
	Name         string       `json:"name"`
	State        string       `json:"state"`
	DesiredState string       `json:"desiredState"`
	OperationID  *string      `json:"operationId"`
	Template     *TemplateRef `json:"template"`
	Snapshot     *struct {
		ID string `json:"id"`
	} `json:"snapshot"`
	Spec    Spec `json:"spec"`
	Storage struct {
		SizeGiB int  `json:"sizeGiB"`
		Synced  bool `json:"synced"`
	} `json:"storage"`
	Runtime struct {
		Ready bool `json:"ready"`
	} `json:"runtime"`
	Metadata  map[string]string `json:"metadata"`
	LastError *struct {
		Message string `json:"message"`
	} `json:"lastError"`
	CreatedAt time.Time `json:"createdAt"`
}

func (m Machine) Target() string { return TargetPrefix + m.ID }

type Template struct {
	ID           string   `json:"id"`
	Version      string   `json:"version"`
	Description  string   `json:"description"`
	Capabilities []string `json:"capabilities"`
	DefaultSpec  Spec     `json:"defaultSpec"`
}

func (t Template) Has(capability string) bool {
	for _, c := range t.Capabilities {
		if c == capability {
			return true
		}
	}
	return false
}

type Snapshot struct {
	ID              string       `json:"id"`
	Name            string       `json:"name"`
	Description     string       `json:"description"`
	SourceMachineID string       `json:"sourceMachineId"`
	Template        *TemplateRef `json:"template"`
	RootSizeBytes   int64        `json:"rootSizeBytes"`
	// DataSizeBytes is 0 for a root-only snapshot (older ones are).
	DataSizeBytes  int64     `json:"dataSizeBytes"`
	ExclusiveBytes int64     `json:"exclusiveBytes"`
	CreatedAt      time.Time `json:"createdAt"`
}

func (s Snapshot) HasData() bool { return s.DataSizeBytes > 0 }

type Usage struct {
	ComputedAt  *time.Time `json:"computedAt"`
	StoredBytes int64      `json:"storedBytes"`
	Machines    int        `json:"machines"`
	Snapshots   int        `json:"snapshots"`
	Limits      struct {
		MaxMachines  int `json:"maxMachines"`
		MaxSnapshots int `json:"maxSnapshots"`
		MaxStoredGiB int `json:"maxStoredGiB"`
	} `json:"limits"`
}

// Account is the parsed `instabox whoami`.
type Account struct {
	Login, UserID, Server string
	// SignedOut is a definite "nobody is signed in"; Err is any other failure,
	// such as an unreachable server, which must not be mistaken for it.
	SignedOut bool
	Err       error
}

func (a Account) SignedIn() bool { return a.Login != "" }

// Key identifies an account across sign-ins.
func (a Account) Key() string { return a.UserID + "@" + a.Server }

func instaboxBin() string {
	if bin := os.Getenv("INSTABOX_BIN"); bin != "" {
		return bin
	}
	if path, err := exec.LookPath("instabox"); err == nil {
		return path
	}
	if home, err := os.UserHomeDir(); err == nil {
		path := filepath.Join(home, ".local", "bin", "instabox")
		if _, err := os.Stat(path); err == nil {
			return path
		}
	}
	return "instabox"
}

// run executes a command without a terminal and returns its stdout. A failure
// carries the last line of stderr, which is where both CLIs explain themselves.
func run(name string, args ...string) (string, error) {
	cmd := exec.Command(name, args...)
	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		msg := strings.TrimSpace(stderr.String())
		if msg == "" {
			msg = strings.TrimSpace(stdout.String())
		}
		if lines := strings.Split(msg, "\n"); msg != "" {
			msg = strings.TrimSpace(lines[len(lines)-1])
		} else {
			msg = err.Error()
		}
		msg = strings.TrimPrefix(msg, "instabox: ")
		msg = strings.TrimPrefix(msg, "herdr: ")
		return stdout.String(), errors.New(msg)
	}
	return stdout.String(), nil
}

func instabox(args ...string) (string, error) { return run(instaboxBin(), args...) }

func instaboxJSON(v any, args ...string) error {
	out, err := instabox(append(args, "--json")...)
	if err != nil {
		return err
	}
	if err := json.Unmarshal([]byte(out), v); err != nil {
		return fmt.Errorf("instabox %s: unexpected output: %w", args[0], err)
	}
	return nil
}

var whoamiPattern = regexp.MustCompile(`^(\S+) \(user (\d+)\) on (\S+)`)

func whoami() Account {
	out, err := instabox("whoami")
	if err != nil {
		if strings.Contains(err.Error(), "not logged in") {
			return Account{SignedOut: true, Server: defaultServer()}
		}
		return Account{Err: err, Server: defaultServer()}
	}
	m := whoamiPattern.FindStringSubmatch(strings.TrimSpace(out))
	if m == nil {
		return Account{Err: fmt.Errorf("unexpected instabox whoami output: %s", strings.TrimSpace(out))}
	}
	return Account{Login: m[1], UserID: m[2], Server: m[3]}
}

func defaultServer() string {
	for _, env := range []string{"INSTABOX_SERVER", "INSTABOX_API_URL"} {
		if v := strings.TrimSpace(os.Getenv(env)); v != "" {
			return strings.TrimRight(v, "/")
		}
	}
	return "https://api.box.instacloud.com"
}

func listMachines() ([]Machine, error) {
	var machines []Machine
	if err := instaboxJSON(&machines, "ls"); err != nil {
		return nil, err
	}
	return machines, nil
}

func getMachine(id string) (Machine, error) {
	var m Machine
	err := instaboxJSON(&m, "get", id)
	return m, err
}

func listTemplates() ([]Template, error) {
	var out struct {
		Templates []Template `json:"templates"`
	}
	if err := instaboxJSON(&out, "templates"); err != nil {
		return nil, err
	}
	return out.Templates, nil
}

// templateFor finds the catalog entry a machine or snapshot was made from.
func templateFor(templates []Template, ref *TemplateRef) (Template, bool) {
	if ref == nil {
		return Template{}, false
	}
	for _, t := range templates {
		if t.ID == ref.ID && t.Version == ref.Version {
			return t, true
		}
	}
	return Template{}, false
}

// listSnapshots returns the account's snapshots (always private), newest first.
func listSnapshots() ([]Snapshot, error) {
	var snapshots []Snapshot
	if err := instaboxJSON(&snapshots, "snapshot", "ls"); err != nil {
		return nil, err
	}
	sort.SliceStable(snapshots, func(i, j int) bool {
		if !snapshots[i].CreatedAt.Equal(snapshots[j].CreatedAt) {
			return snapshots[i].CreatedAt.After(snapshots[j].CreatedAt)
		}
		return snapshots[i].Name < snapshots[j].Name
	})
	return snapshots, nil
}

func getUsage() (Usage, error) {
	var u Usage
	err := instaboxJSON(&u, "usage")
	return u, err
}

// notFound tells a deleted machine or snapshot from other failures.
func notFound(err error) bool {
	if err == nil {
		return false
	}
	s := strings.ToLower(err.Error())
	return strings.Contains(s, "not found") || strings.Contains(s, "no such") ||
		strings.Contains(s, "does not exist")
}

var namePattern = regexp.MustCompile(`^[a-z0-9][a-z0-9-]{0,62}$`)

func validName(name string) bool { return namePattern.MatchString(name) }
