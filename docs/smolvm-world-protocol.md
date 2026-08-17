# Smolworld companion adapter

Smolworld has one internal boundary for operations against a selected smolvm
binary. It is implemented by `src/world_protocol.rs` and `src/smolvm.rs`.

This is not a new smolvm CLI protocol and does not define a Smolfile format.
Smolfiles and the smolvm command surface remain upstream contracts. The
adapter maps typed Smolworld operations—preparation, validation, lifecycle,
statistics, command execution, copy, checkpoint, and restore—to that existing
surface, then verifies the upstream versioned replies before they enter world
state.

Only `src/smolvm.rs` may name upstream command flags, TSV field positions, or
their ABI literals. The rest of smolworld speaks in domain operations and
typed records. An upstream ABI change is handled by updating this one adapter;
it is never papered over with a fallback parser or a Smolfile compatibility
layer.
