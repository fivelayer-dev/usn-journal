# Changelog

All notable changes are recorded here. This project follows
[Semantic Versioning](https://semver.org/).

## Unreleased

- First version: raw journal records, master file table listings, path resolution,
  merged per-file changes on demand or live, journal health, and the `usn-watch`
  command-line program.
- `FileChange::parent_ids` lists every directory a change's records named: the one the
  file is in, each one it was moved out of, and the directory of each hard link added or
  removed. `usn-watch` writes it as `parent_ids`.
- `PathFilter::is_excluded` is public, so that a listing made another way can leave out
  the same paths.
