package main

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"strings"
	"time"
)

// The plugin talks to the instabox API directly; it needs no instabox CLI.
// Only the calls the plugin uses are here.

const defaultServerURL = "https://api.box.instacloud.com"

// apiError is a non-2xx response.
type apiError struct {
	Status  int
	Code    string
	Message string
}

func (e *apiError) Error() string {
	if e.Message != "" {
		return e.Message
	}
	if e.Code != "" {
		return e.Code
	}
	return http.StatusText(e.Status)
}

func errorCode(err error) string {
	var ae *apiError
	if errors.As(err, &ae) {
		return ae.Code
	}
	return ""
}

// notFound tells a deleted machine or snapshot from other failures.
func notFound(err error) bool {
	var ae *apiError
	return errors.As(err, &ae) && (ae.Code == "not_found" || ae.Status == http.StatusNotFound)
}

// serverURL is the API server: INSTABOX_SERVER or INSTABOX_API_URL, then the
// signed-in server, then the default.
func serverURL() string {
	for _, env := range []string{"INSTABOX_SERVER", "INSTABOX_API_URL"} {
		if v := strings.TrimSpace(os.Getenv(env)); v != "" {
			return strings.TrimRight(v, "/")
		}
	}
	if c, err := loadCredentials(); err == nil && c != nil && c.Server != "" {
		return c.Server
	}
	return defaultServerURL
}

var httpClient = &http.Client{Timeout: 2 * time.Minute}

type request struct {
	method, path string
	in, out      any
	auth         bool
	mutate       bool // sends an Idempotency-Key
	server       string
	timeout      time.Duration
}

// call sends one request. An authenticated request that the server rejects
// with 401 is retried once after refreshing the access token.
func call(r request) error {
	err := send(r, false)
	if r.auth && errorCode(err) == "unauthenticated" {
		err = send(r, true)
	}
	return err
}

func send(r request, refresh bool) error {
	server := r.server
	if server == "" {
		server = serverURL()
	}
	timeout := r.timeout
	if timeout == 0 {
		timeout = 30 * time.Second
	}
	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()
	var body io.Reader
	if r.in != nil {
		data, err := json.Marshal(r.in)
		if err != nil {
			return err
		}
		body = bytes.NewReader(data)
	}
	req, err := http.NewRequestWithContext(ctx, r.method, server+r.path, body)
	if err != nil {
		return err
	}
	req.Header.Set("Accept", "application/json")
	req.Header.Set("User-Agent", "herdr-instabox")
	if r.in != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	if r.mutate {
		req.Header.Set("Idempotency-Key", newIdempotencyKey())
	}
	if r.auth {
		tok, err := accessToken(refresh)
		if err != nil {
			return err
		}
		req.Header.Set("Authorization", "Bearer "+tok)
	}
	hc := *httpClient
	hc.Timeout = timeout
	resp, err := hc.Do(req)
	if err != nil {
		return fmt.Errorf("cannot reach %s: %w", server, unwrapURLError(err))
	}
	defer resp.Body.Close()
	data, err := io.ReadAll(io.LimitReader(resp.Body, 16<<20))
	if err != nil {
		return err
	}
	if resp.StatusCode < 200 || resp.StatusCode > 299 {
		return decodeError(resp.StatusCode, data)
	}
	if r.out == nil || len(bytes.TrimSpace(data)) == 0 {
		return nil
	}
	if err := json.Unmarshal(data, r.out); err != nil {
		return fmt.Errorf("%s %s: unexpected response: %w", r.method, r.path, err)
	}
	return nil
}

func unwrapURLError(err error) error {
	var ue *url.Error
	if errors.As(err, &ue) {
		return ue.Err
	}
	return err
}

func decodeError(status int, data []byte) error {
	var body struct {
		Error struct {
			Code    string `json:"code"`
			Message string `json:"message"`
		} `json:"error"`
	}
	if json.Unmarshal(data, &body) == nil && body.Error.Code != "" {
		return &apiError{Status: status, Code: body.Error.Code, Message: body.Error.Message}
	}
	code := "bad_request"
	switch {
	case status == http.StatusUnauthorized:
		code = "unauthenticated"
	case status == http.StatusNotFound:
		code = "not_found"
	case status >= 500:
		code = "internal"
	}
	msg := strings.TrimSpace(string(data))
	if len(msg) > 200 {
		msg = msg[:200] + "…"
	}
	return &apiError{Status: status, Code: code, Message: msg}
}

func newIdempotencyKey() string {
	var b [16]byte
	_, _ = rand.Read(b[:])
	return "ik_" + hex.EncodeToString(b[:])
}

func machinePath(id, suffix string) string { return "/v1/machines/" + url.PathEscape(id) + suffix }

// Operation is an asynchronous machine change.
type Operation struct {
	ID        string `json:"id"`
	MachineID string `json:"machineId"`
	State     string `json:"state"` // queued, running, succeeded, failed
	Phase     string `json:"phase"`
	Error     *struct {
		Message string `json:"message"`
	} `json:"error"`
}

func (o Operation) done() bool { return o.State == "succeeded" || o.State == "failed" }

// mutateMachine starts an operation and waits for it to finish.
func mutateMachine(method, path string, in any) (Operation, error) {
	var op Operation
	if err := call(request{method: method, path: path, in: in, out: &op, auth: true, mutate: true}); err != nil {
		return op, err
	}
	return waitOperation(op)
}

// waitOperation polls an operation until it is done. A failed operation is an
// error. Brief network failures are retried.
func waitOperation(op Operation) (Operation, error) {
	delay := 250 * time.Millisecond
	failures := 0
	deadline := time.Now().Add(30 * time.Minute)
	for !op.done() {
		if time.Now().After(deadline) {
			return op, fmt.Errorf("operation %s is still %s after 30 minutes", op.ID, op.State)
		}
		time.Sleep(delay)
		delay = min(delay*3/2, 2*time.Second)
		var next Operation
		err := call(request{method: http.MethodGet, path: "/v1/operations/" + url.PathEscape(op.ID), out: &next, auth: true})
		var ae *apiError
		switch {
		case err == nil:
			op, failures = next, 0
		case errors.As(err, &ae) && ae.Status < 500:
			return op, err
		default:
			if failures++; failures >= 10 {
				return op, err
			}
		}
	}
	if op.State == "failed" {
		if op.Error != nil && op.Error.Message != "" {
			return op, errors.New(op.Error.Message)
		}
		return op, errors.New("the operation failed")
	}
	return op, nil
}

func listMachines() ([]Machine, error) {
	var all []Machine
	cursor := ""
	for range 1000 {
		path := "/v1/machines"
		if cursor != "" {
			path += "?cursor=" + url.QueryEscape(cursor)
		}
		var page struct {
			Machines   []Machine `json:"machines"`
			NextCursor *string   `json:"nextCursor"`
		}
		if err := call(request{method: http.MethodGet, path: path, out: &page, auth: true}); err != nil {
			return nil, err
		}
		all = append(all, page.Machines...)
		if page.NextCursor == nil || *page.NextCursor == "" || *page.NextCursor == cursor {
			return all, nil
		}
		cursor = *page.NextCursor
	}
	return nil, errors.New("the machine list did not end")
}

func getMachine(id string) (Machine, error) {
	var m Machine
	err := call(request{method: http.MethodGet, path: machinePath(id, ""), out: &m, auth: true})
	return m, err
}

func machineByName(name string) (Machine, error) {
	machines, err := listMachines()
	if err != nil {
		return Machine{}, err
	}
	for _, m := range machines {
		if m.Name == name {
			return m, nil
		}
	}
	return Machine{}, &apiError{Status: http.StatusNotFound, Code: "not_found", Message: "machine " + name + " not found"}
}

func listTemplates() ([]Template, error) {
	var out struct {
		Templates []Template `json:"templates"`
	}
	err := call(request{method: http.MethodGet, path: "/v1/templates", out: &out, auth: true})
	return out.Templates, err
}

func getUsage() (Usage, error) {
	var u Usage
	err := call(request{method: http.MethodGet, path: "/v1/usage", out: &u, auth: true})
	return u, err
}

func createMachine(name, template, snapshot string) (Operation, error) {
	in := map[string]string{"name": name}
	if snapshot != "" {
		in["snapshotId"] = snapshot
	} else {
		in["templateId"] = template
	}
	var op Operation
	err := call(request{method: http.MethodPost, path: "/v1/machines", in: in, out: &op, auth: true, mutate: true})
	return op, err
}

func startMachine(id string) error {
	_, err := mutateMachine(http.MethodPost, machinePath(id, "/start"), struct{}{})
	return err
}

func stopMachine(id string) error {
	_, err := mutateMachine(http.MethodPost, machinePath(id, "/stop"), struct{}{})
	return err
}

func suspendMachine(id string) error {
	_, err := mutateMachine(http.MethodPost, machinePath(id, "/suspend"), struct{}{})
	return err
}

func resumeMachine(id string) error {
	_, err := mutateMachine(http.MethodPost, machinePath(id, "/resume"), struct{}{})
	return err
}

func deleteMachine(id string) error {
	_, err := mutateMachine(http.MethodDelete, machinePath(id, ""), nil)
	return err
}

func forkMachine(id, name string) error {
	_, err := mutateMachine(http.MethodPost, machinePath(id, "/fork"), map[string]string{"name": name})
	return err
}

func createSnapshot(machineID, name, description string) error {
	in := map[string]string{"name": name}
	if description != "" {
		in["description"] = description
	}
	return call(request{method: http.MethodPost, path: machinePath(machineID, "/snapshots"), in: in,
		auth: true, mutate: true, timeout: 30 * time.Minute})
}

func deleteSnapshot(id string) error {
	return call(request{method: http.MethodDelete, path: "/v1/snapshots/" + url.PathEscape(id), auth: true, mutate: true})
}

// execOnMachine runs cmd with bash -lc as the machine's user.
func execOnMachine(id, cmd string, timeout int) error {
	var out struct {
		Stderr   string `json:"stderr"`
		ExitCode int    `json:"exitCode"`
		TimedOut bool   `json:"timedOut"`
	}
	err := call(request{method: http.MethodPost, path: machinePath(id, "/exec"), auth: true,
		in:  map[string]any{"cmd": cmd, "timeoutSeconds": timeout},
		out: &out, timeout: time.Duration(timeout+30) * time.Second})
	switch {
	case err != nil:
		return err
	case out.TimedOut:
		return fmt.Errorf("timed out after %ds", timeout)
	case out.ExitCode != 0:
		msg := strings.TrimSpace(out.Stderr)
		if lines := strings.Split(msg, "\n"); msg != "" {
			msg = lines[len(lines)-1]
		}
		return fmt.Errorf("exit %d: %s", out.ExitCode, msg)
	}
	return nil
}
