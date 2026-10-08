package main

import (
	"crypto/ed25519"
	"crypto/rand"
	"encoding/json"
	"encoding/pem"
	"errors"
	"fmt"
	"net/http"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"golang.org/x/crypto/ssh"
)

// The plugin connects with its own SSH key and short-lived certificates the
// API issues for it. All of it lives next to the Host blocks, so signing out
// removes it with them.

// certTTL is how long a certificate is valid. The gateway checks it only when
// a connection opens; an open connection outlives it.
const certTTL = 24 * time.Hour

func keyPath() string           { return filepath.Join(hostsDir(), "id_ed25519") }
func knownHostsPath() string    { return filepath.Join(hostsDir(), "known_hosts") }
func certPath(id string) string { return filepath.Join(hostsDir(), id+"-cert.pub") }
func certMetaPath(id string) string {
	return filepath.Join(hostsDir(), id+"-cert.json")
}

// ensureKey creates the plugin's SSH key once and returns its public key in
// authorized_keys format.
func ensureKey() (string, error) {
	if data, err := os.ReadFile(keyPath()); err == nil {
		signer, err := ssh.ParsePrivateKey(data)
		if err == nil {
			return authorizedKey(signer.PublicKey()), nil
		}
	}
	pub, priv, err := ed25519.GenerateKey(rand.Reader)
	if err != nil {
		return "", err
	}
	block, err := ssh.MarshalPrivateKey(priv, "herdr-instabox")
	if err != nil {
		return "", err
	}
	if err := os.MkdirAll(hostsDir(), 0o700); err != nil {
		return "", err
	}
	if err := writeFileAtomic(keyPath(), pemEncode(block.Type, block.Bytes), 0o600); err != nil {
		return "", err
	}
	sshPub, err := ssh.NewPublicKey(pub)
	if err != nil {
		return "", err
	}
	return authorizedKey(sshPub), nil
}

func authorizedKey(k ssh.PublicKey) string {
	return strings.TrimSpace(string(ssh.MarshalAuthorizedKey(k)))
}

// certMeta records an issued certificate.
type certMeta struct {
	Host      string    `json:"host"`
	Port      int       `json:"port"`
	Username  string    `json:"username"`
	ExpiresAt time.Time `json:"expiresAt"`
	PublicKey string    `json:"publicKey"` // the key it was issued for
}

// ensureCert returns the machine's certificate, issuing a new one unless the
// stored one was issued for the current key and is far enough from expiry.
// force always issues a new one.
func ensureCert(id string, force bool) (certMeta, error) {
	pub, err := ensureKey()
	if err != nil {
		return certMeta{}, fmt.Errorf("ssh key: %w", err)
	}
	if !force {
		var m certMeta
		data, err := os.ReadFile(certMetaPath(id))
		if err == nil && json.Unmarshal(data, &m) == nil && m.PublicKey == pub &&
			time.Until(m.ExpiresAt) > certMargin {
			if _, err := os.Stat(certPath(id)); err == nil {
				return m, nil
			}
		}
	}
	var conn struct {
		Host        string    `json:"host"`
		Port        int       `json:"port"`
		Username    string    `json:"username"`
		Certificate string    `json:"certificate"`
		ExpiresAt   time.Time `json:"expiresAt"`
		HostTrust   struct {
			PublicKey string `json:"publicKey"`
		} `json:"hostTrust"`
	}
	err = call(request{method: http.MethodPost, path: machinePath(id, "/connections"), auth: true, mutate: true,
		in: map[string]any{"publicKey": pub, "ttlSeconds": int(certTTL / time.Second)}, out: &conn})
	if err != nil {
		return certMeta{}, err
	}
	if conn.Certificate == "" || conn.Host == "" || conn.HostTrust.PublicKey == "" {
		return certMeta{}, errors.New("the server returned an incomplete connection")
	}
	m := certMeta{Host: conn.Host, Port: conn.Port, Username: conn.Username, ExpiresAt: conn.ExpiresAt, PublicKey: pub}
	if m.Username == "" {
		m.Username = id
	}
	if err := writeFileAtomic(certPath(id), []byte(strings.TrimSpace(conn.Certificate)+"\n"), 0o600); err != nil {
		return certMeta{}, err
	}
	data, _ := json.MarshalIndent(m, "", "  ")
	if err := writeFileAtomic(certMetaPath(id), data, 0o600); err != nil {
		return certMeta{}, err
	}
	return m, trustHost(conn.Host, conn.Port, conn.HostTrust.PublicKey)
}

// trustHost records the gateway's host CA for host:port in the plugin's
// known_hosts, replacing an earlier line for it. Updates are serialized, since
// ssh may renew certificates for several machines at once.
func trustHost(host string, port int, caKey string) error {
	pattern := host
	if port != 0 && port != 22 {
		pattern = "[" + host + "]:" + strconv.Itoa(port)
	}
	f := strings.Fields(caKey)
	if len(f) < 2 {
		return errors.New("the server returned an invalid host key")
	}
	want := "@cert-authority " + pattern + " " + f[0] + " " + f[1]
	return withLock("known_hosts.lock", func() error {
		var lines []string
		if data, err := os.ReadFile(knownHostsPath()); err == nil {
			for _, line := range strings.Split(string(data), "\n") {
				lf := strings.Fields(line)
				if line == want {
					return nil
				}
				if len(lf) == 0 || (len(lf) >= 2 && lf[0] == "@cert-authority" && lf[1] == pattern) {
					continue
				}
				lines = append(lines, line)
			}
		}
		lines = append(lines, want)
		return writeFileAtomic(knownHostsPath(), []byte(strings.Join(lines, "\n")+"\n"), 0o600)
	})
}

func pemEncode(typ string, b []byte) []byte {
	return pem.EncodeToMemory(&pem.Block{Type: typ, Bytes: b})
}
