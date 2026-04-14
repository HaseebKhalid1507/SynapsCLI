# Security Policy

## Supported Versions

| Version | Supported |
|---------|-----------|
| main (latest) | ✅ |

## Reporting a Vulnerability

**Do not open a public issue for security vulnerabilities.**

Instead, email **haseebkhalid1507@gmail.com** with:

- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if any)

You will receive a response within 48 hours. We take security seriously — SynapsCLI runs autonomous agents with tool access, so the attack surface matters.

## Security Model

### Agent Isolation
- Watcher agents run as child processes, not in the supervisor's address space
- Agent names are validated (`^[a-zA-Z0-9_-]+$`) to prevent path traversal
- IPC socket permissions are set to `0600` (owner-only)

### File Safety
- Critical files (handoff.json, stats.json) use atomic writes (tmp + rename)
- Stats file access uses file locking (`flock`) to prevent race conditions
- Agent configs are read-only to agent processes

### Cost Protection
- Per-session cost limits (default: $0.50)
- Daily cost limits per agent (default: $10.00)
- Token and tool call limits with automatic session termination
- Cost-limited agents exit with code 2, supervisor won't restart

### Known Limitations
- Agents can execute arbitrary bash commands — this is by design, not a bug
- No network isolation between agents
- No sandboxed filesystem access (planned)
- Tool permission system (should_confirm) is on the roadmap

## Scope

The following are in scope for security reports:
- Privilege escalation between agents or to the supervisor
- Path traversal or file access outside intended directories
- IPC protocol vulnerabilities
- Cost limit bypass
- Denial of service against the supervisor daemon
- Information disclosure between agents

The following are **not** in scope:
- Agents executing bash commands (intended functionality)
- LLM prompt injection (application-layer concern, not runtime)
- Vulnerabilities in upstream dependencies (report to them directly)