package main

import (
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"sync"
	"testing"
	"time"
)

// fakeAPI is an instabox API server for tests. Its state lives in
// $FAKE_DIR/instabox.json (fakeInstabox), so tests can read and change it.
type fakeAPI struct {
	dir string
	mu  sync.Mutex
	// refreshes counts token refreshes; certs counts issued certificates.
	refreshes, certs int
	// devicePolls is how many device token polls return
	// authorization_pending before the sign-in is approved.
	devicePolls int
	challenge   string // the last browser sign-in's PKCE challenge
}

func startFakeAPI(t *testing.T, dir string) (*fakeAPI, string) {
	t.Helper()
	f := &fakeAPI{dir: dir}
	srv := httptest.NewServer(f.handler())
	t.Cleanup(srv.Close)
	return f, srv.URL
}

func (f *fakeAPI) load() fakeInstabox {
	var st fakeInstabox
	if data, err := os.ReadFile(filepath.Join(f.dir, "instabox.json")); err == nil {
		_ = json.Unmarshal(data, &st)
	}
	if st.Machines == nil {
		st.Machines = []map[string]any{}
	}
	if st.Snapshots == nil {
		st.Snapshots = []map[string]any{}
	}
	return st
}

func (f *fakeAPI) save(st fakeInstabox) {
	data, _ := json.Marshal(st)
	tmp := filepath.Join(f.dir, "instabox.json.tmp")
	_ = os.WriteFile(tmp, data, 0o600)
	_ = os.Rename(tmp, filepath.Join(f.dir, "instabox.json"))
}

func writeJSON(w http.ResponseWriter, status int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(v)
}

func writeErr(w http.ResponseWriter, status int, code, msg string) {
	writeJSON(w, status, map[string]any{"error": map[string]any{"code": code, "message": msg}})
}

func succeeded(machineID string) map[string]any {
	return map[string]any{"id": "op_" + machineID, "machineId": machineID, "state": "succeeded"}
}

func (f *fakeAPI) handler() http.Handler {
	mux := http.NewServeMux()
	// Every handler runs under f.mu with the state loaded; it returns whether
	// the state changed.
	handle := func(pattern string, auth bool, h func(w http.ResponseWriter, r *http.Request, st *fakeInstabox) bool) {
		mux.HandleFunc(pattern, func(w http.ResponseWriter, r *http.Request) {
			f.mu.Lock()
			defer f.mu.Unlock()
			st := f.load()
			if auth && (!st.SignedIn || r.Header.Get("Authorization") == "") {
				writeErr(w, 401, "unauthenticated", "sign in again")
				return
			}
			if h(w, r, &st) {
				f.save(st)
			}
		})
	}
	find := func(st *fakeInstabox, id string) map[string]any {
		for _, m := range st.Machines {
			if m["id"] == id {
				return m
			}
		}
		return nil
	}
	machine := func(w http.ResponseWriter, r *http.Request, st *fakeInstabox) map[string]any {
		m := find(st, r.PathValue("id"))
		if m == nil {
			writeErr(w, 404, "not_found", "machine "+r.PathValue("id")+" not found")
		}
		return m
	}
	newMachine := func(st *fakeInstabox, name string) map[string]any {
		m := fakeMachine(fmt.Sprintf("m_fake%04d", len(st.Machines)+len(st.Snapshots)+1), name, "running")
		st.Machines = append(st.Machines, m)
		return m
	}

	handle("POST /v1/auth/refresh", false, func(w http.ResponseWriter, r *http.Request, st *fakeInstabox) bool {
		if !st.SignedIn {
			writeErr(w, 401, "unauthenticated", "refresh token revoked")
			return false
		}
		f.refreshes++
		writeJSON(w, 200, map[string]any{"accessToken": fmt.Sprintf("at-%d", f.refreshes),
			"accessExpiresAt": time.Now().Add(time.Hour), "refreshToken": fmt.Sprintf("rt-%d", f.refreshes),
			"refreshExpiresAt": time.Now().Add(24 * time.Hour)})
		return false
	})
	handle("POST /v1/auth/logout", true, func(w http.ResponseWriter, r *http.Request, st *fakeInstabox) bool {
		w.WriteHeader(204)
		return false
	})
	// The browser sign-in: the start page signs in at once and redirects to
	// the client's callback with a code bound to its PKCE challenge.
	handle("GET /auth/cli/start", false, func(w http.ResponseWriter, r *http.Request, st *fakeInstabox) bool {
		q := r.URL.Query()
		if q.Get("redirect_uri") == "" {
			writeErr(w, 400, "bad_request", "missing parameters")
			return false
		}
		f.challenge = q.Get("code_challenge")
		http.Redirect(w, r, q.Get("redirect_uri")+"?code=ibc_fake&state="+url.QueryEscape(q.Get("state")), http.StatusFound)
		return false
	})
	handle("POST /v1/auth/cli/token", false, func(w http.ResponseWriter, r *http.Request, st *fakeInstabox) bool {
		var in map[string]string
		_ = json.NewDecoder(r.Body).Decode(&in)
		sum := sha256.Sum256([]byte(in["codeVerifier"]))
		if in["code"] != "ibc_fake" || base64.RawURLEncoding.EncodeToString(sum[:]) != f.challenge {
			writeErr(w, 400, "invalid_grant", "invalid code or verifier")
			return false
		}
		st.SignedIn = true
		writeJSON(w, 200, map[string]any{"accessToken": "at-browser", "accessExpiresAt": time.Now().Add(time.Hour),
			"refreshToken": "rt-browser", "refreshExpiresAt": time.Now().Add(24 * time.Hour)})
		return true
	})
	handle("POST /v1/auth/device", false, func(w http.ResponseWriter, r *http.Request, st *fakeInstabox) bool {
		writeJSON(w, 200, map[string]any{"deviceCode": "dc", "userCode": "ABCD-1234",
			"verificationUri": "https://instabox.test/device", "interval": 1, "expiresIn": 60})
		return false
	})
	handle("POST /v1/auth/device/token", false, func(w http.ResponseWriter, r *http.Request, st *fakeInstabox) bool {
		if f.devicePolls > 0 {
			f.devicePolls--
			writeErr(w, 400, "authorization_pending", "not yet")
			return false
		}
		st.SignedIn = true
		writeJSON(w, 200, map[string]any{"accessToken": "at-device", "accessExpiresAt": time.Now().Add(time.Hour),
			"refreshToken": "rt-device", "refreshExpiresAt": time.Now().Add(24 * time.Hour)})
		return true
	})
	handle("GET /v1/me", true, func(w http.ResponseWriter, r *http.Request, st *fakeInstabox) bool {
		writeJSON(w, 200, map[string]any{"userId": 42, "login": "tester"})
		return false
	})
	handle("GET /v1/templates", true, func(w http.ResponseWriter, r *http.Request, st *fakeInstabox) bool {
		writeJSON(w, 200, map[string]any{"templates": []any{map[string]any{"id": "herdr", "version": "2026-10-05.1",
			"capabilities": []string{"identity-reset"}, "defaultSpec": map[string]any{"vcpus": 4, "memMiB": 8192}}}})
		return false
	})
	handle("GET /v1/usage", true, func(w http.ResponseWriter, r *http.Request, st *fakeInstabox) bool {
		writeJSON(w, 200, map[string]any{"storedBytes": 1 << 30, "machines": len(st.Machines), "snapshots": len(st.Snapshots),
			"limits": map[string]any{"maxMachines": 10, "maxSnapshots": 10, "maxStoredGiB": 200}})
		return false
	})
	handle("GET /v1/machines", true, func(w http.ResponseWriter, r *http.Request, st *fakeInstabox) bool {
		writeJSON(w, 200, map[string]any{"machines": st.Machines, "nextCursor": nil})
		return false
	})
	handle("POST /v1/machines", true, func(w http.ResponseWriter, r *http.Request, st *fakeInstabox) bool {
		var in map[string]string
		_ = json.NewDecoder(r.Body).Decode(&in)
		m := newMachine(st, in["name"])
		if in["snapshotId"] != "" {
			m["snapshot"] = map[string]any{"id": in["snapshotId"]}
		}
		writeJSON(w, 202, succeeded(m["id"].(string)))
		return true
	})
	handle("GET /v1/machines/{id}", true, func(w http.ResponseWriter, r *http.Request, st *fakeInstabox) bool {
		if m := machine(w, r, st); m != nil {
			writeJSON(w, 200, m)
		}
		return false
	})
	handle("DELETE /v1/machines/{id}", true, func(w http.ResponseWriter, r *http.Request, st *fakeInstabox) bool {
		m := machine(w, r, st)
		if m == nil {
			return false
		}
		for i, x := range st.Machines {
			if x["id"] == m["id"] {
				st.Machines = append(st.Machines[:i], st.Machines[i+1:]...)
				break
			}
		}
		writeJSON(w, 202, succeeded(m["id"].(string)))
		return true
	})
	handle("POST /v1/machines/{id}/{action}", true, func(w http.ResponseWriter, r *http.Request, st *fakeInstabox) bool {
		m := machine(w, r, st)
		if m == nil {
			return false
		}
		id := m["id"].(string)
		switch r.PathValue("action") {
		case "start", "resume":
			m["state"] = "running"
		case "stop":
			m["state"] = "stopped"
		case "suspend":
			m["state"] = "suspended"
		case "fork":
			if m["state"] != "stopped" {
				writeErr(w, 409, "conflict", "machine must be stopped")
				return false
			}
			var in map[string]string
			_ = json.NewDecoder(r.Body).Decode(&in)
			id = newMachine(st, in["name"])["id"].(string)
		case "snapshots":
			if m["state"] != "stopped" {
				writeErr(w, 409, "conflict", "machine must be stopped")
				return false
			}
			var in map[string]string
			_ = json.NewDecoder(r.Body).Decode(&in)
			sn := map[string]any{"id": fmt.Sprintf("sn_fake%d", len(st.Snapshots)+1), "name": in["name"],
				"description": in["description"], "sourceMachineId": id, "rootSizeBytes": 1 << 34,
				"dataSizeBytes": 1 << 33, "exclusiveBytes": 1 << 20, "template": m["template"],
				"createdAt": "2026-10-05T01:00:00Z"}
			st.Snapshots = append(st.Snapshots, sn)
			writeJSON(w, 201, sn)
			return true
		case "connections":
			var in map[string]any
			_ = json.NewDecoder(r.Body).Decode(&in)
			f.certs++
			writeJSON(w, 201, map[string]any{"host": "ssh.instabox.test", "port": 2222, "username": id,
				"certificate": fmt.Sprintf("ssh-ed25519-cert-v01@openssh.com AAAAfake%d", f.certs),
				"expiresAt":   time.Now().Add(time.Duration(in["ttlSeconds"].(float64)) * time.Second),
				"hostTrust":   map[string]any{"type": "ca", "publicKey": "ssh-ed25519 AAAAhostca instabox-gateway"}})
			return false
		case "exec":
			writeJSON(w, 200, map[string]any{"stdout": "", "stderr": "", "exitCode": 0})
			return false
		default:
			writeErr(w, 404, "not_found", "no such route")
			return false
		}
		writeJSON(w, 202, succeeded(id))
		return true
	})
	handle("GET /v1/operations/{id}", true, func(w http.ResponseWriter, r *http.Request, st *fakeInstabox) bool {
		writeJSON(w, 200, map[string]any{"id": r.PathValue("id"), "state": "succeeded"})
		return false
	})
	handle("GET /v1/snapshots", true, func(w http.ResponseWriter, r *http.Request, st *fakeInstabox) bool {
		writeJSON(w, 200, map[string]any{"snapshots": st.Snapshots})
		return false
	})
	handle("DELETE /v1/snapshots/{id}", true, func(w http.ResponseWriter, r *http.Request, st *fakeInstabox) bool {
		for i, sn := range st.Snapshots {
			if sn["id"] == r.PathValue("id") {
				st.Snapshots = append(st.Snapshots[:i], st.Snapshots[i+1:]...)
				w.WriteHeader(204)
				return true
			}
		}
		writeErr(w, 404, "not_found", "snapshot not found")
		return false
	})
	return mux
}
