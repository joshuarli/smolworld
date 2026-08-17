#!/usr/bin/env python3
"""Measure the currently available one-to-many SmolVM transition substrate.

This is deliberately not a durable smolworld checkpoint benchmark. A SmolVM
fork freezes one golden machine and creates non-forkable, disposable children.
It measures that primitive against fresh, local-archive cold starts.

The harness never pulls an image or uses a host network. Its caller supplies
the prepared OCI archive that a Smolworld material lock has already sealed.

Required environment:
    SMOLWORLD_TRANSITION_BENCH=1
    SMOLVM_BIN=/absolute/path/to/smolvm
    SMOLVM_AGENT_ROOTFS=/absolute/path/to/agent-rootfs
    SMOLWORLD_TRANSITION_ARCHIVE=/absolute/path/to/prepared/archive.tar

Optional environment:
    SMOLWORLD_TRANSITION_ITERATIONS=3
    SMOLWORLD_TRANSITION_BRANCHES=3
    SMOLVM_LIB_DIR=/absolute/path/to/libkrun
    DYLD_LIBRARY_PATH=/absolute/path/to/libkrun

The process always creates and removes an isolated SMOLVM_RUNTIME_ROOT. This
is SmolVM's cross-platform per-machine storage boundary, so prior machine state
cannot affect timing or byte measurements.
"""

from __future__ import annotations

import os
import json
import secrets
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Callable, Sequence


MIB = 1024 * 1024
MUTATION_BYTES = 4 * MIB


class BenchmarkError(Exception):
    """A configuration or measurement prerequisite was invalid."""


def emit(line: str) -> None:
    """Write a benchmark record promptly when stdout is captured by a runner."""

    print(line, flush=True)


def require_file(variable: str) -> Path:
    value = os.environ.get(variable, "")
    path = Path(value)
    if not value or not path.is_file():
        raise BenchmarkError(f"{variable} must name a regular file: {value}")
    return path


def require_directory(variable: str) -> Path:
    value = os.environ.get(variable, "")
    path = Path(value)
    if not value or not path.is_dir():
        raise BenchmarkError(f"{variable} must name a directory: {value}")
    return path


def positive_integer(variable: str, default: int) -> int:
    value = os.environ.get(variable, str(default))
    try:
        parsed = int(value)
    except ValueError as error:
        raise BenchmarkError(f"{variable} must be a positive integer: {value}") from error
    if parsed < 1:
        raise BenchmarkError(f"{variable} must be a positive integer: {value}")
    return parsed


def accounted_file_blocks_bytes(root: Path) -> int:
    """Return the sum of each file's allocated blocks beneath ``root``.

    This is a portable per-world accounting upper bound, not physical APFS
    consumption: clonefile reports shared blocks for both the golden and its
    clone. It deliberately excludes live guest RAM; macOS offers no stable
    per-process proportional-set-size interface, while RSS double-counts CoW
    pages.
    """

    total = 0
    for directory, subdirectories, filenames in os.walk(root, followlinks=False):
        entries = [Path(directory)]
        entries.extend(Path(directory, name) for name in subdirectories)
        entries.extend(Path(directory, name) for name in filenames)
        for entry in entries:
            try:
                total += entry.lstat().st_blocks * 512
            except FileNotFoundError:
                # A SmolVM teardown or Unix socket can disappear while being
                # sampled. The benchmark only samples between foreground calls,
                # so treating it as absent is the least surprising result.
                continue
    return total


def volume_used_bytes(root: Path) -> int:
    """Return used bytes on root's enclosing volume.

    This captures APFS clonefile sharing correctly, but it is necessarily noisy:
    unrelated host writes to the same volume are visible. Small deltas therefore
    represent a range around zero, not an exact per-world quota.
    """

    filesystem = os.statvfs(root)
    return (filesystem.f_blocks - filesystem.f_bavail) * filesystem.f_frsize


class SmolvmBenchmark:
    def __init__(self, smolvm_bin: Path, archive: Path, runtime_root: Path) -> None:
        self.smolvm_bin = smolvm_bin
        self.archive = archive
        self.runtime_root = runtime_root
        self.owned_names: list[str] = []
        self.iteration_names: list[str] = []

    def command(self, arguments: Sequence[str]) -> subprocess.CompletedProcess[str]:
        completed = subprocess.run(
            [str(self.smolvm_bin), *arguments],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        if completed.returncode != 0:
            if completed.stderr:
                sys.stderr.write(completed.stderr)
            rendered = " ".join(arguments)
            raise BenchmarkError(
                f"smolvm command failed with exit {completed.returncode}: {rendered}"
            )
        return completed

    def register_machine(self, name: str) -> None:
        # Children and cold controls are registered after their golden. Deleting
        # in reverse creation order always removes a child before its disk base.
        self.owned_names.append(name)
        self.iteration_names.append(name)

    def assert_name_absent(self, name: str) -> None:
        """Refuse to claim a machine name which this benchmark did not create."""

        output = self.command(["machine", "ls", "--json"]).stdout
        try:
            machines = json.loads(output)
        except json.JSONDecodeError as error:
            raise BenchmarkError(f"smolvm machine ls emitted invalid JSON: {error}") from error
        if not isinstance(machines, list):
            raise BenchmarkError("smolvm machine ls JSON is not an array")
        observed = {
            machine.get("name")
            for machine in machines
            if isinstance(machine, dict) and isinstance(machine.get("name"), str)
        }
        if name in observed:
            raise BenchmarkError(
                f"refusing to reuse existing machine name {name}; benchmark owns only newly created names"
            )

    def delete_names(self, names: Sequence[str]) -> None:
        failures: list[str] = []
        for name in reversed(names):
            try:
                self.command(["machine", "delete", "--name", name, "-f"])
            except BenchmarkError as error:
                failures.append(str(error))
                continue
            self.owned_names.remove(name)
            if name in self.iteration_names:
                self.iteration_names.remove(name)
        if failures:
            raise BenchmarkError("benchmark cleanup failed: " + "; ".join(failures))

    def cleanup_iteration(self) -> None:
        self.delete_names(self.iteration_names[:])

    def cleanup(self) -> None:
        self.delete_names(self.owned_names[:])
        shutil.rmtree(self.runtime_root)
        if self.runtime_root.exists():
            raise BenchmarkError(
                f"benchmark cleanup left its private runtime root: {self.runtime_root}"
            )

    def create_machine(self, name: str) -> None:
        self.assert_name_absent(name)
        self.command(
            [
                "machine",
                "create",
                "--name",
                name,
                "--image",
                str(self.archive),
                "--cpus",
                "1",
                "--mem",
                "256",
                "--storage",
                "2",
                "--overlay",
                "1",
                "--",
                "/bin/sh",
                "-c",
                "exec sleep infinity",
            ]
        )
        self.register_machine(name)

    def start_golden(self, name: str) -> None:
        self.command(["machine", "start", "--name", name, "--forkable"])

    def start_cold(self, name: str) -> None:
        self.command(["machine", "start", "--name", name])

    def fork(self, golden: str, clone: str) -> None:
        self.assert_name_absent(clone)
        self.command(["machine", "fork", "--golden", golden, "--name", clone])
        self.register_machine(clone)

    def write_mutation(self, name: str, marker: str) -> None:
        # `marker` is an argument, not source text. Keeping the guest script
        # constant makes this a test of state transition, not host-shell quoting.
        guest_script = """set -eu
marker=$1
directory=/workspace/world-transition-benchmark
mkdir -p "$directory"
printf '%s' "$marker" > "$directory/marker"
dd if=/dev/zero of="$directory/$marker.bin" bs=1048576 count=4 conv=fsync status=none
test "$(cat "$directory/marker")" = "$marker"
test "$(wc -c < "$directory/$marker.bin")" = 4194304
"""
        self.command(
            [
                "machine",
                "exec",
                "--name",
                name,
                "--",
                "/bin/sh",
                "-ceu",
                guest_script,
                "smolworld-transition-mutation",
                marker,
            ]
        )

    def measure(
        self,
        operation: str,
        iteration: int,
        branch: str,
        action: Callable[[], None],
    ) -> None:
        accounted_before = accounted_file_blocks_bytes(self.runtime_root)
        volume_before = volume_used_bytes(self.runtime_root)
        started = time.monotonic_ns()
        action()
        finished = time.monotonic_ns()
        accounted_after = accounted_file_blocks_bytes(self.runtime_root)
        volume_after = volume_used_bytes(self.runtime_root)
        emit(
            f"{operation}\t{iteration}\t{branch}\t"
            f"{(finished - started) / 1_000_000:.3f}\t"
            f"{accounted_after - accounted_before}\t"
            f"{volume_after - volume_before}"
        )


def main() -> int:
    if os.environ.get("SMOLWORLD_TRANSITION_BENCH") != "1":
        raise BenchmarkError("set SMOLWORLD_TRANSITION_BENCH=1 to run VM-transition measurements")

    smolvm_bin = require_file("SMOLVM_BIN")
    agent_rootfs = require_directory("SMOLVM_AGENT_ROOTFS")
    archive = require_file("SMOLWORLD_TRANSITION_ARCHIVE")
    iterations = positive_integer("SMOLWORLD_TRANSITION_ITERATIONS", 3)
    branches = positive_integer("SMOLWORLD_TRANSITION_BRANCHES", 3)

    # A deep macOS TMPDIR forces SmolVM to a shared fallback socket root. A
    # unique direct child of /tmp stays below Darwin's sockaddr_un path limit,
    # preserving both private state and a valid byte census.
    runtime_root = Path(
        tempfile.mkdtemp(prefix=f"smolworld-transition-benchmark-{secrets.token_hex(8)}.", dir="/tmp")
    )
    os.environ["SMOLVM_AGENT_ROOTFS"] = str(agent_rootfs)
    os.environ["SMOLVM_RUNTIME_ROOT"] = str(runtime_root)
    benchmark = SmolvmBenchmark(smolvm_bin, archive, runtime_root)

    def interrupt(_signum: int, _frame: object) -> None:
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupt)

    emit("# smolworld transition substrate benchmark")
    emit(f"# archive={archive}")
    emit(
        f"# iterations={iterations} branches={branches} cpus=1 memory_mib=256 "
        f"mutation_bytes={MUTATION_BYTES}"
    )
    emit(f"# smolvm_runtime_root={runtime_root} (removed after the run)")
    emit(
        "# accounted_file_blocks_delta_bytes counts APFS-shared clone blocks per file; "
        "volume_used_delta_bytes is physical but host-noisy"
    )
    emit(
        "operation\titeration\tbranch\twall_ms\t"
        "accounted_file_blocks_delta_bytes\tvolume_used_delta_bytes"
    )

    try:
        for iteration in range(1, iterations + 1):
            golden = f"smw-transition-bench-{os.getpid()}-{iteration}-golden"
            benchmark.measure("create", iteration, "base", lambda: benchmark.create_machine(golden))
            benchmark.measure("start", iteration, "base", lambda: benchmark.start_golden(golden))
            benchmark.measure(
                "base_mutation",
                iteration,
                "base",
                lambda: benchmark.write_mutation(golden, f"base-{iteration}"),
            )

            for branch in range(1, branches + 1):
                clone = f"smw-transition-bench-{os.getpid()}-{iteration}-clone-{branch}"
                benchmark.measure("fork", iteration, str(branch), lambda: benchmark.fork(golden, clone))
                benchmark.measure(
                    "fork_mutation",
                    iteration,
                    str(branch),
                    lambda: benchmark.write_mutation(clone, f"fork-{iteration}-{branch}"),
                )

                cold = f"smw-transition-bench-{os.getpid()}-{iteration}-cold-{branch}"
                benchmark.measure(
                    "cold_create", iteration, str(branch), lambda: benchmark.create_machine(cold)
                )
                benchmark.measure(
                    "cold_start", iteration, str(branch), lambda: benchmark.start_cold(cold)
                )
                benchmark.measure(
                    "cold_mutation",
                    iteration,
                    str(branch),
                    lambda: benchmark.write_mutation(cold, f"cold-{iteration}-{branch}"),
                )

            # Bound disk use and ensure the next sample has no earlier base or
            # clone competing for VMM or filesystem resources.
            benchmark.cleanup_iteration()
    finally:
        benchmark.cleanup()

    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except BenchmarkError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(2)
    except KeyboardInterrupt:
        print("interrupted: cleaned exact benchmark machines and runtime root", file=sys.stderr)
        raise SystemExit(130)
