# Rust library evaluation

This assessment follows the tested module split. `smolworld` currently has no
third-party Rust dependencies; that remains the chosen implementation.

## TOML parsing: defer `toml` + `serde`

The [`toml`](https://docs.rs/toml/latest/toml/) crate is the standard
Serde-compatible TOML parser. A future migration could deserialize a document
shape with `#[serde(deny_unknown_fields)]` on the world, network, and machine
tables, then retain `config`'s semantic validation for labels, domains, static
addresses, resources, image paths, and dependency cycles.

That would improve full-TOML syntax coverage and parser diagnostics, but it
would be a contract change: today `.smolworld` intentionally accepts only a
small documented subset, and rejects every unsupported TOML construct. The
current 10-test configuration contract is small enough that its dependency-free
parser is easier to audit than a `toml`/`serde`/derive stack.

Adopt only if we decide to support richer TOML syntax or the schema gains
enough fields that maintaining the strict parser becomes a burden. The
migration boundary is `src/config.rs`; no state, switch, gateway, smolvm, or
runtime API should change.

## Packet handling: defer `smoltcp`

[`smoltcp` 0.13.1](https://docs.rs/smoltcp/latest/smoltcp/) provides mature
Ethernet, ARP, IPv4, UDP, and DNS wire representations. It would be a good
candidate to replace the manual packet parsing and encoding in `src/gateway.rs`
if the gateway grows beyond its present ARP plus authoritative A-record DNS
scope.

Its higher-level `Interface` requires a `phy::Device` and works with explicit
buffered socket sets; its DNS socket is a client facility rather than an
authoritative local nameserver. Consequently it does not replace any of these
domain-owned responsibilities:

* Unix-stream length framing and accepted-socket lifecycle in `src/switch.rs`;
* MAC learning and L2 flood/unicast forwarding in `src/switch.rs`;
* authoritative local service-name policy in `src/gateway.rs`; or
* static provisioning and machine lifecycle in `src/smolvm.rs` and
  `src/runtime.rs`.

Adopt only after a small packet-adapter spike proves that `smoltcp::wire` can
preserve the current ARP/DNS reply bytes and E2E behavior without pulling in an
interface/socket scheduler. Start with wire-level parsing only, keep the
existing switch and authority policy, and compare the existing packet unit test
plus `tests/e2e-redis.sh` before and after. Do not adopt its interface or DNS
client abstractions for the present design.

## Decision

No dependency is added. The present standard-library implementation is the
better fit for a two-machine local POC whose lifecycle and packet contract are
now covered by unit and real-VM tests. Any later adoption needs explicit user
approval and a focused proposal naming the crate version, enabled features,
new dependency graph, code boundary, and regression evidence.
