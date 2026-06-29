# Maintainers

| Maintainer | Contact | Areas |
|---|---|---|
| Clay Townsend | clay@twn.systems | Everything — backend, GUI, releases |

Repository: https://github.com/Ptyktos/sdirstat

## Scope

sdirstat is part of TWN Systems R&D, in the same line as `qwalk` (find), `cerialize`
(serialization), and the search / data-plane tooling. It shares their conventions: **zero runtime
dependencies**, one focused binary, real-data benchmarks over synthetic ones.

## Decisions

- Small, scoped changes are merged by the maintainer after review.
- Architectural changes (new dependencies, a new frontend, changing the size semantics or the cache
  format) get a short design note in `docs/` or the MR description first.
- The **zero-dependency** rule for the core scanner is load-bearing — adding a crate to the backend
  is a maintainer decision, not a routine PR (see [CONTRIBUTING.md](CONTRIBUTING.md)).

## Adding a maintainer

Open an MR adding the person to the table above with their area, and have an existing maintainer
approve it.
