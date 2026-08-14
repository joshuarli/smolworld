# Redis foundation world

This fixture is the first Smolfile-composed world in the Niceforge migration.
It contains two machines on one private static network:

| machine | Smolfile | role |
| --- | --- | --- |
| `redis` | `smol/redis.Smolfile` | Redis server |
| `runner` | `smol/runner.Smolfile` | long-lived command runner |

`.smolworld` owns only the world name, network, machine names, and startup
order. Each Smolfile owns its local image source, command, and resources. The
runner uses `redis-cli` for an explicit connectivity action after the world is
up; this is not a smolworld readiness or health contract.

`redis.tar` is an ignored, host-prepared local image archive. It is deliberately
not created by the test and the foundation gate never invokes `docker`,
`docker compose`, `orbctl`, or a Docker socket. Supply an already prepared
archive at this path, or set `SMOLWORLD_REDIS_ARCHIVE` when running the opt-in
integration harness. Image preparation is the separate `smolworld prepare`
boundary described in `NOICEFORGE.md`.

The expected lifecycle is:

```text
prepare (mutating host material + lockfile)
  -> check (read-only: no world state, listener, or machine)
  -> up (Redis + runner, external private NIC only)
  -> runner DNS lookup + redis-cli PING
  -> down (only the recorded world machines and sockets)
```

Run the static fixture contract check without a VM:

```bash
bash tests/check-redis-foundation-fixture.sh
```

Run the real Apple-Silicon/Hypervisor integration only with prepared local
artifacts:

```bash
SMOLWORLD_E2E=1 \
SMOLWORLD_SMOLVM=/path/to/smolvm \
SMOLVM_AGENT_ROOTFS=/path/to/agent-rootfs \
SMOLVM_LIB_DIR=/path/to/smolvm/lib \
bash tests/e2e-redis-foundation.sh
```
