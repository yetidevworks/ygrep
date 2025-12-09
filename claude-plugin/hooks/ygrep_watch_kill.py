#!/usr/bin/env python3
"""
SessionEnd hook for ygrep.
Cleans up any ygrep daemon processes if needed.
"""

import json
import sys

def main():
    # Read hook input from stdin
    try:
        hook_input = json.load(sys.stdin)
    except json.JSONDecodeError:
        hook_input = {}

    # Currently ygrep doesn't run a persistent daemon per-session
    # (it uses auto-start with idle timeout instead)
    # This hook is here for future use if we need per-session cleanup

    response = {
        "result": "continue"
    }
    print(json.dumps(response))

if __name__ == "__main__":
    main()
