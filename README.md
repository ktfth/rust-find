# rust-find

`rust-find` is a faithful Rust port of FreeBSD's `find(1)`, modeled directly on the original `usr.bin/find/` utility and its expression grammar.

The goal is not to imitate GNU `find` loosely. The goal is to preserve the FreeBSD command shape, predicate vocabulary, default behavior, and operator semantics for the feature set implemented in this repository.

## Why this project exists

FreeBSD's `find(1)` has a clean, well-defined interface:

- one or more start paths come first
- the predicate expression comes after the paths
- adjacent predicates are an implicit logical AND
- `-o`, `!`, and parentheses behave like the classic BSD utility

This project keeps that structure intact while moving the implementation to Rust.

## Current scope

The current binary implements the core traversal and expression engine, including:

- global flags: `-H`, `-L`, `-P`, `-d`, `-x`
- name and path predicates: `-name`, `-iname`, `-path`, `-ipath`, `-wholename`, `-iwholename`
- type predicates: `-type f`, `d`, `l`, `b`, `c`, `p`, `s`
- metadata predicates: `-empty`, `-maxdepth`, `-mindepth`, `-size`, `-mtime`, `-newer`
- actions: `-print`, `-print0`, `-delete`, `-prune`
- boolean operators: `!`, `-not`, `-a`, `-and`, `-o`, `-or`, `(`

## Faithfulness to FreeBSD `find(1)`

This repository is intentionally framed as a faithful port of FreeBSD's `find(1)`:

- the implementation is derived from the FreeBSD utility's model and syntax
- the CLI expects the BSD ordering `find path ... expression`
- the expression parser preserves implicit AND, explicit OR, NOT, and grouping
- predicates and flags use the same vocabulary as the FreeBSD tool where they are implemented

Faithful does not mean feature-complete yet. It means the project is anchored to the FreeBSD utility rather than inventing a new interface. The README and man pages document the current compatibility boundaries explicitly.

## Build

```powershell
cargo build
```

Run the binary through Cargo:

```powershell
cargo run -- . -iname "main.rs"
```

Build an optimized binary:

```powershell
cargo build --release
```

## Usage

```text
rust-find [-H | -L | -P] [-d] [-x] path ... [expression]
```

Important: at least one start path is required before the expression.

These examples are valid:

```powershell
cargo run -- . -iname "main.rs"
cargo run -- src -type f -name "*.rs"
cargo run -- . -maxdepth 2 -mtime -7
cargo run -- . "(" -name "*.rs" -o -name "*.toml" ")"
```

These examples are not valid:

```powershell
cargo run -- -iname "main.rs"
```

That fails because no start path was provided.

```powershell
cargo run -- main.rs
```

That treats `main.rs` as the start path itself. In this repository the file is `src/main.rs`, not `main.rs` at the project root.

## Supported predicates and operators

| Category | Support |
| --- | --- |
| Start paths | One or more required |
| Traversal flags | `-H`, `-L`, `-P`, `-d`, `-x` |
| Name/path matching | `-name`, `-iname`, `-path`, `-ipath`, `-wholename`, `-iwholename` |
| File types | `f`, `d`, `l`, `b`, `c`, `p`, `s` |
| Metadata | `-empty`, `-maxdepth`, `-mindepth`, `-size`, `-mtime`, `-newer` |
| Actions | `-print`, `-print0`, `-delete`, `-prune` |
| Logic | implicit AND, `-a`, `-and`, `-o`, `-or`, `!`, `-not`, parentheses |

## Compatibility notes

The project is intentionally aligned with FreeBSD `find(1)`, but the current implementation still has a few documented gaps:

- `-prune` is parsed and evaluated, but it does not yet stop descent in the active `WalkDir` traversal.
- `-x` is accepted, but cross-device exclusion is not yet enforced.
- `-H` is parsed, but command-line symlink handling is not yet distinct from the default path behavior.
- special Unix file types (`b`, `c`, `p`, `s`) return `false` on non-Unix targets instead of failing to compile.

Those points are documented so the interface stays honest while the port continues toward fuller FreeBSD parity.

## Man pages

Manual pages are included in [`man/man1/rust-find.1`](./man/man1/rust-find.1) and [`man/man1/find.1`](./man/man1/find.1).

If you have a Unix manpage toolchain available, you can inspect them with:

```sh
man ./man/man1/rust-find.1
```

or render them with:

```sh
mandoc ./man/man1/rust-find.1
```

## Project status

`rust-find` is already useful as a compact BSD-style file finder, but it should still be understood as an in-progress port rather than a complete drop-in replacement for every corner of FreeBSD `find(1)`.

That said, the design direction is fixed: preserve the FreeBSD interface, keep the semantics legible, and close the remaining compatibility gaps without drifting into a different tool.
