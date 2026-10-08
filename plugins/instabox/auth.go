package main

import (
	"crypto/rand"
	"crypto/sha256"
	"crypto/subtle"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"html"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strconv"
	"time"
)

// The plugin has its own instabox sign-in, kept in its state directory. It is
// independent of the instabox CLI's.

// Credentials is credentials.json.
type Credentials struct {
	Server           string    `json:"server"`
	AccessToken      string    `json:"accessToken"`
	AccessExpiresAt  time.Time `json:"accessExpiresAt"`
	RefreshToken     string    `json:"refreshToken"`
	RefreshExpiresAt time.Time `json:"refreshExpiresAt"`
}

func credentialsPath() string { return filepath.Join(stateDir(), "credentials.json") }

// loadCredentials returns the stored sign-in, or nil when there is none.
func loadCredentials() (*Credentials, error) {
	data, err := os.ReadFile(credentialsPath())
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	var c Credentials
	if err := json.Unmarshal(data, &c); err != nil {
		return nil, fmt.Errorf("%s: %w", credentialsPath(), err)
	}
	return &c, nil
}

func saveCredentials(c *Credentials) error {
	data, err := json.MarshalIndent(c, "", "  ")
	if err != nil {
		return err
	}
	return writeFileAtomic(credentialsPath(), data, 0o600)
}

func writeFileAtomic(path string, data []byte, perm os.FileMode) error {
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		return err
	}
	f, err := os.CreateTemp(filepath.Dir(path), "."+filepath.Base(path)+".tmp-*")
	if err != nil {
		return err
	}
	tmp := f.Name()
	defer os.Remove(tmp)
	if err := f.Chmod(perm); err != nil {
		f.Close()
		return err
	}
	if _, err := f.Write(data); err != nil {
		f.Close()
		return err
	}
	if err := f.Close(); err != nil {
		return err
	}
	return os.Rename(tmp, path)
}

// errSignedOut means nobody is signed in, or the sign-in can no longer be
// refreshed. Any other error (such as an unreachable server) is not that.
var errSignedOut = errors.New("not signed in to instabox")

// refreshSkew refreshes an access token this long before it expires.
const refreshSkew = 30 * time.Second

// accessToken returns a usable access token, refreshing it when it is about
// to expire or when force is set (the server rejected it). Refreshes are
// serialized across processes, so a rotated refresh token is used only once.
func accessToken(force bool) (string, error) {
	c, err := loadCredentials()
	if err != nil {
		return "", err
	}
	if c == nil || c.AccessToken == "" {
		return "", errSignedOut
	}
	if !force && (c.AccessExpiresAt.IsZero() || time.Now().Add(refreshSkew).Before(c.AccessExpiresAt)) {
		return c.AccessToken, nil
	}
	stale := c.AccessToken
	var tok string
	err = withLock("auth.lock", func() error {
		c, err := loadCredentials()
		if err != nil {
			return err
		}
		if c == nil || c.AccessToken == "" {
			return errSignedOut
		}
		fresh := c.AccessExpiresAt.IsZero() || time.Now().Add(refreshSkew).Before(c.AccessExpiresAt)
		if c.AccessToken != stale && fresh {
			tok = c.AccessToken // another process refreshed it
			return nil
		}
		if c.RefreshToken == "" || (!c.RefreshExpiresAt.IsZero() && time.Now().After(c.RefreshExpiresAt)) {
			return errSignedOut
		}
		var nt Credentials
		err = call(request{method: http.MethodPost, path: "/v1/auth/refresh", server: c.Server,
			in: map[string]string{"refreshToken": c.RefreshToken}, out: &nt})
		var ae *apiError
		if errors.As(err, &ae) && (ae.Status == http.StatusUnauthorized || ae.Status == http.StatusBadRequest) {
			return errSignedOut
		}
		if err != nil {
			return err
		}
		nt.Server = c.Server
		if err := saveCredentials(&nt); err != nil {
			return err
		}
		tok = nt.AccessToken
		return nil
	})
	return tok, err
}

// whoami checks the stored sign-in with the server.
func whoami() Account {
	c, err := loadCredentials()
	if err != nil {
		return Account{Err: err, Server: serverURL()}
	}
	if c == nil {
		return Account{SignedOut: true, Server: serverURL()}
	}
	var me struct {
		UserID int64  `json:"userId"`
		Login  string `json:"login"`
	}
	err = call(request{method: http.MethodGet, path: "/v1/me", out: &me, auth: true})
	switch {
	case errors.Is(err, errSignedOut) || errorCode(err) == "unauthenticated":
		return Account{SignedOut: true, Server: c.Server}
	case err != nil:
		return Account{Err: err, Server: c.Server}
	}
	return Account{Login: me.Login, UserID: strconv.FormatInt(me.UserID, 10), Server: c.Server}
}

// signOut revokes the sign-in on the server (best effort) and forgets it.
func signOut() error {
	if c, _ := loadCredentials(); c != nil {
		_ = call(request{method: http.MethodPost, path: "/v1/auth/logout", server: c.Server, auth: true})
	}
	err := os.Remove(credentialsPath())
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	return err
}

// login is the `herdr-instabox login` command: it runs in the terminal the
// settings pane hands over, signs in with the browser (or a device code when
// no browser can be used) and stores the result.
func login(provider string) error {
	if provider == "" {
		provider = "github"
	}
	server := serverURL()
	var tok *Credentials
	reason := browserUnavailable()
	if reason == "" {
		var err error
		tok, err = browserLogin(server, provider)
		var fb fallbackError
		switch {
		case errors.As(err, &fb):
			reason = fb.reason
		case err != nil:
			return err
		}
	}
	if tok == nil {
		if provider == "google" {
			return fmt.Errorf("Google sign-in needs a browser on this computer (%s); sign in with GitHub instead", reason)
		}
		fmt.Printf("Using a device code to sign in: %s.\n", reason)
		var err error
		if tok, err = deviceLogin(server); err != nil {
			return err
		}
	}
	tok.Server = server
	if err := saveCredentials(tok); err != nil {
		return err
	}
	acct := whoami()
	if !acct.SignedIn() {
		return fmt.Errorf("signed in, but the account could not be checked: %v", acct.Err)
	}
	fmt.Printf("Signed in to %s as %s.\n", server, acct.Login)
	return nil
}

// fallbackError makes login switch to the device code sign-in.
type fallbackError struct{ reason string }

func (e fallbackError) Error() string { return e.reason }

func browserUnavailable() string {
	display := os.Getenv("DISPLAY") != "" || os.Getenv("WAYLAND_DISPLAY") != ""
	switch {
	case os.Getenv("SSH_CONNECTION") != "" && !display:
		return "this is an SSH session without a display"
	case runtime.GOOS != "darwin" && !display:
		return "no display"
	}
	return ""
}

// openBrowser opens u in the default browser; tests replace it.
var openBrowser = func(u string) error {
	name := "xdg-open"
	if runtime.GOOS == "darwin" {
		name = "open"
	}
	return exec.Command(name, u).Start()
}

func randomURLSafe(n int) string {
	b := make([]byte, n)
	_, _ = rand.Read(b)
	return base64.RawURLEncoding.EncodeToString(b)
}

// browserLogin is the loopback sign-in (RFC 8252 with PKCE): listen on
// 127.0.0.1, open the server's sign-in page, receive the code at /callback and
// exchange it. Problems before the browser opens return a fallbackError.
func browserLogin(server, provider string) (*Credentials, error) {
	probe, err := httpClient.Get(server + "/auth/cli/start")
	if err != nil {
		return nil, fmt.Errorf("cannot reach %s: %w", server, unwrapURLError(err))
	}
	probe.Body.Close()
	if probe.StatusCode != http.StatusBadRequest {
		return nil, fallbackError{fmt.Sprintf("the server does not offer browser sign-in (HTTP %d)", probe.StatusCode)}
	}
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return nil, fallbackError{"cannot listen on 127.0.0.1: " + err.Error()}
	}
	defer ln.Close()
	redirect := fmt.Sprintf("http://127.0.0.1:%d/callback", ln.Addr().(*net.TCPAddr).Port)
	verifier, state := randomURLSafe(32), randomURLSafe(24)
	sum := sha256.Sum256([]byte(verifier))
	q := url.Values{"redirect_uri": {redirect}, "state": {state}, "provider": {provider},
		"code_challenge": {base64.RawURLEncoding.EncodeToString(sum[:])}, "code_challenge_method": {"S256"}}
	start := server + "/auth/cli/start?" + q.Encode()

	type result struct {
		tok *Credentials
		err error
	}
	done := make(chan result, 1)
	mux := http.NewServeMux()
	mux.HandleFunc("/callback", func(w http.ResponseWriter, r *http.Request) {
		cq := r.URL.Query()
		if subtle.ConstantTimeCompare([]byte(cq.Get("state")), []byte(state)) != 1 {
			loginPage(w, "Sign-in failed", "This sign-in does not match. Start again from herdr.")
			return
		}
		var res result
		switch {
		case cq.Get("error") != "":
			msg := cq.Get("error")
			if d := cq.Get("error_description"); d != "" {
				msg = d
			}
			res.err = fmt.Errorf("sign-in failed: %s", msg)
		case cq.Get("code") == "":
			res.err = errors.New("sign-in failed: no code returned")
		default:
			var tok Credentials
			res.err = call(request{method: http.MethodPost, path: "/v1/auth/cli/token", server: server,
				in:  map[string]string{"code": cq.Get("code"), "codeVerifier": verifier, "redirectUri": redirect},
				out: &tok})
			res.tok = &tok
		}
		if res.err != nil {
			loginPage(w, "Sign-in failed", res.err.Error()+". Return to herdr.")
		} else {
			loginPage(w, "Signed in to instabox", "You can close this tab and return to herdr.")
		}
		select {
		case done <- res:
		default:
		}
	})
	srv := &http.Server{Handler: mux, ReadHeaderTimeout: 10 * time.Second}
	go func() { _ = srv.Serve(ln) }()
	defer srv.Close()

	if err := openBrowser(start); err != nil {
		return nil, fallbackError{"cannot open a browser: " + err.Error()}
	}
	fmt.Printf("Opened the browser to sign in. If it did not open, visit:\n\n  %s\n\nWaiting… (Ctrl-C to cancel)\n", start)
	select {
	case <-time.After(5 * time.Minute):
		return nil, errors.New("timed out waiting for the browser sign-in")
	case res := <-done:
		return res.tok, res.err
	}
}

func loginPage(w http.ResponseWriter, title, msg string) {
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	w.Header().Set("Cache-Control", "no-store")
	fmt.Fprintf(w, `<!doctype html><meta charset="utf-8"><title>%s</title>`+
		`<body style="font:16px system-ui;max-width:32rem;margin:15vh auto;padding:0 1rem">`+
		`<h1>%s</h1><p>%s</p>`, html.EscapeString(title), html.EscapeString(title), html.EscapeString(msg))
}

// deviceLogin shows a code to enter at the server's verification page and
// polls until the user approves it.
func deviceLogin(server string) (*Credentials, error) {
	var ds struct {
		DeviceCode      string `json:"deviceCode"`
		UserCode        string `json:"userCode"`
		VerificationURI string `json:"verificationUri"`
		Interval        int    `json:"interval"`
		ExpiresIn       int    `json:"expiresIn"`
	}
	if err := call(request{method: http.MethodPost, path: "/v1/auth/device", server: server, in: struct{}{}, out: &ds}); err != nil {
		return nil, err
	}
	fmt.Printf("To sign in, open\n\n  %s\n\nand enter the code: %s\n\nWaiting… (Ctrl-C to cancel)\n", ds.VerificationURI, ds.UserCode)
	interval := time.Duration(max(ds.Interval, 1)) * time.Second
	expires := ds.ExpiresIn
	if expires <= 0 {
		expires = 900
	}
	deadline := time.Now().Add(time.Duration(expires) * time.Second)
	for time.Now().Before(deadline) {
		time.Sleep(interval)
		var tok Credentials
		err := call(request{method: http.MethodPost, path: "/v1/auth/device/token", server: server,
			in: map[string]string{"deviceCode": ds.DeviceCode}, out: &tok})
		switch errorCode(err) {
		case "":
			if err != nil {
				return nil, err
			}
			return &tok, nil
		case "authorization_pending":
		case "slow_down":
			interval += 5 * time.Second
		case "access_denied":
			return nil, errors.New("sign-in was denied")
		case "expired_token":
			return nil, errors.New("the code expired; sign in again")
		default:
			return nil, err
		}
	}
	return nil, errors.New("the code expired; sign in again")
}
