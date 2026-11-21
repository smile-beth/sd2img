# Contributing

Bug reports, documentation improvements, and focused pull requests are welcome.

## Development setup

Use Windows with a current stable Rust toolchain and Windows native build tools. Runtime disk discovery and elevation use native Windows APIs without a shell subprocess. Build with `cargo build --locked`. The source uses Windows APIs and does not compile for non-Windows targets.

Before submitting a pull request, run:

```powershell
cargo fmt --all -- --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
python -m unittest discover -s .github/scripts -p "test_*.py" -v
```

Release-tool tests require Python 3.11 or newer; CI uses Python 3.14. They use temporary Git repositories and validate tag versions and main-branch ancestry without publishing to GitHub. The Rust tests use temporary files, synthetic data, and named pipes. Reset ordering is tested without resetting hardware. Hardware testing is optional; use expendable media and a separate destination disk. Do not make automated tests depend on administrator privileges or physical disks.

## Reporting bugs

Include the application version or revision and the `build=` identifier from the session log, Windows version, Rust version if relevant, reader/connection type, sanitized command, expected behavior, actual behavior, and relevant error text. Explain whether the issue occurs during initial scanning, resume, or reconnection. Distinguish a request that is still pending from one that completed with an error; failure of a multi-sector request does not identify a permanently bad individual sector.

Never attach real disk images, complete map files or recovery logs, credentials, or private recovered data. A map contains a reference sector copied from the source. Prefer a minimal reproduction with synthetic data. Follow [SECURITY.md](SECURITY.md) for vulnerabilities.

## Pull requests

- Keep each change focused and explain its purpose and validation.
- Update both [English](README.md) and [Japanese](README.ja.md) READMEs when changing CLI behavior, display statistics, scheduling, or recovery semantics.
- Preserve compatibility with existing map files or document and test migration behavior.
- Explain ownership, lifetime, alignment, and pending-I/O assumptions when changing unsafe Windows code.
- Add meaningful tests for behavior changes, using synthetic data wherever possible.

By submitting a contribution, you agree that it may be distributed under this project's [MIT License](LICENSE).

## Community expectations

Be respectful, provide constructive feedback, and protect other people's private data. Harassment and disclosure of personal information are not welcome.
