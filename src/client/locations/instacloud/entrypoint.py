"""A persistent Herdr session plus an HTTP health endpoint for compute readiness."""
import http.server
import os
from pathlib import Path
import signal
import subprocess
import sys

if not os.path.ismount('/data'):
    raise SystemExit('Herdr requires a persistent volume mounted at /data')
for directory in ('home', 'config', 'state', 'workspace'):
    Path('/data', directory).mkdir(exist_ok=True)
os.chdir('/data/workspace')
config = Path('/data/config/herdr/config.toml')
config.parent.mkdir(parents=True, exist_ok=True)
if not config.exists():
    config.write_text('onboarding = false\n[terminal]\nnew_cwd = "/data/workspace"\n'
                      '[update]\nversion_check = false\nmanifest_check = false\n')
subprocess.run(['herdr', '--session', 'herdr-remote', 'remote-client-bridge'],
               stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, check=True)


def shutdown(_signum, _frame):
    try:
        subprocess.run(['herdr', 'session', 'stop', 'herdr-remote'], timeout=10,
                       stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL)
    finally:
        sys.exit(0)


signal.signal(signal.SIGTERM, shutdown)
signal.signal(signal.SIGINT, shutdown)


class Health(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200 if self.path in ('/', '/healthz') else 404)
        self.send_header('Content-Type', 'text/plain')
        self.end_headers()
        self.wfile.write(b'herdr remote\n')

    def log_message(self, *_args):
        pass


http.server.HTTPServer(('0.0.0.0', 8080), Health).serve_forever()
