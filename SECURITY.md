# Security

winapfs installs a Windows service and registers a WinFsp launcher entry, both of which run as SYSTEM. The design and its limits are described in [docs/HOW-IT-WORKS.md](docs/HOW-IT-WORKS.md#security-model).

If you find a way to use winapfs to gain rights you should not have, or to write to a drive, please report it privately through GitHub's **Report a vulnerability** button on the Security tab rather than in a public issue.
