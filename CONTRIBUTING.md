# Contributing to Truss

Contributions are welcome through GitHub issues and pull requests.

## Development setup

Install a current stable Rust toolchain with `rustfmt` and `clippy`, then clone
the repository and fetch the locked dependency set:

```sh
git clone https://github.com/ControlStackAI/truss.git
cd truss
cargo fetch --locked
```

The bundled SQLite build also requires a working C/C++ toolchain.

## Before opening a pull request

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Keep changes focused. Add a regression test for runtime behavior changes, update
the relevant documentation, and do not commit credentials, signing keys, run
databases, or generated build output.

By submitting a contribution, you agree that it is licensed under the project's
[MIT License](LICENSE).
