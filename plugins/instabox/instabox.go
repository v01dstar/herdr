package main

import (
	"bytes"
	"errors"
	"net/http"
	"os/exec"
	"regexp"
	"sort"
	"strings"
	"time"
)

// Types of the instabox API that the plugin reads.

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

// Account is who the plugin is signed in as.
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

// run executes a command without a terminal and returns its stdout. A failure
// carries the last line of stderr, which is where herdr explains itself.
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
		msg = strings.TrimPrefix(msg, "herdr: ")
		return stdout.String(), errors.New(msg)
	}
	return stdout.String(), nil
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
	var out struct {
		Snapshots []Snapshot `json:"snapshots"`
	}
	if err := call(request{method: http.MethodGet, path: "/v1/snapshots", out: &out, auth: true}); err != nil {
		return nil, err
	}
	snapshots := out.Snapshots
	sort.SliceStable(snapshots, func(i, j int) bool {
		if !snapshots[i].CreatedAt.Equal(snapshots[j].CreatedAt) {
			return snapshots[i].CreatedAt.After(snapshots[j].CreatedAt)
		}
		return snapshots[i].Name < snapshots[j].Name
	})
	return snapshots, nil
}

var namePattern = regexp.MustCompile(`^[a-z0-9][a-z0-9-]{0,62}$`)

func validName(name string) bool { return namePattern.MatchString(name) }
