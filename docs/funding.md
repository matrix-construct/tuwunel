# Enterprise Development Partnership

[Tuwunel](https://github.com/matrix-construct/tuwunel)'s development is
directly driven by the needs of its corporate sponsors. For these businesses
and governments, communication systems are often critical infrastructure, and
Tuwunel forms part of that infrastructure. They create value through features
tailored to their needs, control costs through efficient performance that
scales with demand, and expect quality of service that never buckles under
continued growth.

We exist because companies want to maximize profit. Although Tuwunel is
available at no cost and may already meet the needs of many individuals and
companies, our partners remain steadfast in their commitment to *maximizing*
profit.

Tuwunel is developing into a **highly available, horizontally scalable**,
specification-compliant Matrix cluster capable of handling **high message
volumes** with **low latency** through **hardware acceleration** and robust
**quality-of-service** guarantees. We can do this better together. Partner with
Tuwunel and finally get this right.

### Proven results

An independent 2026 study compared a single Tuwunel process with an 18-worker
Synapse deployment in a 200-user test. **Tuwunel completed three of the four
benchmark scenarios faster than Synapse.**

Researchers at the Federal University of Ceará (UFC), the Federal University
of Piauí (UFPI), and Brazil's Research and Development Center for Communication
Security (CEPESC) published
[A Comparative Performance Study of the Matrix /sync Endpoint on Synapse and Tuwunel](https://doi.org/10.5753/sbrc.2026.19722)
at SBRC 2026. The controlled study issued nearly 17,000 `/sync` requests under
increasing load. It was conducted as part of the Brazilian government's
[msg gov secure communications project](https://www.gov.br/abin/pt-br/centrais-de-conteudo/noticias/abin-e-universidade-federal-do-ceara-debatem-avancos-do-aplicativo-de-comunicacao-segura-msg-gov)
and reflects operational requirements from that project.

Tuwunel delivered lower response times in three of the four version 3
`/sync`<sup>⋆</sup> configurations. The following results are average initial
sync response times under the heaviest load tested: 200 concurrent users, each
in 300 rooms with 300 members per room.

<table>
  <thead>
    <tr>
      <th>Sync configuration</th>
      <th>Synapse, 18 workers</th>
      <th>Tuwunel, one process</th>
      <th>Result</th>
    </tr>
  </thead>
  <tbody>
    <tr>
      <td><code>1T</code>: timeline 1, lazy members</td>
      <td>80.1 s</td>
      <td>8.6 s</td>
      <td><strong>9.3x faster</strong></td>
    </tr>
    <tr>
      <td><code>5T</code>: timeline 5, lazy members</td>
      <td>74.3 s</td>
      <td>14.0 s</td>
      <td><strong>5.3x faster</strong></td>
    </tr>
    <tr>
      <td><code>10F</code>: timeline 10, full member state</td>
      <td>465.3 s</td>
      <td>725.9 s</td>
      <td>Synapse <strong>1.6x faster</strong></td>
    </tr>
    <tr>
      <td><code>20T</code>: timeline 20, lazy members, the Element Web profile</td>
      <td>117.5 s</td>
      <td>33.8 s</td>
      <td><strong>3.5x faster</strong></td>
    </tr>
  </tbody>
</table>

<div style="clear: both"></div>

<sup>⋆ Tuwunel has since implemented Simplified Sliding Sync, as recorded in our
[MSC implementation audit](development/compliance/msc.md). That implementation
postdates the study and has not yet been measured by the same independent
benchmark.</sup>

### Production foundation

Tuwunel is the official successor to
[conduwuit](https://github.com/girlbossceo/conduwuit) and is developed by
full-time staff. It has an active public codebase,
[regular releases](https://github.com/matrix-construct/tuwunel/releases),
institutional users, and existing
[sponsorship from the Swiss government](https://matrix.org/blog/2025/11/07/this-week-in-matrix-2025-11-07/).
Tuwunel is deployed for use by citizens in Switzerland. New sponsors join a
working, publicly auditable project with production use, institutional backing,
and independent benchmark results, rather than a speculative prototype.

### Current status

The clustering initiative starts from a working, independently tested
homeserver. We publish the evidence needed for technical due diligence:

- **[MSC implementations](development/compliance/msc.md):** Our audit covers
  Matrix Spec Change proposals, including correctness scores and supporting
  evidence. Of 200 in-scope merged MSCs, 186 are implemented, for 93% coverage.
  Across merged and proposed features, 254 MSCs are implemented outright.

- **[Complement progress](development/compliance/complement.md):** We run the
  Matrix homeserver acceptance suite continuously. Tuwunel currently passes
  over 80% of top-level test groups, with raw results and logs committed to the
  repository.

- **[Synapse Admin API](development/compliance/synapse-admin.md):** Tuwunel
  supports most known endpoints today, including the core user, room, device,
  registration, and moderation surfaces used by existing tools.

- **Enterprise authentication:** Tuwunel has built-in
  [OAuth 2.0 and OIDC](authentication/oidc-server.md),
  [LDAP support](authentication/ldap.md), and
  [JWT authentication](authentication/jwt.md).

- **[Matrix RTC](calls/matrix_rtc.md):** Tuwunel supports Element Call video
  and voice conferencing.

### Future plans

1. **Tuwunel-native multi-node scale-out.** Synapse can distribute work across
   [worker processes that share PostgreSQL and Redis](https://element-hq.github.io/synapse/latest/workers.html).
   Our objective is to design scale-out around Tuwunel's own architecture while
   retaining its operational simplicity wherever possible. Sponsors fund the
   architecture, implementation, migration path, and production validation
   needed to turn that objective into a supported deployment model.

2. **High availability and business continuity.** The program targets
   redundancy, failure recovery, and rolling maintenance across nodes. Concrete
   availability targets, recovery objectives, and acceptance tests can be
   agreed with sponsors so that the result maps to real operational and
   compliance requirements.

3. **Roadmap alignment with sponsor requirements.** Funders help select the
   feature gaps and deployment constraints that matter to their organizations.
   Each engagement can be organized around documented requirements,
   milestones, acceptance criteria, and release targets, giving technical and
   procurement teams a clear delivery path.

4. **Direct enterprise engineering and support.** Sponsors receive access to
   full-time project staff, priority for deployment-critical defects and
   features, and custom integration work where identity, administration,
   policy, migration, or infrastructure requirements go beyond the public
   project's defaults.

### Get in touch

For sponsorship and enterprise inquiries, contact
[@jason:tuwunel.me](https://matrix.to/#/@jason:tuwunel.me) on Matrix or at
<jasonzemos@gmail.com>. You can also contact
[@june:woof.gay](https://matrix.to/#/@june:woof.gay) on Matrix or at
<june@girlboss.ceo>. General project channels are listed on the
[introduction page](introduction.md).
