# TAM — Terminal Agent Multiplexer

[![CI](https://github.com/ComeBertrand/tam/actions/workflows/ci.yml/badge.svg)](https://github.com/ComeBertrand/tam/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

> Manage units of work, not just processes.

TAM manages **tasks** — named units of work that bind a directory to a series of AI agent runs. It unifies git worktree management and agent process supervision into a single tool.

See [`tam_manifesto.md`](tam_manifesto.md) for the full design.

## Install

### From source

```bash
cargo install tam-cli
```

### GitHub Releases

Download a prebuilt binary from [Releases](https://github.com/ComeBertrand/tam/releases).

### Nix

```bash
nix run github:ComeBertrand/tam
```

## License

MIT
