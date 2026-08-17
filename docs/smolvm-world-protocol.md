# Smolworld companion adapter

The user-facing ownership and boundary rules are defined in the [world
contract](world-contract.md). This page records only the adapter scope; it is
not a new smolvm CLI protocol, Smolfile format, or parallel lifecycle
specification.

Smolworld has one internal boundary for operations against a selected smolvm
binary. It is implemented by `src/companion_adapter.rs` and `src/smolvm.rs`.

The adapter maps typed smolworld operations—preparation, validation, lifecycle,
statistics, command execution, copy, checkpoint, and restore—to the existing
upstream surface, then verifies versioned replies before they enter world
state. This is not a new smolvm CLI protocol and does not define a Smolfile
format. Smolfiles and the smolvm command surface remain upstream contracts.

Only `src/smolvm.rs` may name upstream command flags, TSV field positions, or
their ABI literals. The rest of smolworld speaks in domain operations and
typed records. An upstream ABI change is handled at this adapter boundary; it
is never papered over with a fallback parser or a Smolfile compatibility layer.
The literal public labels and schemas affected by such a change belong in the
[world contract](world-contract.md).
