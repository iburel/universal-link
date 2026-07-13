# Contributing to UniversalLink

Thanks for your interest in UniversalLink. This document explains how to
build, test, and submit changes.

## Building and testing

See the [README](README.md) for prerequisites and the exact build and test
commands. In short:

```sh
# Frontend
cd gui/ui && npm ci && npm run check && npm test && cd ../..

# Rust workspace (capped parallelism: some tests rely on timing windows)
cargo build --workspace --lib --bins --locked
cargo test --workspace --locked -- --test-threads=2
```

Please make sure the whole workspace builds and all tests pass before opening
a pull request.

## Coding conventions

- Rust is formatted with `cargo fmt` and must be clippy-clean
  (`cargo clippy --workspace --all-targets`).
- The codebase is written and commented in **English**. Comments explain the
  *why*, not the *what*; match the density and tone of the surrounding code.
- Keep changes focused and self-contained.

## Developer Certificate of Origin (DCO)

Every commit must be **signed off** to certify that you wrote the change (or
otherwise have the right to submit it under the project's license). This is the
same mechanism the Linux kernel uses. Add a sign-off line to each commit:

```
Signed-off-by: Your Name <your.email@example.com>
```

`git commit -s` adds it automatically. By signing off you agree to the
Developer Certificate of Origin 1.1, reproduced below.

> **Note on copyright and licensing.** UniversalLink is licensed under
> **AGPL-3.0-only**. The DCO certifies the *provenance* of a contribution; it
> does **not** transfer copyright. Contributors retain copyright to their work
> and license it to the project under AGPL-3.0-only. Relicensing the project as
> a whole would therefore require the agreement of all copyright holders. If a
> future need for unilateral relicensing or dual-licensing arises, a separate
> Contributor License Agreement (CLA) would be introduced at that point.

```
Developer Certificate of Origin
Version 1.1

Copyright (C) 2004, 2006 The Linux Foundation and its contributors.

Everyone is permitted to copy and distribute verbatim copies of this
license document, but changing it is not allowed.


Developer's Certificate of Origin 1.1

By making a contribution to this project, I certify that:

(a) The contribution was created in whole or in part by me and I
    have the right to submit it under the open source license
    indicated in the file; or

(b) The contribution is based upon previous work that, to the best
    of my knowledge, is covered under an appropriate open source
    license and I have the right under that license to submit that
    work with modifications, whether created in whole or in part
    by me, under the same open source license (unless I am
    permitted to submit under a different license), as indicated
    in the file; or

(c) The contribution was provided directly to me by some other
    person who certified (a), (b) or (c) and I have not modified
    it.

(d) I understand and agree that this project and the contribution
    are public and that a record of the contribution (including all
    personal information I submit with it, including my sign-off) is
    maintained indefinitely and may be redistributed consistent with
    this project or the open source license(s) involved.
```

## Reporting security issues

Please do **not** open a public issue for security vulnerabilities. See
[SECURITY.md](SECURITY.md) for how to report them privately.
