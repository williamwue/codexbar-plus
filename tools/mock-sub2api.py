"""Local sub2api-compatible endpoint for manual CLI verification of the plugin host."""
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PAYLOAD = {
    "mode": "subscription",
    "isValid": True,
    "planName": "Pro plan",
    "balance": 12.5,
    "unit": "USD",
    "subscription": {
        "daily_usage_usd": 2.5,
        "daily_limit_usd": 10,
        "weekly_usage_usd": 20,
        "weekly_limit_usd": 100,
        "monthly_usage_usd": 60,
        "monthly_limit_usd": 400,
        "expires_at": "2026-12-31T00:00:00Z",
    },
    "rate_limits": [
        {"window": "5h", "limit": 100, "used": 25, "remaining": 75, "reset_at": "2026-09-02T04:00:00Z"},
        {"window": "7d", "limit": 1000, "used": 900, "remaining": 100, "reset_at": "2026-09-08T00:00:00Z"},
    ],
    "usage": {
        "today": {"requests": 12, "total_tokens": 34567, "actual_cost": 1.25},
        "total": {"requests": 4321, "total_tokens": 9876543, "actual_cost": 250.5},
    },
}


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        print(
            json.dumps({"path": self.path, "authorization": self.headers.get("Authorization")}),
            file=sys.stderr,
            flush=True,
        )
        body = json.dumps(PAYLOAD).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", 8899), Handler).serve_forever()
