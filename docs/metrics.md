# World metrics boundary

`smolworld metrics --json` is a read-only observation command. It reports one
fixed-shape row for each machine declared in the `.smolworld` file.

For a machine with an allocation, smolworld uses only the recorded
`smw-*` name from its world state file and delegates measurement to:

```text
smolvm machine stats --name RECORDED_NAME --format tsv
```

The subprocess record is versioned as `machine-stats-v1`. smolworld verifies
the returned identity and lifecycle state before rendering its own JSON. This
keeps process sampling and disk accounting in smolvm, while world identity,
namespacing, and the world-level schema remain owned by smolworld.

The output has exactly these top-level fields:

```json
{
  "schemaVersion": 1,
  "world": "world-name",
  "machines": []
}
```

Each machine row has exactly these fields:

```text
machine smolvmName state pid cpus memoryMb storageGb overlayGb
cpuSeconds cpuMillis rssMb diskUsedMb
```

Values are JSON `null` when the machine has no recorded allocation or an
observation is unavailable. CPU counters are cumulative for the current host
VMM process and reset on restart; RSS and disk are instantaneous host gauges.
These are not guest-process or guest-filesystem measurements.
