# GitHub Workflows

All workflow files live in `.github/workflows/`. Their role is described in the
workflows README: they are a "thin client" that drives the Docker builder in
`docker/`. Control flow and GitHub integration live here; the actual build and
test logic lives in `bake.hcl`.

## Workflow overview

| File | Type | Purpose |
|---|---|---|
| `main.yml` | Push/PR trigger | Orchestrator; initializes and calls all others |
| `lint.yml` | `workflow_call` | Lint phase: fmt, typos, audit, lychee, check, clippy |
| `test.yml` | `workflow_call` | Test phase: all test jobs |
| `bake.yml` | `workflow_call` | Bake executor: runs `docker/bake.sh` and extracts artifacts |
| `package.yml` | `workflow_call` | Package phase: docs, book, binaries, containers, distro |
| `publish.yml` | `workflow_call` | Publish phase: GitHub Pages, GHCR, Docker Hub |
| `autocopr.yml` | Cron + dispatch | Daily RPM spec auto-update |

---

## `main.yml` — Orchestrator

**Triggers**: push (all branches and `v*` tags), pull_request, workflow_dispatch.

**Concurrency**: One run per `workflow + ref` combination. In-progress runs are
not cancelled — they run to completion.

### `init` job

The `init` job runs before everything else. It has two responsibilities:

1. **Create the BuildKit builder** if one does not already exist for the current
   GitHub actor. Builder settings vary by runner pool:

   | Runner | Reserved space | Max space |
   |---|---|---|
   | `het` | 192 GB | 384 GB |
   | `aws` | 48 GB  | 64 GB  |
   | `gcp` | 160 GB | 192 GB |

2. **Emit output variables** that all downstream jobs consume as inputs.
   These control which matrix dimensions to test, which phases to enable, and
   metadata like the pages URL and release upload URL.

   Key outputs include:
   - Matrix selectors: `cargo_profiles`, `feat_sets`, `rust_toolchains`,
     `rust_targets`, `sys_targets`, etc.
   - Phase flags: `enable_lint`, `enable_test`, `enable_package`, `enable_publish`
   - Per-job flags: `enable_test_nix`, `enable_test_complement`,
     `enable_package_distro`, `enable_package_checks`, etc.
   - Branch classifications: `is_release`, `is_main`, `is_test`, `is_pull`,
     `is_fat`, `is_dev`
   - URLs: `pages_url`, `release_url`

   `is_fat` is true for `main`, `test`, and release tags — these refs get the
   full build matrix, distro packages, and multi-arch runners.

3. **Create a GitHub release** for version tags. Draft status is detected from
   the tag name: tags containing `-rc`, `-alpha`, `-beta` produce pre-releases.

### Input precedence

For each matrix dimension, `init` resolves inputs in priority order:

1. `workflow_dispatch` manual input
2. Repository variable (e.g. `vars.CARGO_PROFILES`)
3. Default value defined in `main.yml`

This lets repository administrators adjust the default CI matrix via GitHub
repository variables without editing workflow files.

### Downstream job calls

After `init`, four jobs run in order, each gated on the previous:

```
init → lint → test → package → publish
```

Each calls its corresponding reusable workflow, passing the matrix and flag
outputs from `init` as inputs.
