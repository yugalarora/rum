# rum — a fast, parallel yum/dnf-compatible package manager written in Rust

**rum** is a drop-in-friendly [`yum`](https://en.wikipedia.org/wiki/Yum_(software)) /
[`dnf`](https://en.wikipedia.org/wiki/DNF_(software)) alternative for RPM-based Linux
distributions, written from scratch in **Rust**. It reads your existing `dnf.conf`,
`yum.conf`, and `.repo` files, talks to the same repositories, and shares the same
RPM database — so it coexists safely with `yum` and `dnf` on the same host while being
**dramatically faster** at metadata refresh, querying, and dependency resolution.

> Keywords: yum replacement, dnf alternative, RPM package manager in Rust, faster dnf,
> parallel yum, Amazon Linux 2023, RHEL, Fedora, Rocky Linux, AlmaLinux, CentOS Stream,
> rpm, librepo, libsolv, rpmvercmp, dependency resolver.

---

## Why rum?

`dnf`/`yum` are written in Python and re-download the full metadata set and spin up a
Python interpreter on every invocation. `rum` is a native Rust binary that fetches
repository metadata in parallel, keeps a lean cache, and starts instantly.

- ⚡ **Fast & parallel** — parallel metadata fetch, verify, and decompress; instant startup.
- 🦀 **Rust-native, memory-safe** — no Python runtime; a pure-Rust librepo (rustls TLS,
  pure-Rust gzip/xz/zstd) and a bit-exact `rpmvercmp` port.
- 🤝 **Coexists with yum/dnf** — rum keeps **no** package database of its own. Installed
  state always comes live from the shared rpmdb (`/var/lib/rpm`), so packages installed
  by `dnf`/`yum` are visible to `rum` and vice-versa.
- 🔁 **Uses your existing config** — `dnf.conf` / `yum.conf`, `/etc/yum.repos.d/*.repo`,
  variable substitution (`$releasever`, `$basearch`, `/etc/dnf/vars/`), mirrorlists and
  metalinks.
- 🧩 **Works across rpmdb backends** — validated on **sqlite** (Amazon Linux 2023, RHEL 9+,
  Fedora 33+) and **BerkeleyDB** (RHEL 8).
- 🔒 **Safe transactions** — the actual install/erase transaction is committed through the
  system `rpm` (librpm), which performs its own dependency and conflict checks before
  writing the rpmdb.

## Benchmarks

Measured on an Amazon Linux 2023 EC2 instance (x86_64), **cold cache** (caches cleared
before every run), `rum` vs the system `dnf`:

| Operation | dnf / yum | rum | Speedup |
|---|---:|---:|---:|
| Refresh metadata (`makecache`) | 21.8 s | 1.85 s | **~12×** |
| Cold query (empty cache → "which versions are available") | 22.0 s | 2.3 s | **~9.5×** |
| Startup latency (`repolist`, warm) | 0.30 s | 0.01 s | **~30×** |
| Install a package + dependencies (`install git`, cold) | 28.3 s | 4.6 s | **~6×** |

Notes for honesty: `rum` currently fetches only the `primary` metadata it needs for
querying/resolution, while `dnf` fetches the full metadata set — so part of the metadata
speedup reflects fetching less. The transaction-commit phase (running `rpm`) is identical
for both. The **startup** and **parallel-fetch** wins are the most fundamental.

Dependency resolution is validated to produce the **identical** package set as `dnf` for
representative closures (e.g. `nginx` → 7 packages, `git` → 8 packages including its Perl
dependencies).

## Installation

### From a release binary

Download the latest `rum-*-x86_64-linux.tar.gz` from the
[Releases](https://github.com/yugalarora/rum/releases) page, verify, and install:

```bash
curl -fsSL -O https://github.com/yugalarora/rum/releases/latest/download/rum-<ver>-x86_64-linux.tar.gz
tar -xzf rum-*-x86_64-linux.tar.gz
sudo install -m755 rum-*/rum /usr/local/bin/rum
```

`rum` links the system `librpm`, which is already present on every RPM-based distro.

### Build from source

Requires Rust (stable) and the RPM development headers:

```bash
# Fedora / RHEL / Amazon Linux
sudo dnf install -y gcc rpm-devel

git clone https://github.com/yugalarora/rum.git
cd rum
cargo build --release
sudo install -m755 target/release/rum /usr/local/bin/rum
```

## Usage

`rum` mirrors familiar `yum`/`dnf` verbs:

```bash
rum repolist                 # list configured repositories
rum makecache                # refresh and cache repo metadata (parallel)
rum list installed           # installed packages (read live from the rpmdb)
rum list available 'kernel*' # latest available versions matching a glob
rum info bash                # package details
rum search web server        # search name + summary
rum provides /usr/bin/tree   # (planned) which package provides a path
rum check-update             # list available updates (exit 100 if any, like dnf)
rum download --resolve git   # download a package + its dependency closure
rum install -y git           # resolve, download, and install
rum remove -y git            # erase
rum upgrade httpd            # upgrade in place
```

Global flags: `-y/--assumeyes`, `--assumeno`, `-v/--verbose` (repeatable),
`RUM_LOG=debug` for tracing.

## Architecture

`rum` is a Cargo workspace of focused crates:

| Crate | Role |
|---|---|
| `rum-config` | Parse `dnf.conf`/`yum.conf` + `.repo` files, variable substitution |
| `rum-repo` | Pure-Rust librepo: parallel fetch, mirrorlist/metalink, checksum verify, gz/xz/zstd, cache |
| `rum-rpm` | Read-only rpmdb access via `librpm` FFI (sqlite + BerkeleyDB backends) |
| `rum-solve` | Bit-exact `rpmvercmp`, EVR comparison, and SAT dependency resolution (via [`resolvo`](https://crates.io/crates/resolvo)) |
| `rum-cli` | The `rum` command-line interface |

**Design principle — coexistence:** rum never maintains its own idea of what is installed.
Every "installed" query reads the shared rpmdb through librpm, and every transaction is
committed through `rpm`, so rum and dnf/yum can be used interchangeably on one system.

`$releasever` is derived the way dnf does it — from the rpmdb `Provides:
system-release(releasever)` — not from `/etc/os-release`, which matters on Amazon Linux
2023 where the two differ.

## Compatibility & status

Implemented and validated: `repolist`, `makecache`, `list`, `info`, `search`,
`check-update`, `download`, `install`, `remove`, `upgrade`. Verified against `dnf`/`rpm`
on Amazon Linux 2023 (sqlite rpmdb) and RHEL 8 (BerkeleyDB rpmdb).

Known limitations / roadmap:

- **Weak dependencies** (`Recommends`/`Suggests`) are not yet pulled; `dnf` installs them
  by default, so rum may install a smaller set for packages that use them.
- **Transaction commit** currently shells out to `rpm` (librpm); a native `rpmtsRun` FFI
  path is planned.
- **Red Hat RHUI** repos (RHEL-on-AWS): region substitution and TLS client-certificate
  auth are implemented and the TLS handshake succeeds, but Red Hat's Pulp content
  endpoint needs additional RHUI-specific handling.
- GPG signature verification of packages is performed by `rpm` at commit time; rum
  additionally verifies SHA-256 checksums of everything it downloads.

## License

Apache-2.0. See [LICENSE](LICENSE).
