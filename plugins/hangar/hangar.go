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

// The hangar CLI is the plugin's only path to the control plane: it owns the
// sign-in (~/.config/hangar/credentials.json), certificates and the API.

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
	Image        *struct {
		ID string `json:"id"`
	} `json:"image"`
	Spec    Spec `json:"spec"`
	Storage struct {
		SizeGiB int  `json:"sizeGiB"`
		Synced  bool `json:"synced"`
	} `json:"storage"`
	Runtime struct {
		Ready bool `json:"ready"`
	} `json:"runtime"`
	Metadata  map[string]string `json:"metadata"`
	LastError string            `json:"lastError"`
	CreatedAt time.Time         `json:"createdAt"`
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

type Image struct {
	ID              string       `json:"id"`
	Name            string       `json:"name"`
	Description     string       `json:"description"`
	SourceMachineID string       `json:"sourceMachineId"`
	Template        *TemplateRef `json:"template"`
	RootSizeBytes   int64        `json:"rootSizeBytes"`
	ExclusiveBytes  int64        `json:"exclusiveBytes"`
	CreatedAt       time.Time    `json:"createdAt"`
	Official        bool         `json:"official"`
	Owned           bool         `json:"owned"`
}

type Usage struct {
	ComputedAt  *time.Time `json:"computedAt"`
	StoredBytes int64      `json:"storedBytes"`
	Machines    int        `json:"machines"`
	Images      int        `json:"images"`
	Limits      struct {
		MaxMachines  int `json:"maxMachines"`
		MaxImages    int `json:"maxImages"`
		MaxStoredGiB int `json:"maxStoredGiB"`
	} `json:"limits"`
}

// Account is the parsed `hangar whoami`.
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

func hangarBin() string {
	if bin := os.Getenv("HANGAR_BIN"); bin != "" {
		return bin
	}
	if path, err := exec.LookPath("hangar"); err == nil {
		return path
	}
	if home, err := os.UserHomeDir(); err == nil {
		path := filepath.Join(home, ".local", "bin", "hangar")
		if _, err := os.Stat(path); err == nil {
			return path
		}
	}
	return "hangar"
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
		msg = strings.TrimPrefix(msg, "hangar: ")
		msg = strings.TrimPrefix(msg, "herdr: ")
		return stdout.String(), errors.New(msg)
	}
	return stdout.String(), nil
}

func hangar(args ...string) (string, error) { return run(hangarBin(), args...) }

func hangarJSON(v any, args ...string) error {
	out, err := hangar(append(args, "--json")...)
	if err != nil {
		return err
	}
	if err := json.Unmarshal([]byte(out), v); err != nil {
		return fmt.Errorf("hangar %s: unexpected output: %w", args[0], err)
	}
	return nil
}

var whoamiPattern = regexp.MustCompile(`^(\S+) \(GitHub user (\d+)\) on (\S+)`)

func whoami() Account {
	out, err := hangar("whoami")
	if err != nil {
		if strings.Contains(err.Error(), "not logged in") {
			return Account{SignedOut: true, Server: defaultServer()}
		}
		return Account{Err: err, Server: defaultServer()}
	}
	m := whoamiPattern.FindStringSubmatch(strings.TrimSpace(out))
	if m == nil {
		return Account{Err: fmt.Errorf("unexpected hangar whoami output: %s", strings.TrimSpace(out))}
	}
	return Account{Login: m[1], UserID: m[2], Server: m[3]}
}

func defaultServer() string {
	for _, env := range []string{"HANGAR_SERVER", "HANGAR_API_URL"} {
		if v := strings.TrimSpace(os.Getenv(env)); v != "" {
			return strings.TrimRight(v, "/")
		}
	}
	return "https://152.236.1.51"
}

func listMachines() ([]Machine, error) {
	var machines []Machine
	if err := hangarJSON(&machines, "ls"); err != nil {
		return nil, err
	}
	return machines, nil
}

func getMachine(id string) (Machine, error) {
	var m Machine
	err := hangarJSON(&m, "get", id)
	return m, err
}

func listTemplates() ([]Template, error) {
	var out struct {
		Templates []Template `json:"templates"`
	}
	if err := hangarJSON(&out, "templates"); err != nil {
		return nil, err
	}
	return out.Templates, nil
}

// templateFor finds the catalog entry a machine or image was made from.
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

// listImages returns the account's own images, newest first.
func listImages() ([]Image, error) {
	var all []Image
	if err := hangarJSON(&all, "image", "ls"); err != nil {
		return nil, err
	}
	var images []Image
	for _, im := range all {
		if im.Owned {
			images = append(images, im)
		}
	}
	sort.SliceStable(images, func(i, j int) bool {
		if !images[i].CreatedAt.Equal(images[j].CreatedAt) {
			return images[i].CreatedAt.After(images[j].CreatedAt)
		}
		return images[i].Name < images[j].Name
	})
	return images, nil
}

func getUsage() (Usage, error) {
	var u Usage
	err := hangarJSON(&u, "usage")
	return u, err
}

// notFound tells a deleted machine or image from other failures.
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

var invitePattern = regexp.MustCompile(`^hgi_[A-Za-z0-9]{20}$`)
