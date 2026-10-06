package main

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"time"
)

// Long operations run as detached `herdr-hangar job <id>` processes, so closing
// the settings pane never interrupts one. Each job records its progress and
// result in a file the pane polls; a result that arrives while the pane is
// closed becomes a herdr notification.

type Job struct {
	ID          string    `json:"id"`
	Kind        string    `json:"kind"`
	MachineID   string    `json:"machineId,omitempty"`
	MachineName string    `json:"machineName,omitempty"`
	Name        string    `json:"name,omitempty"`        // new machine or image name
	Image       string    `json:"image,omitempty"`       // source image ID or name
	ImageName   string    `json:"imageName,omitempty"`   // for messages
	Description string    `json:"description,omitempty"` // image description
	Status      string    `json:"status"`                // running, done, failed
	Progress    string    `json:"progress,omitempty"`
	Result      string    `json:"result,omitempty"`
	PID         int       `json:"pid,omitempty"`
	Seen        bool      `json:"seen,omitempty"`
	Started     time.Time `json:"started"`
	Updated     time.Time `json:"updated"`
}

const (
	jobCreate      = "create"
	jobStart       = "start"
	jobResume      = "resume"
	jobSuspend     = "suspend"
	jobStop        = "stop"
	jobDelete      = "delete"
	jobClone       = "clone"
	jobSaveImage   = "save-image"
	jobDeleteImage = "delete-image"
)

func jobsDir() string { return filepath.Join(stateDir(), "jobs") }

func jobPath(id string) string { return filepath.Join(jobsDir(), id+".json") }

func (j *Job) save() error {
	j.Updated = time.Now()
	data, err := json.MarshalIndent(j, "", "  ")
	if err != nil {
		return err
	}
	if err := os.MkdirAll(jobsDir(), 0o700); err != nil {
		return err
	}
	tmp := jobPath(j.ID) + ".tmp"
	if err := os.WriteFile(tmp, data, 0o600); err != nil {
		return err
	}
	return os.Rename(tmp, jobPath(j.ID))
}

func loadJob(id string) (Job, error) {
	var j Job
	data, err := os.ReadFile(jobPath(id))
	if err != nil {
		return j, err
	}
	err = json.Unmarshal(data, &j)
	return j, err
}

// listJobs returns recent jobs, oldest first, marking ones whose process died.
func listJobs() []Job {
	entries, _ := os.ReadDir(jobsDir())
	var jobs []Job
	for _, e := range entries {
		id, ok := strings.CutSuffix(e.Name(), ".json")
		if !ok {
			continue
		}
		j, err := loadJob(id)
		if err != nil {
			continue
		}
		if j.Status == "running" && !processAlive(j.PID) && time.Since(j.Updated) > 5*time.Second {
			j.Status, j.Result = "failed", "The operation stopped unexpectedly."
			_ = j.save()
		}
		if j.Status != "running" && time.Since(j.Updated) > 24*time.Hour {
			_ = os.Remove(jobPath(id))
			_ = os.Remove(filepath.Join(jobsDir(), id+".log"))
			continue
		}
		jobs = append(jobs, j)
	}
	sort.Slice(jobs, func(a, b int) bool { return jobs[a].Started.Before(jobs[b].Started) })
	return jobs
}

func markJobSeen(id string) {
	if j, err := loadJob(id); err == nil && !j.Seen {
		j.Seen = true
		_ = j.save()
	}
}

// startJob records the job and launches its detached runner.
func startJob(j Job) (Job, error) {
	var b [6]byte
	_, _ = rand.Read(b[:])
	j.ID = time.Now().Format("20060102-150405") + "-" + hex.EncodeToString(b[:])
	j.Status = "running"
	j.Started = time.Now()
	if err := j.save(); err != nil {
		return j, err
	}
	self, err := os.Executable()
	if err != nil {
		return j, err
	}
	log, err := os.OpenFile(filepath.Join(jobsDir(), j.ID+".log"), os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o600)
	if err != nil {
		return j, err
	}
	defer log.Close()
	cmd := exec.Command(self, "job", j.ID)
	cmd.Stdout, cmd.Stderr = log, log
	detach(cmd)
	if err := cmd.Start(); err != nil {
		j.Status, j.Result = "failed", err.Error()
		_ = j.save()
		return j, err
	}
	j.PID = cmd.Process.Pid
	_ = j.save()
	_ = cmd.Process.Release()
	return j, nil
}

// runJob is the detached runner's entry point.
func runJob(id string) error {
	j, err := loadJob(id)
	if err != nil {
		return err
	}
	j.PID = os.Getpid()
	_ = j.save()
	progress := func(format string, args ...any) {
		j.Progress = fmt.Sprintf(format, args...)
		_ = j.save()
	}
	result, err := j.run(progress)
	if err != nil {
		j.Status, j.Result = "failed", err.Error()
	} else {
		j.Status, j.Result = "done", result
	}
	j.Progress = ""
	if !uiAlive() {
		title := "hangar"
		if j.Status == "failed" {
			title = "hangar: operation failed"
		}
		notify(title, j.Result)
		j.Seen = true
	}
	return j.save()
}

func (j *Job) run(progress func(string, ...any)) (string, error) {
	name := j.MachineName
	switch j.Kind {
	case jobCreate:
		return createMachine(j, progress)

	case jobStart:
		progress("Starting %s…", name)
		_ = updateState(func(s *State) { delete(s.Fence, j.MachineID) })
		if _, err := hangar("start", j.MachineID); err != nil {
			return "", err
		}
		if err := connectWhenReady(j.MachineID, progress, true); err != nil {
			return "", err
		}
		return name + " is ready; automatic connection enabled.", nil

	case jobResume:
		progress("Resuming %s…", name)
		_ = updateState(func(s *State) { delete(s.Fence, j.MachineID) })
		if _, err := hangar("resume", j.MachineID); err != nil {
			return "", err
		}
		if err := connectWhenReady(j.MachineID, progress, false); err != nil {
			return "", err
		}
		return name + " resumed; automatic connection enabled.", nil

	case jobSuspend:
		progress("Suspending %s…", name)
		if err := disconnect(j.MachineID); err != nil {
			return "", err
		}
		if _, err := hangar("suspend", j.MachineID); err != nil {
			return "", err
		}
		return name + ": suspended. Running programs continue after Resume.", nil

	case jobStop:
		progress("Stopping %s…", name)
		if err := disconnect(j.MachineID); err != nil {
			return "", err
		}
		warning := stopRemoteSession(j.MachineID)
		if _, err := hangar("stop", j.MachineID); err != nil {
			return "", err
		}
		if warning != "" {
			return name + ": stopped. Graceful shutdown warnings: " + warning, nil
		}
		return name + ": stopped.", nil

	case jobDelete:
		progress("Deleting %s…", name)
		_ = disconnect(j.MachineID)
		_ = stopRemoteSession(j.MachineID)
		_, err := hangar("rm", j.MachineID)
		switch {
		case notFound(err):
			_ = forgetMachine(j.MachineID)
			return fmt.Sprintf("hangar machine '%s' was already deleted.", name), nil
		case err != nil:
			return "", fmt.Errorf("Could not delete '%s': %v. It stays listed and does not reconnect for a few minutes. Retry Delete machine…, or Start it to keep using it.", name, err)
		}
		if err := forgetMachine(j.MachineID); err != nil {
			return "", err
		}
		return fmt.Sprintf("Deleted hangar machine '%s'.", name), nil

	case jobClone:
		stopped, err := prepareStopped(j.MachineID, name, progress)
		if err != nil {
			return "", err
		}
		progress("Cloning %s into %s…", name, j.Name)
		if _, err := hangar("fork", j.MachineID, j.Name); err != nil {
			return "", err
		}
		if m, err := getMachine(j.Name); err == nil && m.State == "running" {
			if err := connectWhenReady(m.ID, progress, true); err != nil {
				return "", err
			}
		}
		msg := fmt.Sprintf("Cloned %s into %s. %s is ready; select it in the sidebar.", name, j.Name, j.Name)
		if stopped {
			msg += fmt.Sprintf(" %s stays stopped; Start it to work on it again.", name)
		}
		return msg, nil

	case jobSaveImage:
		stopped, err := prepareStopped(j.MachineID, name, progress)
		if err != nil {
			return "", err
		}
		progress("Saving image %s from %s…", j.Name, name)
		args := []string{"image", "save", j.MachineID, "--name", j.Name}
		if j.Description != "" {
			args = append(args, "--description", j.Description)
		}
		if _, err := hangar(args...); err != nil {
			return "", err
		}
		msg := fmt.Sprintf("Saved image %s from %s. It is listed on the images tab; New machine from image… starts machines from it.", j.Name, name)
		if stopped {
			msg += fmt.Sprintf(" %s stays stopped; Start it to work on it again.", name)
		}
		return msg, nil

	case jobDeleteImage:
		progress("Deleting image %s…", j.ImageName)
		_, err := hangar("image", "rm", j.Image)
		if notFound(err) {
			return fmt.Sprintf("Image %s was already deleted.", j.ImageName), nil
		}
		if err != nil {
			return "", err
		}
		return fmt.Sprintf("Deleted image %s.", j.ImageName), nil
	}
	return "", fmt.Errorf("unknown job kind %q", j.Kind)
}

func createMachine(j *Job, progress func(string, ...any)) (string, error) {
	args := []string{"create", "--name", j.Name, "--no-wait"}
	if j.Image != "" {
		args = append(args, "--image", j.Image)
		progress("Creating hangar machine %s from image %s…", j.Name, j.ImageName)
	} else {
		args = append(args, "--template", "herdr")
		progress("Creating hangar machine %s…", j.Name)
	}
	if _, err := hangar(args...); err != nil {
		return "", err
	}
	deadline := time.Now().Add(15 * time.Minute)
	var m Machine
	for {
		var err error
		m, err = getMachine(j.Name)
		if err == nil {
			switch m.State {
			case "running":
				if err := connectWhenReady(m.ID, progress, true); err != nil {
					return "", err
				}
				return j.Name + " is ready. Select it in the sidebar.", nil
			case "stopped", "suspended", "error", "failed":
				return fmt.Sprintf("%s is listed. It is %s; Start it from hangar settings.", j.Name, m.State), nil
			}
			progress("hangar: %s %s…", j.Name, m.State)
		}
		if time.Now().After(deadline) {
			return "", fmt.Errorf("%s is still %s after 15 minutes; check hangar settings later", j.Name, m.State)
		}
		time.Sleep(2 * time.Second)
	}
}

// connectWhenReady waits for a running machine to accept SSH, then saves or
// re-enables it in herdr. fresh re-adds the profile so herdr starts a new
// remote session (the machine booted); otherwise the session survived.
func connectWhenReady(id string, progress func(string, ...any), fresh bool) error {
	m, err := getMachine(id)
	if err != nil {
		return err
	}
	if readState().Hidden[id] {
		return nil
	}
	progress("Connecting to %s…", m.Name)
	deadline := time.Now().Add(3 * time.Minute)
	for {
		if fresh {
			err = readdMachine(m)
		} else {
			err = reconcileMachine(m)
		}
		if err == nil {
			return nil
		}
		if time.Now().After(deadline) {
			return fmt.Errorf("%s is running but herdr could not connect: %w", m.Name, err)
		}
		_ = updateState(func(s *State) { delete(s.RetryAdd, id) })
		time.Sleep(10 * time.Second)
	}
}

// reconcileMachine applies the reconciliation rules to one machine only.
func reconcileMachine(m Machine) error {
	all, err := listProfiles()
	if err != nil {
		return err
	}
	managed, _ := splitProfiles(all)
	p, has := managed[m.ID]
	return reconcileOne(m, p, has, readState())
}

// stopRemoteSession asks the machine's Herdr session to shut down cleanly
// before the machine stops. Best effort: it returns a warning, never an error.
func stopRemoteSession(id string) string {
	_, err := hangar("exec", id, "--timeout", "20", "--",
		"herdr", "--session", RemoteSession, "server", "stop")
	if err != nil {
		return err.Error()
	}
	return ""
}

// prepareStopped brings a machine to a stopped, uploaded state so it can be
// cloned or saved. It reports whether it had to stop the machine.
func prepareStopped(id, name string, progress func(string, ...any)) (bool, error) {
	m, err := getMachine(id)
	if err != nil {
		return false, err
	}
	switch m.State {
	case "stopped":
		if m.Storage.Synced {
			return false, nil
		}
		progress("Uploading the changes on %s…", name)
		_, err = hangar("stop", id)
		return true, err
	case "suspended":
		progress("Resuming %s so it can stop…", name)
		if _, err := hangar("resume", id); err != nil {
			return false, err
		}
		fallthrough
	case "running", "error", "failed":
		progress("Stopping %s…", name)
		if err := disconnect(id); err != nil {
			return false, err
		}
		_ = stopRemoteSession(id)
		_, err = hangar("stop", id)
		return true, err
	}
	return false, errors.New("Wait until it is stopped or running, then try again.")
}
