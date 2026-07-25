# Security policy

## Supported versions

Security fixes are provided for the latest published version of Yo.

## Reporting a vulnerability

Do not open a public issue for a suspected credential leak, sandbox escape, command-execution bypass, update-verification flaw, or memory privacy issue. Use GitHub's private vulnerability reporting for this repository. Include the affected version, operating system, reproduction steps, and impact.

You should receive an acknowledgement within seven days. A fix, disclosure timeline, and release plan will be coordinated privately after the report is reproduced.

## Security boundaries

Yo treats model output as untrusted. Approval policy and OS sandboxing are separate controls. The supported OS sandbox backends are Seatbelt on macOS and Bubblewrap on Linux; required sandbox mode fails closed when no supported backend exists.

Diagnostics are opt-in and local-only. They must never contain credentials, prompt or memory content, command text or output, or user filesystem paths.
