# Metrics adapter note

The normative `metrics --json` command, its closed `schemaVersion: 1` output,
row fields, nullability, and measurement meanings are defined in the
[world contract](world-contract.md). This page is an implementation-scope
note, not a second schema.

For an allocated machine, smolworld uses only the exact recorded `smw-*` name
from world state and delegates host observation to the selected smolvm
adapter. The upstream subprocess record is the literal `machine-stats-v1`
ABI. smolworld verifies the returned identity and lifecycle state before
rendering its world-owned JSON. Process sampling and disk accounting remain in
smolvm; world identity, namespacing, and the public envelope remain in
smolworld.

The adapter never lists or discovers unrelated smolvm machines. Upstream flag,
TSV-position, and ABI changes belong at the narrow adapter boundary described
in [`docs/smolvm-world-protocol.md`](smolvm-world-protocol.md), with the
user-facing consequences recorded in the [world contract](world-contract.md).
