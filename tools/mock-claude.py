"""Mock Anthropic OAuth endpoints to prove the Claude fetch path end-to-end."""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

USAGE = {
    "five_hour": {"utilization": 11.0, "resets_at": "2026-09-01T16:12:00Z"},
    "seven_day": {"utilization": 2.0, "resets_at": "2026-09-08T11:00:00Z"},
    "seven_day_sonnet": {"utilization": 30.0, "resets_at": "2026-09-08T11:00:00Z"},
    "seven_day_opus": {"utilization": 64.5, "resets_at": "2026-09-08T11:00:00Z"},
    "routines": {"utilization": 5.0, "resets_at": "2026-09-08T11:00:00Z"},
    "limits": [
        {
            "kind": "weekly_scoped",
            "group": "weekly",
            "percent": 77.0,
            "is_active": False,
            "resets_at": "2026-09-08T11:00:00Z",
            "scope": {"model": {"id": "claude-opus-4", "display_name": "Opus"}},
        }
    ],
}
PROFILE = {"account": {"email_address": "dev@example.com"}, "organization": {"name": "Acme"}}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        print(
            json.dumps(
                {
                    "path": self.path,
                    "authorization": self.headers.get("Authorization"),
                    "anthropic-beta": self.headers.get("anthropic-beta"),
                    "accept": self.headers.get("Accept"),
                    "user-agent": self.headers.get("User-Agent"),
                }
            ),
            file=sys.stderr,
            flush=True,
        )
        if self.path == "/api/oauth/usage":
            body = json.dumps(USAGE).encode()
        elif self.path == "/api/oauth/profile":
            body = json.dumps(PROFILE).encode()
        else:
            self.send_response(404)
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", 8787), Handler).serve_forever()
