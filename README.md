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

`dnf`/`yum` are written in Python and spin up an interpreter and rebuild their metadata
state on every invocation. `rum` is a native Rust binary that fetches repository metadata
in parallel, keeps a lean zero-copy cache, and starts instantly.

- ⚡ **Fast & parallel** — parallel metadata fetch, checksum verify, and decompress; a
  memory-mapped, zero-copy metadata cache; instant startup.
- 🦀 **Rust-native, memory-safe** — no Python runtime; a pure-Rust librepo (rustls TLS,
  pure-Rust gzip/xz/zstd), a bit-exact `rpmvercmp` port, and a SAT dependency resolver.
- 🤝 **Coexists with yum/dnf** — rum keeps **no** package database of its own. Installed
  state always comes live from the shared rpmdb (`/var/lib/rpm`), so packages installed
  by `dnf`/`yum` are visible to `rum` and vice-versa.
- 🔁 **Uses your existing config** — `dnf.conf` / `yum.conf`, `/etc/yum.repos.d/*.repo`,
  variable substitution (`$releasever`, `$basearch`, `/etc/dnf/vars/`), mirrorlists,
  metalinks, and AWS RHUI (region substitution + instance authentication).
- 🧩 **Full resolver** — weak dependencies (`Recommends`), rich/boolean dependencies
  (`(A if B)`, `(A or B)`, …), multilib policy, comps groups and environments, and a
  `filelists` fallback for file-path requirements. Produces the **same** package set as
  `dnf` on validated closures.
- 🔒 **Native, safe transactions** — installs and removals are committed in-process
  through `librpm` (`rpmtsRun`), which performs its own dependency and conflict checks and
  runs scriptlets before writing the rpmdb. Everything downloaded is SHA-256 verified;
  package signatures are checked by rpm at commit time.

## Benchmarks

Measured on an Amazon Linux 2023 EC2 instance (x86_64), **cold cache** (caches cleared
before every run), `rum` vs the system `dnf`:

| Operation | dnf / yum | rum | Speedup |
|---|---:|---:|---:|
| Refresh metadata (`makecache`) | 21.8 s | 1.85 s | **~12×** |
| Cold query (empty cache → "which versions are available") | 22.0 s | 2.3 s | **~9.5×** |
| Startup latency (`repolist`, warm) | 0.30 s | 0.01 s | **~30×** |
| List a group (`group list`, warm) | 0.65 s | 0.05 s | **~13×** |
| Install a package + dependencies (`install git`, cold) | 28.3 s | 4.6 s | **~6×** |

The transaction-commit phase (unpacking + scriptlets, via librpm) is inherent to RPM and
identical for both tools; the **startup**, **parallel-fetch**, and **cached-query** wins
are where rum pulls ahead. Dependency resolution produces the **identical** package set as
`dnf` on representative closures — e.g. `nginx` → 7, `git` → 8, `mariadb105-server` → 19,
`postgresql15-server-devel` → 36 — including weak and rich dependencies.

## Supported operating systems

rum targets the RPM ecosystem and is continuously tested on both **x86_64** and
**aarch64 (arm64)**. Validated distributions:

| Distribution | Architectures | rpmdb backend | rpm |
|---|---|---|---|
| Amazon Linux 2023 | x86_64, aarch64 | sqlite | 4.16 |
| RHEL 10 | x86_64, aarch64 | sqlite | 4.19 |
| RHEL 9 | x86_64 | sqlite | 4.16 |
| RHEL 8 | x86_64 | BerkeleyDB | 4.14 |
| Rocky Linux 9 | x86_64, aarch64 | sqlite | 4.16 |

Because rum reads standard yum/dnf configuration and links the system `librpm`, it also
works on the wider RPM family that shares those conventions — **Fedora, AlmaLinux, and
CentOS Stream** — across the sqlite and BerkeleyDB rpmdb backends.

## Installation

### From a release binary

Binaries are published for **x86_64** and **aarch64**. Pick your architecture
(`uname -m`), download from the [Releases](https://github.com/yugalarora/rum/releases)
page, verify, and install:

```bash
arch=$(uname -m)                     # x86_64 or aarch64
base=https://github.com/yugalarora/rum/releases/latest/download
curl -fsSL -O "$base/rum-<ver>-${arch}-linux.tar.gz"
curl -fsSL -O "$base/rum-<ver>-${arch}-linux.tar.gz.sha256"
sha256sum -c "rum-<ver>-${arch}-linux.tar.gz.sha256"
tar -xzf "rum-<ver>-${arch}-linux.tar.gz"
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
rum check-update             # list available updates (exit 100 if any, like dnf)
rum download --resolve git   # download a package + its dependency closure
rum install -y git           # resolve, show transaction, download, and install
rum remove -y git            # erase
rum upgrade httpd            # upgrade in place

rum group list               # list available comps groups
rum group install "Development Tools"   # install a group's packages
rum install @development     # groups also work as @-targets
rum clean all                # clear cached metadata and packages
```

`rum install` is idempotent: installing a package already present at the latest available
version is a no-op ("Nothing to do"), and every state-changing command shows the full
transaction before downloading anything.

Global flags: `-y/--assumeyes`, `--assumeno`, `-v/--verbose` (repeatable),
`RUM_LOG=debug` for tracing.

## Architecture

`rum` is a Cargo workspace of focused crates:

| Crate | Role |
|---|---|
| `rum-config` | Parse `dnf.conf`/`yum.conf` + `.repo` files, variable substitution |
| `rum-repo` | Pure-Rust librepo: parallel fetch, mirrorlist/metalink, RHUI auth, checksum verify, gz/xz/zstd, zero-copy mmap cache, comps |
| `rum-rpm` | rpmdb access + native `librpm` transaction engine via FFI (sqlite + BerkeleyDB backends) |
| `rum-solve` | Bit-exact `rpmvercmp`, EVR comparison, and SAT dependency resolution (via [`resolvo`](https://crates.io/crates/resolvo)), including rich/boolean dependencies |
| `rum-cli` | The `rum` command-line interface |

**Design principle — coexistence:** rum never maintains its own idea of what is installed.
Every "installed" query reads the shared rpmdb through librpm, and every transaction is
committed through librpm, so rum and dnf/yum can be used interchangeably on one system.
The rpmdb is opened read-only for queries, so rum never blocks a concurrent dnf run.

`$releasever` is derived the way dnf does it — from the rpmdb `Provides:
system-release(releasever)` — not from `/etc/os-release`, which matters on Amazon Linux
2023 where the two differ.

## License

Apache-2.0. See [LICENSE](LICENSE).
