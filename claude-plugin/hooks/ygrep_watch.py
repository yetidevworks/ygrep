#!/usr/bin/env python3
"""
SessionStart hook for ygrep.
Starts the ygrep daemon/watcher for the current workspace.
"""

import json
import os
import subprocess
import sys

def main():
    # Read hook input from stdin
    try:
        hook_input = json.load(sys.stdin)
    except json.JSONDecodeError:
        hook_input = {}

    cwd = hook_input.get("cwd", os.getcwd())
    session_id = hook_input.get("session_id", "unknown")

    # Log file for debugging
    log_file = os.environ.get("YGREP_WATCH_LOG", "/tmp/ygrep-watch.log")

    try:
        # Check if ygrep is installed
        result = subprocess.run(
            ["which", "ygrep"],
            capture_output=True,
            text=True
        )

        if result.returncode != 0:
            # ygrep not found, skip
            response = {
                "result": "continue",
                "additionalContextForSession": "ygrep not installed. Install with: cargo install --path /path/to/ygrep"
            }
            print(json.dumps(response))
            return

        # Index the workspace if needed (this is fast if already indexed)
        subprocess.run(
            ["ygrep", "index"],
            cwd=cwd,
            capture_output=True,
            timeout=60
        )

        # Return success with skill loading instruction
        response = {
            "result": "continue",
            "additionalContextForSession": "You must load ygrep skill for searching and exploring rather than grep"
        }
        print(json.dumps(response))

    except Exception as e:
        with open(log_file, "a") as f:
            f.write(f"Error: {e}\n")

        response = {
            "result": "continue",
            "additionalContextForSession": f"ygrep watch failed: {e}"
        }
        print(json.dumps(response))

if __name__ == "__main__":
    main()
