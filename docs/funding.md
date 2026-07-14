# Funding and Enterprise

Tuwunel is a high-performance Matrix homeserver written in Rust. In independent
benchmarks, a single Tuwunel process with its embedded database outperformed a
fully tuned Synapse worker cluster on the busiest endpoint in the Matrix
protocol, at roughly a third of the CPU. We are seeking sponsors to fund the
next stage of the project: true horizontal scaling and clustering, making
Tuwunel the first Matrix homeserver that scales out across machines.

## Independent benchmark results

Researchers at the Federal University of Ceará (UFC), the Federal University of
Piauí (UFPI), and Brazil's Research and Development Center for Communication
Security (CEPESC) published a controlled study of the Matrix `/sync` endpoint
at SBRC 2026, issuing nearly 17,000 sync requests against Synapse and Tuwunel
under increasing load:
[A Comparative Performance Study of the Matrix /sync Endpoint on Synapse and Tuwunel](https://www.researchgate.net/publication/407165702_A_Comparative_Performance_Study_of_the_Matrix_sync_Endpoint_on_Synapse_and_Tuwunel).
The study is part of the Brazilian government's msg gov project and reflects
real operational requirements.

The setup was not tilted in our favor. Synapse ran a production-grade,
high-scalability deployment: a main process, 18 specialized workers, Redis, and
PostgreSQL. Tuwunel ran as a single monolithic process with embedded RocksDB,
following the default installation instructions.

Tuwunel delivered lower response times in three of the four sync
configurations tested. Average initial-sync response times under the heaviest
load, 200 concurrent users in 300 rooms of 300 members each:

| Sync configuration | Synapse (18 workers) | Tuwunel (single process) | Result |
|---|---:|---:|---|
| `1T`: timeline 1, lazy members | 80.1 s | 8.6 s | **9.3x faster** |
| `5T`: timeline 5, lazy members | 74.3 s | 14.0 s | **5.3x faster** |
| `10F`: timeline 10, full member state | 465.3 s | 725.9 s | Synapse ahead |
| `20T`: timeline 20, lazy members (Element Web profile) | 117.5 s | 33.8 s | **3.5x faster** |

With a single user the gap widens to 11x on the lightest configurations. The
cost side tells the same story: in the 200-user scenario Synapse consumed
approximately three times the CPU of Tuwunel in every lazy-loading
configuration (about 30% versus 8 to 9%), and roughly 1% of Synapse's requests
in the heaviest configuration failed with timeouts while Tuwunel completed
them all.

The study also identified our gaps, and we want to be equally upfront about
those. Tuwunel returned larger payloads and fell behind Synapse when clients
request the full member state (`10F`), and our Simplified Sliding Sync (SSS)
implementation stalls after the first window, so the paper's SSS measurements
ran on Synapse alone. Even there the authors note Synapse's SSS lead required
a specialized worker configuration whose optimal application "current
documentation does not clearly describe"; the default setup timed out under
load. Their conclusion about us:

> Once a correct and well-documented implementation becomes available in
> Tuwunel, the expectation is that its performance will further improve,
> potentially surpassing Synapse by following the performance trends observed
> in other synchronization strategies.

That work is already underway: our team is implementing Simplified Sliding
Sync correctly ourselves, on our own resources. This funding initiative aims
at the step beyond it, the one no homeserver has taken: horizontal scaling and
clustering.

## Where Tuwunel already leads

Performance is not the only dimension where Tuwunel is already a strong
alternative to Synapse, and we publish the evidence rather than asking anyone
to take our word for it:

- **[MSC implementation status](development/compliance/msc.md)**: a full audit
  of every Matrix Spec Change proposal, with per-proposal correctness
  percentages. 186 of the 200 in-scope merged MSCs (93%) are implemented, and
  254 MSCs are implemented outright across merged and proposed features.
- **[Complement results](development/compliance/complement.md)**: Tuwunel runs
  the Matrix homeserver acceptance suite continuously, currently passing 81.5%
  of test groups, with raw results and logs committed to the repository.
- **[Synapse Admin API coverage](development/compliance/synapse-admin.md)**:
  existing administration dashboards and moderation bots work against Tuwunel.
- **Modern authentication out of the box**: [OIDC](authentication/oidc-server.md),
  [LDAP delegation](authentication/ldap.md), and
  [enterprise JWT](authentication/jwt.md).
- **[Matrix RTC](calls/matrix_rtc.md)** for Element Call video and voice
  conferencing.
- **Radically simpler operations**: one static binary with an embedded
  database. No PostgreSQL, no Redis, no worker topology to configure, monitor,
  and upgrade. The operating cost advantage measured in the paper compounds
  with the administration time an operator never has to spend.

## What the funding delivers

1. **The first true horizontal scaling in a Matrix homeserver.** No maintained
   homeserver scales horizontally today. Synapse distributes load across
   specialized workers, but they orbit a single coordinating main process and
   a single PostgreSQL primary. Dendrite set out to be multi-process, but its
   development has stalled. This funding lets Tuwunel deliver proper scale-out
   deployment before anyone else, from a codebase that already wins benchmarks
   on one machine. Combined with the CPU numbers above, the goal is simple:
   the largest Matrix deployments at the lowest operating cost available.

2. **Clustered deployment and high availability.** Scaling out is about more
   than throughput. A clustered Tuwunel means redundancy, failover, and
   rolling upgrades without downtime: the deployment properties enterprise
   and government operators are required to provide, and that no Matrix
   homeserver offers today.

3. **A funder-directed feature roadmap.** Tuwunel implements the Matrix
   specification in full along with hundreds of MSC proposals, audited in our
   [status table](development/compliance/msc.md). Synapse's decade of
   development still carries a longer tail of proposals and niche features,
   many of which we have simply had no reason to support. Sponsors decide
   which of those gaps actually matter: funders choose the features we adopt
   and their deployments set our priorities.

4. **Enterprise support and in-house customization.** Sponsors get direct
   access to full-time staff, priority on the features their deployment
   depends on, and custom development where an organization needs behavior the
   public project does not carry.

## A de-risked investment

Tuwunel is the official successor to conduwuit and is developed by full-time
staff. It is already used by companies with a vested interest in its continued
development, and it is primarily sponsored by the government of Switzerland,
where it is deployed for citizens today. New sponsors join an established,
publicly auditable operation with existing institutional backing, not a
speculative effort.

## Get in touch

For sponsorship and enterprise inquiries, contact
[@jason:tuwunel.me](https://matrix.to/#/@jason:tuwunel.me) or email
<jasonzemos@gmail.com>, or contact
[@june:woof.gay](https://matrix.to/#/@june:woof.gay) or email
<june@girlboss.ceo>. General project channels are listed on the
[introduction page](introduction.md).
