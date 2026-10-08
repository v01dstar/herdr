//go:build unix

package main

import (
	"os/exec"
	"syscall"
)

// detach starts cmd in its own session, so it outlives the settings pane and
// is not signalled when herdr closes the pane's process group.
func detach(cmd *exec.Cmd) {
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
}

func processAlive(pid int) bool {
	if pid <= 0 {
		return false
	}
	err := syscall.Kill(pid, 0)
	return err == nil || err == syscall.EPERM
}
