# Contributing

## Set up

You need Windows and a recent stable Rust. Either the MSVC or the GNU toolchain works,
and the GNU one needs nothing else installed.

```powershell
winget install Rustlang.Rustup
git clone https://github.com/fivelayer-dev/usn-journal
cd usn-journal
cargo test
```

The library's logic is portable, so its unit tests also run on Linux and macOS with
`cargo test -p usn-journal --lib`. Only the parts that touch a real volume need Windows.

## Run the tests

| Command | Needs | What it covers |
|---|---|---|
| `cargo test -p usn-journal --lib` | Nothing | Parsing and change logic, using a fake journal |
| `cargo test` | A journal on the temp drive | The above, plus real journal records with names removed, and the command-line program |
| `cargo test --workspace -- --ignored --skip shadow_copy --skip disposable_volume` | An elevated terminal | Everything that needs real paths and full listings |

Tests that can't run in your environment are marked `#[ignore]` and show as ignored,
so a passing run never includes a test that silently did nothing. An elevated test run
without elevation fails, rather than passing.

The tests only read the journal. Their only writes are small files in a new temp
directory, which they remove afterwards.

Two groups need something extra, so only the CI workflow runs them:

- `shadow_copy` tests need a volume shadow copy, named in `USN_TEST_SHADOW_COPY`.
- `disposable_volume` tests create and delete a journal, so they need a volume that can
  safely lose one, named in `USN_TEST_DISPOSABLE_VOLUME`.

## Run the tests on GitHub instead

You don't need to run anything elevated on your own PC. GitHub's Windows runners are
disposable machines that already run as administrator, and
[the CI workflow](.github/workflows/ci.yml) runs the full suite there. To use it on your
fork, open the **Actions** tab and enable workflows. It runs on every pull request.

## Measure scan speed

`examples/scan_bench.rs` times the operations that grow with the size of a volume. It
only reads the volume, but it needs an elevated terminal:

```powershell
cargo run --release --example scan_bench -- C: 3
```

## Before opening a pull request

```powershell
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

Add a test with any bug fix, and let the CI workflow run the elevated tests.

## Scope

This crate reads the journal and says what changed. Reading files, storing results,
scheduling and creating shadow copies belong to the program that uses it.

The default build must stay read-only. Anything that changes a volume goes behind the
`journal-admin` feature.

The public types use `#[non_exhaustive]` and builders so that fields and options can be
added without a breaking change. `ChangeKind` and `WatchEvent` are deliberately exhaustive,
so that a `match` on them stays complete.
