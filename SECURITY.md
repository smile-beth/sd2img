# Security Policy

## Scope

Security reports are welcome for the current development version. No long-term support or guaranteed response time is currently offered.

## Reporting a vulnerability

If the hosting repository provides private vulnerability reporting, use its **Security > Advisories > Report a vulnerability** feature. Otherwise, open an issue asking for a private reporting channel without including exploit details or sensitive data. Wait for a maintainer to provide that channel before sharing details.

Include the affected revision, Windows version, reproduction steps using synthetic data, potential impact, and any suggested mitigation. Do not submit source-disk images, complete rescue maps or logs, credentials, or recovered personal data.

## Sensitive artifacts

Disk images may contain deleted files and filesystem metadata. Rescue maps contain a complete reference sector in hexadecimal. Recovery logs can also expose private paths and device details. Treat images, maps, and logs as private recovery artifacts, even if the filenames look harmless.

The program requests administrator privileges and uses native Windows APIs. Reports involving unintended writes, unsafe path handling, memory safety, or incorrect device matching are especially relevant.
