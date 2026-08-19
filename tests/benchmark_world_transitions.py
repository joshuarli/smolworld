#!/usr/bin/env python3
"""Measure cold SmolVM transition costs without collapsing their boundaries.

This measures the upstream per-machine substrate rather than a durable
smolworld checkpoint. It retains a fork reference and adds a cold-start
scaling matrix with separate signals:

* archive creation/start: prepared local-image staging, guest boot, agent
  readiness, local-image setup, and configured workload launch.
* archive_forkable creation/start: the same image path with the forkable launch
  mode smolworld uses for durable checkpoint coordination.
* world startup: a real smolworld supervisor records material preparation,
  each reported private-NIC attachment, and the all-machines-ready barrier.
* prepared-world attachment: an externally prepared, sealed world records
  configuration/check, each declared machine's create/start/attachment
  boundaries, a selected declared service becoming host visible, and a
  successful exact command attachment.

The direct scenarios never configure a NIC: attaching a raw Unix listener is
not a substitute for the smolworld L2 switch and gateway. The world scenarios
exercise that boundary through smolworld itself. The harness never lists,
cleans, or otherwise acts on machines outside exact names recorded by its
generated worlds. The content-addressed local-image cache is user-owned and is
deliberately not cleared, so archive-create timings include the real
hash/staging path with the cache state that existed at run time.

Required environment:
    SMOLWORLD_TRANSITION_BENCH=1
    SMOLVM_BIN=/absolute/path/to/smolvm
    SMOLVM_AGENT_ROOTFS=/absolute/path/to/agent-rootfs

At least one benchmark scenario:
    SMOLWORLD_TRANSITION_ARCHIVE=/absolute/path/to/prepared/archive.tar
    SMOLWORLD_TRANSITION_PREPARED_WORLD=/absolute/path/to/prepared/.smolworld
    SMOLWORLD_TRANSITION_ATTACH_SERVICE=<declared-service>

Optional environment:
    SMOLWORLD_TRANSITION_ITERATIONS=3
    SMOLWORLD_TRANSITION_BRANCHES=3
    SMOLWORLD_TRANSITION_CONCURRENCY=1,2,4
    SMOLWORLD_BIN=/absolute/path/to/smolworld
    SMOLVM_LIB_DIR=/absolute/path/to/libkrun
    DYLD_LIBRARY_PATH=/absolute/path/to/libkrun
    SMOLWORLD_TRANSITION_TRACE=1
    SMOLWORLD_TRANSITION_ATTACH_SETTLE_SECONDS=2
    RUST_LOG=smolvm::agent::manager=debug

The output is TSV. machine_sample rows are per-machine latencies and
wave_sample rows are wall-clock times for one serial or parallel wave.
failure_sample rows preserve an unsuccessful wave (including lock contention)
without relabeling it as a timing sample. summary and wave_summary report
p50/p95 over their matching successful records. Storage accounting is
intentionally out of scope: current macOS smolvm does not expose an
isolatable per-benchmark runtime root, and a synthetic directory would report
misleading zeroes.

Set SMOLWORLD_TRANSITION_TRACE=1 to emit trace_sample and trace_summary rows
from smolvm's existing boot instrumentation. Trace stages are nested upstream
spans, not additive wall-clock timings; the start and NIC-attachment rows
remain the authoritative external boundaries.

The prepared-world profile never invokes `prepare` or removes sealed material.
It requires every declared service to be absent before it starts, then owns
only the supervisor it started for the measurement; cleanup is through that
supervisor or its exact `down` command. Its `ps` observation is deliberately
distinct from command attachment: lifecycle status is not application
readiness, while the successful exact command proves only the command
transport boundary.
"""

from __future__ import annotations

from collections import defaultdict
from dataclasses import dataclass
from datetime import datetime
import json
import math
import os
import re
import secrets
import shutil
import signal
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path
from typing import Callable, Sequence


MUTATION_BYTES = 4 * 1024 * 1024
MAX_CONCURRENCY = 250
WORLD_READY_TIMEOUT_SECONDS = 120.0
TRACE_ENVIRONMENT_VARIABLE = "SMOLWORLD_TRANSITION_TRACE"
PREPARED_WORLD_VARIABLE = "SMOLWORLD_TRANSITION_PREPARED_WORLD"
ATTACH_SERVICE_VARIABLE = "SMOLWORLD_TRANSITION_ATTACH_SERVICE"
ATTACH_SETTLE_SECONDS_VARIABLE = "SMOLWORLD_TRANSITION_ATTACH_SETTLE_SECONDS"
DEFAULT_ATTACH_SETTLE_SECONDS = 2.0
BOOT_TIMING_PATTERN = re.compile(
    r"^\[(?P<scope>proc|boot)\]\s+(?P<label>.+?)\s+(?P<milliseconds>\d+)ms\s*$",
    re.MULTILINE,
)
PARENT_BOOT_PATTERNS = (
    (
        "launch_disks_ready",
        re.compile(
            r"\bboot: disks ready\b.*\belapsed_ms=(\d+)"
            r"|\belapsed_ms=(\d+).*\bboot: disks ready\b"
        ),
    ),
    (
        "launch_config_written",
        re.compile(
            r"\bboot: config written\b.*\belapsed_ms=(\d+)"
            r"|\belapsed_ms=(\d+).*\bboot: config written\b"
        ),
    ),
    (
        "launch_subprocess_spawn",
        re.compile(
            r"\bboot: subprocess spawned\b.*\bspawn_ms=(\d+)"
            r"|\bspawn_ms=(\d+).*\bboot: subprocess spawned\b"
        ),
    ),
    (
        "agent_ready",
        re.compile(
            r"\b(?:clone )?agent ready (?:\([^)]*\)|via socket).*\belapsed_ms=(\d+)"
            r"|\belapsed_ms=(\d+).*\b(?:clone )?agent ready (?:\([^)]*\)|via socket)"
        ),
    ),
    (
        "agent_boot_complete",
        re.compile(
            r"\bagent VM is ready\b.*\bboot_ms=(\d+(?:\.\d+)?)"
            r"|\bboot_ms=(\d+(?:\.\d+)?).*\bagent VM is ready\b"
        ),
    ),
)
TRACING_LINE_PATTERN = re.compile(
    r"^(?P<timestamp>\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(?:\.\d+)?Z).*?(?P<message>.+)$",
    re.MULTILINE,
)


class BenchmarkError(Exception):
    """A configuration or measurement prerequisite was invalid."""


def emit(line: str) -> None:
    """Write one benchmark record promptly when stdout is captured by a runner."""

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


def require_executable(variable: str, default: Path) -> Path:
    value = os.environ.get(variable)
    path = Path(value) if value else default
    if not path.is_file() or not os.access(path, os.X_OK):
        raise BenchmarkError(f"{variable} must name an executable file: {path}")
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


def nonnegative_seconds(variable: str, default: float) -> float:
    """Read a finite attachment delay while allowing an explicit zero delay."""

    value = os.environ.get(variable, str(default))
    try:
        parsed = float(value)
    except ValueError as error:
        raise BenchmarkError(f"{variable} must be a non-negative number of seconds: {value}") from error
    if not math.isfinite(parsed) or parsed < 0:
        raise BenchmarkError(f"{variable} must be a non-negative number of seconds: {value}")
    return parsed


def prepared_world_profile_from_environment() -> PreparedWorldProfile | None:
    """Select the optional external-world profile without accepting a partial one."""

    config_value = os.environ.get(PREPARED_WORLD_VARIABLE, "")
    service = os.environ.get(ATTACH_SERVICE_VARIABLE, "")
    settle_value = os.environ.get(ATTACH_SETTLE_SECONDS_VARIABLE)
    if not config_value:
        if service or settle_value is not None:
            raise BenchmarkError(
                f"{PREPARED_WORLD_VARIABLE} is required when configuring prepared-world attachment"
            )
        return None
    config = Path(config_value)
    if not config.is_file():
        raise BenchmarkError(f"{PREPARED_WORLD_VARIABLE} must name a regular file: {config_value}")
    if not service or service != service.strip() or any(character.isspace() for character in service):
        raise BenchmarkError(
            f"{ATTACH_SERVICE_VARIABLE} must name one non-whitespace declared service"
        )
    return PreparedWorldProfile(
        config=config,
        service=service,
        attach_settle_seconds=nonnegative_seconds(
            ATTACH_SETTLE_SECONDS_VARIABLE, DEFAULT_ATTACH_SETTLE_SECONDS
        ),
    )


def parse_concurrency_levels(value: str) -> list[int]:
    """Parse one bounded, duplicate-free cold-start scaling matrix."""

    if not value.strip():
        raise BenchmarkError("SMOLWORLD_TRANSITION_CONCURRENCY must not be empty")
    levels: list[int] = []
    for raw in value.split(","):
        try:
            level = int(raw.strip())
        except ValueError as error:
            raise BenchmarkError(
                "SMOLWORLD_TRANSITION_CONCURRENCY must be comma-separated positive integers"
            ) from error
        if not 1 <= level <= MAX_CONCURRENCY:
            raise BenchmarkError(
                f"SMOLWORLD_TRANSITION_CONCURRENCY values must be between 1 and {MAX_CONCURRENCY}"
            )
        if level in levels:
            raise BenchmarkError(
                f"SMOLWORLD_TRANSITION_CONCURRENCY repeats concurrency level {level}"
            )
        levels.append(level)
    return sorted(levels)


def milliseconds(started_ns: int, finished_ns: int) -> float:
    return (finished_ns - started_ns) / 1_000_000


def tsv_field(value: object) -> str:
    """Keep diagnostic records to one TSV row without discarding their cause."""

    return " ".join(str(value).split())


def percentile(values: Sequence[float], quantile: float) -> float:
    """Return nearest-rank percentile so small benchmark samples stay obvious."""

    if not values:
        raise BenchmarkError("cannot summarize an empty timing sample")
    if not 0 < quantile <= 1:
        raise BenchmarkError(f"invalid percentile {quantile}")
    ordered = sorted(values)
    return ordered[math.ceil(len(ordered) * quantile) - 1]


@dataclass(frozen=True)
class TimingSample:
    profile: str
    mode: str
    iteration: int
    concurrency: int
    phase: str
    machine: str
    wall_ms: float


@dataclass(frozen=True)
class WaveSample:
    profile: str
    mode: str
    iteration: int
    concurrency: int
    phase: str
    wall_ms: float


@dataclass(frozen=True)
class WaveResult:
    started_ns: int
    finished_ns: int
    timings: list[tuple[str, int, int]]


@dataclass(frozen=True)
class PreparedWorldProfile:
    """One external fixture and declared service attachment to measure."""

    config: Path
    service: str
    attach_settle_seconds: float


@dataclass(frozen=True)
class TraceSample:
    profile: str
    mode: str
    iteration: int
    concurrency: int
    stage: str
    machine: str
    elapsed_ms: float


def trace_enabled() -> bool:
    """Enable only the explicit boot trace; reject ambiguous environment values."""

    value = os.environ.get(TRACE_ENVIRONMENT_VARIABLE, "0")
    if value in {"", "0"}:
        return False
    if value == "1":
        return True
    raise BenchmarkError(f"{TRACE_ENVIRONMENT_VARIABLE} must be 0 or 1")


def configure_trace_environment() -> bool:
    """Request upstream diagnostic spans without changing normal benchmark output."""

    if not trace_enabled():
        return False
    existing_filter = os.environ.get("RUST_LOG", "")
    trace_filter = "smolvm::agent=debug"
    if trace_filter not in existing_filter.split(","):
        os.environ["RUST_LOG"] = ",".join(
            part for part in (existing_filter, trace_filter) if part
        )
    # The boot helper normally redirects its diagnostics to the per-VM startup
    # log. Surface them only for this opt-in measurement run so the parent
    # command can retain the helper's process and libkrun sub-stages.
    os.environ.setdefault("SMOLVM_BOOT_DEBUG", "1")
    return True


def trace_stage_name(scope: str, label: str) -> str:
    """Convert one stable helper label into a TSV-safe stage name."""

    normalized = re.sub(r"[^a-z0-9]+", "_", label.lower()).strip("_")
    return f"{scope}_{normalized}"


def parse_startup_trace(stderr: str) -> dict[str, float]:
    """Read structured parent and boot-helper timing without inferring a boundary."""

    stages: dict[str, float] = {}
    for scope, label, elapsed in BOOT_TIMING_PATTERN.findall(stderr):
        stages[trace_stage_name(scope, label)] = float(elapsed)
    for stage, pattern in PARENT_BOOT_PATTERNS:
        match = pattern.search(stderr)
        if match is not None:
            value = next((capture for capture in match.groups() if capture is not None), None)
            if value is not None:
                stages[stage] = float(value)
    trace_times: dict[str, datetime] = {}
    for match in TRACING_LINE_PATTERN.finditer(stderr):
        timestamp = datetime.fromisoformat(match.group("timestamp").replace("Z", "+00:00"))
        message = match.group("message")
        if "agent ready" in message:
            trace_times.setdefault("agent_ready", timestamp)
        elif "detached start progress extracting local image layers" in message:
            trace_times.setdefault("layer_materialization", timestamp)
        elif "detached start progress preparing persistent overlay" in message:
            trace_times.setdefault("persistent_overlay", timestamp)
        elif "detached start progress starting detached container" in message:
            trace_times.setdefault("workload_start", timestamp)

    def elapsed(stage: str, started: str, finished: str) -> None:
        if started in trace_times and finished in trace_times:
            stages[stage] = (
                trace_times[finished] - trace_times[started]
            ).total_seconds() * 1000

    elapsed("agent_ready_to_layer_materialization", "agent_ready", "layer_materialization")
    elapsed("layer_materialization_to_overlay", "layer_materialization", "persistent_overlay")
    elapsed("agent_ready_to_workload_start", "agent_ready", "workload_start")
    return stages


def prepared_world_lifecycle_event(line: str) -> tuple[str, str] | None:
    """Read one stable supervisor lifecycle boundary without parsing smolvm output."""

    for phase, prefix in (
        ("machine_created", "smolworld: created "),
        ("machine_started", "smolworld: started "),
        ("nic_attach", "smolworld: attached "),
    ):
        if line.startswith(prefix):
            service = line.removeprefix(prefix)
            if service and service == service.strip() and not any(char.isspace() for char in service):
                return phase, service
    return None


def summarize_samples(
    samples: Sequence[TimingSample],
) -> list[tuple[str, str, int, str, int, float, float]]:
    """Group per-machine data into stable p50/p95 rows for the final TSV block."""

    grouped: dict[tuple[str, str, int, str], list[float]] = defaultdict(list)
    for sample in samples:
        grouped[(sample.profile, sample.mode, sample.concurrency, sample.phase)].append(
            sample.wall_ms
        )
    return [
        (
            profile,
            mode,
            concurrency,
            phase,
            len(values),
            statistics.median(values),
            percentile(values, 0.95),
        )
        for (profile, mode, concurrency, phase), values in sorted(grouped.items())
    ]


def summarize_waves(
    waves: Sequence[WaveSample],
) -> list[tuple[str, str, int, str, int, float, float]]:
    """Group end-to-end barriers, including the world-ready milestone."""

    grouped: dict[tuple[str, str, int, str], list[float]] = defaultdict(list)
    for wave in waves:
        grouped[(wave.profile, wave.mode, wave.concurrency, wave.phase)].append(wave.wall_ms)
    return [
        (
            profile,
            mode,
            concurrency,
            phase,
            len(values),
            statistics.median(values),
            percentile(values, 0.95),
        )
        for (profile, mode, concurrency, phase), values in sorted(grouped.items())
    ]


def summarize_traces(
    samples: Sequence[TraceSample],
) -> list[tuple[str, str, int, str, int, float, float]]:
    """Group opt-in upstream spans separately from external wall-clock samples."""

    grouped: dict[tuple[str, str, int, str], list[float]] = defaultdict(list)
    for sample in samples:
        grouped[(sample.profile, sample.mode, sample.concurrency, sample.stage)].append(
            sample.elapsed_ms
        )
    return [
        (
            profile,
            mode,
            concurrency,
            stage,
            len(values),
            statistics.median(values),
            percentile(values, 0.95),
        )
        for (profile, mode, concurrency, stage), values in sorted(grouped.items())
    ]


class SmolvmBenchmark:
    def __init__(self, smolvm_bin: Path, archive: Path, runtime_root: Path) -> None:
        self.smolvm_bin = smolvm_bin
        self.archive = archive
        self.runtime_root = runtime_root
        self.owned_names: list[str] = []
        self.startup_traces: dict[str, dict[str, float]] = {}

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
            detail = completed.stderr.strip()
            raise BenchmarkError(
                f"smolvm command failed with exit {completed.returncode}: {rendered}"
                + (f": {detail}" if detail else "")
            )
        return completed

    def reserve_names(self, names: Sequence[str]) -> None:
        """Check all exact identities before a concurrent create wave begins."""

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
        repeated = len(set(names)) != len(names)
        conflicts = sorted(set(names).intersection(observed))
        if repeated or conflicts:
            detail = "repeats benchmark names" if repeated else f"found existing names {conflicts}"
            raise BenchmarkError(f"refusing to reuse machine identities: {detail}")
        # Register before starting work so cleanup covers a command that makes a
        # record and then fails before its caller returns.
        self.owned_names.extend(names)

    def delete_names(self, names: Sequence[str]) -> None:
        failures: list[str] = []
        for name in reversed(names):
            try:
                self.command(["machine", "delete", "--name", name, "-f"])
            except BenchmarkError as error:
                if self.machine_is_absent(name):
                    self.owned_names.remove(name)
                    continue
                failures.append(str(error))
                continue
            if name in self.owned_names:
                self.owned_names.remove(name)
        if failures:
            raise BenchmarkError("benchmark cleanup failed: " + "; ".join(failures))

    def machine_is_absent(self, name: str) -> bool:
        """Confirm one reserved identity is absent before ignoring a failed delete."""

        completed = subprocess.run(
            [str(self.smolvm_bin), "machine", "status", "--name", name],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=False,
        )
        if completed.returncode == 0:
            return False
        if "vm not found" in completed.stderr.lower():
            return True
        raise BenchmarkError(
            f"cannot confirm cleanup state for machine '{name}': "
            f"smolvm machine status exited with {completed.returncode}: "
            f"{completed.stderr.strip()}"
        )

    def cleanup(self) -> None:
        try:
            self.delete_names(self.owned_names[:])
        finally:
            shutil.rmtree(self.runtime_root)
        if self.runtime_root.exists():
            raise BenchmarkError(
                f"benchmark cleanup left its private runtime root: {self.runtime_root}"
            )

    def create_machine(self, name: str, image: Path | None) -> None:
        arguments = [
            "machine",
            "create",
            "--name",
            name,
            "--cpus",
            "1",
            "--mem",
            "256",
            "--storage",
            "2",
            "--overlay",
            "1",
        ]
        if image is not None:
            arguments.extend(["--image", str(image)])
        arguments.extend(["--", "/bin/sh", "-c", "exec sleep infinity"])
        self.command(arguments)

    def start_machine(self, name: str, forkable: bool = False) -> None:
        arguments = ["machine", "start", "--name", name]
        if forkable:
            arguments.append("--forkable")
        completed = self.command(arguments)
        self.startup_traces[name] = parse_startup_trace(completed.stderr)

    def fork(self, golden: str, clone: str) -> None:
        self.command(["machine", "fork", "--golden", golden, "--name", clone])

    def verify_guest_agent(self, name: str) -> None:
        self.command(["machine", "exec", "--name", name, "--", "test", "-x", "/bin/sh"])

    def write_mutation(self, name: str, marker: str) -> None:
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


def run_wave(
    names: Sequence[str], mode: str, action: Callable[[str], None]
) -> WaveResult:
    """Run exact machine work serially or behind one simultaneous start gate."""

    if mode not in {"serial", "parallel"}:
        raise BenchmarkError(f"unknown benchmark wave mode {mode}")
    timings: list[tuple[str, int, int]] = []
    if mode == "serial":
        wave_started = time.monotonic_ns()
        for name in names:
            started = time.monotonic_ns()
            action(name)
            timings.append((name, started, time.monotonic_ns()))
        return WaveResult(wave_started, time.monotonic_ns(), timings)

    barrier = threading.Barrier(len(names) + 1)
    lock = threading.Lock()
    errors: dict[str, BaseException] = {}

    def worker(name: str) -> None:
        try:
            barrier.wait()
            started = time.monotonic_ns()
            action(name)
            finished = time.monotonic_ns()
            with lock:
                timings.append((name, started, finished))
        except BaseException as error:  # preserve cleanup after a subprocess failure
            with lock:
                errors[name] = error

    workers = [threading.Thread(target=worker, args=(name,)) for name in names]
    for worker_thread in workers:
        worker_thread.start()
    barrier.wait()
    wave_started = time.monotonic_ns()
    for worker_thread in workers:
        worker_thread.join()
    finished = time.monotonic_ns()
    if errors:
        name = next(name for name in names if name in errors)
        raise BenchmarkError(f"parallel machine '{name}' failed: {errors[name]}")
    return WaveResult(wave_started, finished, sorted(timings))


def emit_wave(
    samples: list[TimingSample],
    waves: list[WaveSample],
    profile: str,
    mode: str,
    iteration: int,
    concurrency: int,
    phase: str,
    runtime_root: Path,
    action: Callable[[], WaveResult],
) -> WaveResult:
    result = action()
    for name, started_ns, finished_ns in result.timings:
        sample = TimingSample(
            profile, mode, iteration, concurrency, phase, name, milliseconds(started_ns, finished_ns)
        )
        samples.append(sample)
        emit(
            f"machine_sample\t{sample.profile}\t{sample.mode}\t{sample.iteration}\t"
            f"{sample.concurrency}\t{sample.phase}\t{sample.machine}\t{sample.wall_ms:.3f}"
        )
    wave = WaveSample(
        profile,
        mode,
        iteration,
        concurrency,
        phase,
        milliseconds(result.started_ns, result.finished_ns),
    )
    waves.append(wave)
    emit(
        f"wave_sample\t{wave.profile}\t{wave.mode}\t{wave.iteration}\t{wave.concurrency}\t"
        f"{wave.phase}\t{wave.wall_ms:.3f}"
    )
    return result


def emit_startup_traces(
    traces: list[TraceSample],
    benchmark: SmolvmBenchmark,
    names: Sequence[str],
    profile: str,
    mode: str,
    iteration: int,
    concurrency: int,
) -> None:
    """Emit one opt-in stage value per direct machine start, when available."""

    for name in names:
        for stage, elapsed_ms in sorted(benchmark.startup_traces.get(name, {}).items()):
            sample = TraceSample(
                profile,
                mode,
                iteration,
                concurrency,
                stage,
                name,
                elapsed_ms,
            )
            traces.append(sample)
            emit(
                f"trace_sample\t{sample.profile}\t{sample.mode}\t{sample.iteration}\t"
                f"{sample.concurrency}\t{sample.stage}\t{sample.machine}\t"
                f"{sample.elapsed_ms:.3f}"
            )


def scenario_names(prefix: str, count: int) -> list[str]:
    return [f"{prefix}-{index}" for index in range(1, count + 1)]


def fnv1a(value: bytes) -> str:
    """Match smolworld's stable runtime/state directory namespace hash."""

    result = 0xCBF29CE484222325
    for byte in value:
        result ^= byte
        result = (result * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return f"{result:012x}"


def generated_world_state_dir(config: Path) -> Path:
    return Path.home() / ".smolworld" / f"world-{fnv1a(os.fsencode(config.resolve()))}"


def write_world_fixture(root: Path, archive: Path, iteration: int, count: int) -> tuple[Path, list[str]]:
    """Write only the temporary, image-backed world material for one live probe."""

    services = [f"machine-{index}" for index in range(1, count + 1)]
    world_name = f"transition-{os.getpid()}-{secrets.token_hex(4)}"
    subnet_octet = (iteration % 250) + 1
    config = root / ".smolworld"
    lines = [
        "format: 2",
        "",
        "world:",
        f"  name: {world_name}",
        "",
        "network:",
        f"  subnet: 10.253.{subnet_octet}.0/24",
        f"  domain: {world_name}.test",
        "",
        "machines:",
    ]
    for service in services:
        smolfile = root / f"{service}.Smolfile"
        smolfile.write_text(
            "\n".join(
                [
                    f'image = "{archive}"',
                    'entrypoint = ["/bin/sh", "-c"]',
                    'cmd = ["exec sleep infinity"]',
                    "cpus = 1",
                    "memory = 256",
                    "storage = 2",
                    "overlay = 1",
                    "",
                ]
            ),
            encoding="utf-8",
        )
        lines.extend([f"  {service}:", f"    smolfile: ./{smolfile.name}"])
    config.write_text("\n".join(lines) + "\n", encoding="utf-8")
    return config, services


def command_environment(smolvm_bin: Path) -> dict[str, str]:
    environment = dict(os.environ)
    environment["SMOLWORLD_SMOLVM"] = str(smolvm_bin)
    return environment


def smolworld_command(
    smolworld_bin: Path,
    config: Path,
    arguments: Sequence[str],
    environment: dict[str, str],
) -> subprocess.CompletedProcess[str]:
    """Run one bounded CLI observation or operation against an exact world."""

    return subprocess.run(
        [str(smolworld_bin), "--file", str(config), *arguments],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=environment,
        check=False,
    )


def parse_ps_json_rows(output: str) -> list[dict[str, str]]:
    """Validate the closed `ps --format json` rows before using their status."""

    rows: list[dict[str, str]] = []
    for line in output.splitlines():
        if not line.strip():
            continue
        try:
            row = json.loads(line)
        except json.JSONDecodeError as error:
            raise BenchmarkError(f"smolworld ps emitted invalid JSON: {error}") from error
        if (
            not isinstance(row, dict)
            or set(row) != {"service", "ip", "mac", "status"}
            or not all(isinstance(value, str) for value in row.values())
        ):
            raise BenchmarkError(f"smolworld ps emitted an invalid closed row: {row!r}")
        rows.append(row)
    if not rows:
        raise BenchmarkError("smolworld ps emitted no JSON rows")
    return rows


def prepared_world_is_idle(output: str) -> bool:
    """Require the external fixture to have no allocated lifecycle to adopt."""

    return all(row["status"] == "absent" for row in parse_ps_json_rows(output))


def service_is_running_in_ps_json(output: str, service: str) -> bool:
    """Read one exact declared-service lifecycle observation, not readiness."""

    rows = parse_ps_json_rows(output)
    if len(rows) != 1 or rows[0]["service"] != service:
        raise BenchmarkError(
            f"smolworld ps did not return one row for declared service {service!r}: {rows!r}"
        )
    return rows[0]["status"] == "running"


def emit_timing_sample(
    samples: list[TimingSample],
    profile: str,
    mode: str,
    iteration: int,
    concurrency: int,
    phase: str,
    machine: str,
    started_ns: int,
    finished_ns: int,
) -> None:
    sample = TimingSample(
        profile,
        mode,
        iteration,
        concurrency,
        phase,
        machine,
        milliseconds(started_ns, finished_ns),
    )
    samples.append(sample)
    emit(
        f"machine_sample\t{sample.profile}\t{sample.mode}\t{sample.iteration}\t"
        f"{sample.concurrency}\t{sample.phase}\t{sample.machine}\t{sample.wall_ms:.3f}"
    )


def emit_wave_sample(
    waves: list[WaveSample],
    profile: str,
    mode: str,
    iteration: int,
    concurrency: int,
    phase: str,
    started_ns: int,
    finished_ns: int,
) -> None:
    wave = WaveSample(
        profile,
        mode,
        iteration,
        concurrency,
        phase,
        milliseconds(started_ns, finished_ns),
    )
    waves.append(wave)
    emit(
        f"wave_sample\t{wave.profile}\t{wave.mode}\t{wave.iteration}\t{wave.concurrency}\t"
        f"{wave.phase}\t{wave.wall_ms:.3f}"
    )


def recorded_world_machine_names(state_dir: Path) -> list[str]:
    """Read only exact benchmark identities from its generated allocation state."""

    state_file = state_dir / "state"
    try:
        lines = state_file.read_text(encoding="utf-8").splitlines()
    except OSError as error:
        raise BenchmarkError(f"read benchmark world allocation {state_file}: {error}") from error
    names: list[str] = []
    for line in lines:
        fields = line.split("\t")
        if not fields or fields[0] != "machine":
            continue
        if len(fields) != 5 or not re.fullmatch(r"smw-[0-9a-f]+-[0-9a-f]+", fields[4]):
            raise BenchmarkError(
                f"benchmark world allocation contains an invalid machine record: {line!r}"
            )
        names.append(fields[4])
    if len(names) != len(set(names)):
        raise BenchmarkError("benchmark world allocation repeats a machine identity")
    return names


def cleanup_recorded_benchmark_world_machines(smolvm_bin: Path, state_dir: Path) -> None:
    """Finish exact generated-world cleanup after its supervisor failed early."""

    benchmark = SmolvmBenchmark(smolvm_bin, Path("/unused-archive"), Path("/unused-root"))
    names = recorded_world_machine_names(state_dir)
    benchmark.owned_names = names[:]
    benchmark.delete_names(names)


def run_world_probe(
    smolworld_bin: Path,
    smolvm_bin: Path,
    archive: Path,
    iteration: int,
    concurrency: int,
    samples: list[TimingSample],
    waves: list[WaveSample],
) -> None:
    """Measure the real switch attachment path without reimplementing L2 in Python."""

    root = Path(tempfile.mkdtemp(prefix=f"smw-world-transition-{secrets.token_hex(6)}.", dir="/tmp"))
    config, services = write_world_fixture(root, archive, iteration, concurrency)
    state_dir = generated_world_state_dir(config)
    if state_dir.exists():
        raise BenchmarkError(f"refusing to reuse existing benchmark world state {state_dir}")
    environment = command_environment(smolvm_bin)
    process: subprocess.Popen[str] | None = None
    readers: list[threading.Thread] = []
    phase = "prepare"
    failure: BenchmarkError | None = None
    try:
        prepare_started = time.monotonic_ns()
        prepared = subprocess.run(
            [str(smolworld_bin), "--file", str(config), "prepare"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=environment,
            check=False,
        )
        prepare_finished = time.monotonic_ns()
        if prepared.returncode != 0:
            raise BenchmarkError(f"smolworld prepare failed: {prepared.stderr.strip()}")
        prepare_sample = TimingSample(
            "world", "parallel", iteration, concurrency, "prepare", "world",
            milliseconds(prepare_started, prepare_finished),
        )
        samples.append(prepare_sample)
        emit(
            f"machine_sample\t{prepare_sample.profile}\t{prepare_sample.mode}\t"
            f"{prepare_sample.iteration}\t{prepare_sample.concurrency}\t{prepare_sample.phase}\t"
            f"{prepare_sample.machine}\t{prepare_sample.wall_ms:.3f}"
        )

        phase = "check"
        check_started = time.monotonic_ns()
        checked = subprocess.run(
            [str(smolworld_bin), "--file", str(config), "check"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=environment,
            check=False,
        )
        check_finished = time.monotonic_ns()
        if checked.returncode != 0:
            raise BenchmarkError(f"smolworld check failed: {checked.stderr.strip()}")
        check_sample = TimingSample(
            "world", "parallel", iteration, concurrency, "check", "world",
            milliseconds(check_started, check_finished),
        )
        samples.append(check_sample)
        emit(
            f"machine_sample\t{check_sample.profile}\t{check_sample.mode}\t"
            f"{check_sample.iteration}\t{check_sample.concurrency}\t{check_sample.phase}\t"
            f"{check_sample.machine}\t{check_sample.wall_ms:.3f}"
        )

        phase = "up"
        process = subprocess.Popen(
            [str(smolworld_bin), "--file", str(config), "up"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            env=environment,
        )
        events: list[tuple[int, str]] = []
        events_lock = threading.Lock()

        def collect(stream: object) -> None:
            assert hasattr(stream, "readline")
            while True:
                line = stream.readline()
                if not line:
                    return
                with events_lock:
                    events.append((time.monotonic_ns(), line.rstrip("\n")))

        readers = [
            threading.Thread(target=collect, args=(process.stdout,), daemon=True),
            threading.Thread(target=collect, args=(process.stderr,), daemon=True),
        ]
        for reader in readers:
            reader.start()
        started = time.monotonic_ns()
        deadline = time.monotonic() + WORLD_READY_TIMEOUT_SECONDS
        created: dict[str, int] = {}
        started_machines: dict[str, int] = {}
        attachments: dict[str, int] = {}
        ready_ns: int | None = None
        while time.monotonic() < deadline and ready_ns is None:
            with events_lock:
                for observed_ns, line in events:
                    lifecycle_event = prepared_world_lifecycle_event(line)
                    if lifecycle_event is not None:
                        lifecycle_phase, service = lifecycle_event
                        if lifecycle_phase == "machine_created":
                            created.setdefault(service, observed_ns)
                        elif lifecycle_phase == "machine_started":
                            started_machines.setdefault(service, observed_ns)
                        else:
                            attachments.setdefault(service, observed_ns)
                    if line == "smolworld: world is up; press Ctrl-C to stop it":
                        ready_ns = observed_ns
            if process.poll() is not None and ready_ns is None:
                break
            time.sleep(0.001)
        expected_services = set(services)
        if (
            ready_ns is None
            or set(created) != expected_services
            or set(started_machines) != expected_services
            or set(attachments) != expected_services
        ):
            with events_lock:
                rendered = "\n".join(line for _timestamp, line in events)
            raise BenchmarkError(
                f"smolworld up did not reach all create/start/attachment boundaries and ready state: {rendered}"
            )
        for service in services:
            sample = TimingSample(
                "world", "parallel", iteration, concurrency, "nic_attach", service,
                milliseconds(started, attachments[service]),
            )
            samples.append(sample)
            emit(
                f"machine_sample\t{sample.profile}\t{sample.mode}\t{sample.iteration}\t"
                f"{sample.concurrency}\t{sample.phase}\t{sample.machine}\t{sample.wall_ms:.3f}"
            )
        ready = WaveSample(
            "world", "parallel", iteration, concurrency, "world_ready",
            milliseconds(started, ready_ns),
        )
        waves.append(ready)
        emit(
            f"wave_sample\t{ready.profile}\t{ready.mode}\t{ready.iteration}\t"
            f"{ready.concurrency}\t{ready.phase}\t{ready.wall_ms:.3f}"
        )
    except BenchmarkError as error:
        failure = error
    finally:
        supervisor_cleaned = False
        if process is not None and process.poll() is None:
            process.send_signal(signal.SIGINT)
            try:
                process.wait(timeout=60)
                supervisor_cleaned = process.returncode == 0
            except subprocess.TimeoutExpired as error:
                process.kill()
                process.wait()
                raise BenchmarkError("smolworld supervisor did not cleanly stop") from error
        elif process is not None:
            supervisor_cleaned = process.returncode == 0
        for reader in readers:
            reader.join(timeout=1)
        if not supervisor_cleaned and state_dir.exists():
            down = subprocess.run(
                [str(smolworld_bin), "--file", str(config), "down"],
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                env=environment,
                check=False,
            )
            if down.returncode != 0 and state_dir.exists():
                cleanup_recorded_benchmark_world_machines(smolvm_bin, state_dir)
        if state_dir.exists():
            shutil.rmtree(state_dir)
        shutil.rmtree(root)
    if failure is not None:
        emit(
            f"failure_sample\tworld\tparallel\t{iteration}\t{concurrency}\t{phase}\t"
            f"{tsv_field(failure)}"
        )


def run_prepared_world_profile(
    smolworld_bin: Path,
    smolvm_bin: Path,
    prepared: PreparedWorldProfile,
    iteration: int,
    samples: list[TimingSample],
    waves: list[WaveSample],
) -> None:
    """Measure a sealed world's declared-service command attachment boundary.

    This profile intentionally treats the configured world as an external
    fixture. It verifies material with read-only commands, refuses to adopt
    non-absent lifecycle state, and never deletes the fixture's state or
    material. Once `up` has started, only the owning supervisor (or its exact
    control-path `down`) is allowed to clean the world.
    """

    profile = "prepared_world"
    mode = "declared_service"
    concurrency = 1
    config = prepared.config
    environment = command_environment(smolvm_bin)
    process: subprocess.Popen[str] | None = None
    readers: list[threading.Thread] = []
    phase = "config"
    failure: BenchmarkError | None = None
    cleanup_failure: BenchmarkError | None = None
    started_supervisor = False
    try:
        config_started = time.monotonic_ns()
        rendered = smolworld_command(smolworld_bin, config, ["config", "--quiet"], environment)
        config_finished = time.monotonic_ns()
        if rendered.returncode != 0:
            raise BenchmarkError(f"smolworld config failed: {rendered.stderr.strip()}")
        emit_timing_sample(
            samples,
            profile,
            mode,
            iteration,
            concurrency,
            "config",
            "world",
            config_started,
            config_finished,
        )

        phase = "check"
        check_started = time.monotonic_ns()
        checked = smolworld_command(smolworld_bin, config, ["check"], environment)
        check_finished = time.monotonic_ns()
        if checked.returncode != 0:
            raise BenchmarkError(f"smolworld check failed: {checked.stderr.strip()}")
        emit_timing_sample(
            samples,
            profile,
            mode,
            iteration,
            concurrency,
            "check",
            "world",
            check_started,
            check_finished,
        )

        phase = "idle_preflight"
        idle = smolworld_command(smolworld_bin, config, ["ps", "--all", "--format", "json"], environment)
        if idle.returncode != 0:
            raise BenchmarkError(f"smolworld ps preflight failed: {idle.stderr.strip()}")
        if not prepared_world_is_idle(idle.stdout):
            raise BenchmarkError("prepared-world attachment requires every declared service to be absent")

        phase = "up"
        process = subprocess.Popen(
            [str(smolworld_bin), "--file", str(config), "up"],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            env=environment,
        )
        started_supervisor = True
        events: list[tuple[int, str]] = []
        events_lock = threading.Lock()

        def collect(stream: object) -> None:
            assert hasattr(stream, "readline")
            while True:
                line = stream.readline()
                if not line:
                    return
                with events_lock:
                    events.append((time.monotonic_ns(), line.rstrip("\n")))

        readers = [
            threading.Thread(target=collect, args=(process.stdout,), daemon=True),
            threading.Thread(target=collect, args=(process.stderr,), daemon=True),
        ]
        for reader in readers:
            reader.start()
        started = time.monotonic_ns()
        deadline = time.monotonic() + WORLD_READY_TIMEOUT_SECONDS
        created: dict[str, int] = {}
        started_machines: dict[str, int] = {}
        attachments: dict[str, int] = {}
        ready_ns: int | None = None
        visible_ns: int | None = None
        attached_command_ns: int | None = None
        last_ps_error = ""
        last_command_error = ""
        while time.monotonic() < deadline:
            with events_lock:
                for observed_ns, line in events:
                    lifecycle_event = prepared_world_lifecycle_event(line)
                    if lifecycle_event is not None:
                        lifecycle_phase, service = lifecycle_event
                        if lifecycle_phase == "machine_created":
                            created.setdefault(service, observed_ns)
                        elif lifecycle_phase == "machine_started":
                            started_machines.setdefault(service, observed_ns)
                        else:
                            attachments.setdefault(service, observed_ns)
                    if line == "smolworld: world is up; press Ctrl-C to stop it":
                        ready_ns = observed_ns
            if visible_ns is None:
                observed = smolworld_command(
                    smolworld_bin,
                    config,
                    ["ps", "--format", "json", prepared.service],
                    environment,
                )
                observed_finished = time.monotonic_ns()
                if observed.returncode == 0:
                    if service_is_running_in_ps_json(observed.stdout, prepared.service):
                        visible_ns = observed_finished
                        emit_timing_sample(
                            samples,
                            profile,
                            mode,
                            iteration,
                            concurrency,
                            "host_visible",
                            prepared.service,
                            started,
                            visible_ns,
                        )
                else:
                    last_ps_error = observed.stderr.strip()
            if (
                visible_ns is not None
                and attached_command_ns is None
                and time.monotonic_ns() - visible_ns
                >= int(prepared.attach_settle_seconds * 1_000_000_000)
            ):
                command_started = time.monotonic_ns()
                attached = smolworld_command(
                    smolworld_bin,
                    config,
                    ["exec", prepared.service, "--", "/bin/true"],
                    environment,
                )
                command_finished = time.monotonic_ns()
                if attached.returncode == 0:
                    attached_command_ns = command_finished
                    emit_timing_sample(
                        samples,
                        profile,
                        mode,
                        iteration,
                        concurrency,
                        "command_attach",
                        prepared.service,
                        command_started,
                        command_finished,
                    )
                    emit_timing_sample(
                        samples,
                        profile,
                        mode,
                        iteration,
                        concurrency,
                        "attached_command",
                        prepared.service,
                        started,
                        attached_command_ns,
                    )
                else:
                    last_command_error = attached.stderr.strip()
            if ready_ns is not None and attached_command_ns is not None:
                break
            if process.poll() is not None:
                break
            time.sleep(0.2)
        expected_services = {json.loads(line)["service"] for line in idle.stdout.splitlines() if line}
        if (
            ready_ns is None
            or attached_command_ns is None
            or set(created) != expected_services
            or set(started_machines) != expected_services
            or set(attachments) != expected_services
        ):
            with events_lock:
                event_output = "\n".join(line for _observed_ns, line in events)
            detail = "; ".join(
                value
                for value in (last_ps_error, last_command_error, event_output)
                if value
            )
            raise BenchmarkError(
                "smolworld up did not reach all create/start/attachment, world-ready, and "
                "declared-service command-attachment boundaries"
                + (f": {detail}" if detail else "")
            )
        for service in sorted(expected_services):
            created_ns = created[service]
            machine_started_ns = started_machines[service]
            attached_ns = attachments[service]
            emit_timing_sample(
                samples,
                profile,
                mode,
                iteration,
                concurrency,
                "machine_created",
                service,
                started,
                created_ns,
            )
            emit_timing_sample(
                samples,
                profile,
                mode,
                iteration,
                concurrency,
                "machine_started",
                service,
                started,
                machine_started_ns,
            )
            emit_timing_sample(
                samples,
                profile,
                mode,
                iteration,
                concurrency,
                "created_to_started",
                service,
                created_ns,
                machine_started_ns,
            )
            emit_timing_sample(
                samples,
                profile,
                mode,
                iteration,
                concurrency,
                "nic_attach",
                service,
                started,
                attached_ns,
            )
            emit_timing_sample(
                samples,
                profile,
                mode,
                iteration,
                concurrency,
                "started_to_nic_attach",
                service,
                machine_started_ns,
                attached_ns,
            )
        emit_wave_sample(
            waves,
            profile,
            mode,
            iteration,
            concurrency,
            "world_ready",
            started,
            ready_ns,
        )
    except BenchmarkError as error:
        failure = error
    finally:
        supervisor_cleaned = False
        if process is not None and process.poll() is None:
            process.send_signal(signal.SIGINT)
            try:
                process.wait(timeout=60)
                supervisor_cleaned = process.returncode == 0
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
                cleanup_failure = BenchmarkError("smolworld supervisor did not cleanly stop")
        elif process is not None:
            supervisor_cleaned = process.returncode == 0
        for reader in readers:
            reader.join(timeout=1)
        if started_supervisor and not supervisor_cleaned:
            post_exit = smolworld_command(
                smolworld_bin, config, ["ps", "--all", "--format", "json"], environment
            )
            try:
                already_idle = post_exit.returncode == 0 and prepared_world_is_idle(post_exit.stdout)
            except BenchmarkError:
                already_idle = False
            if not already_idle:
                down = smolworld_command(smolworld_bin, config, ["down"], environment)
                if down.returncode != 0:
                    cleanup_failure = BenchmarkError(
                        "smolworld supervisor did not cleanly stop and exact down failed: "
                        f"{down.stderr.strip()}"
                    )
        if started_supervisor and cleanup_failure is None:
            idle = smolworld_command(
                smolworld_bin, config, ["ps", "--all", "--format", "json"], environment
            )
            try:
                idle_after_cleanup = idle.returncode == 0 and prepared_world_is_idle(idle.stdout)
            except BenchmarkError as error:
                cleanup_failure = BenchmarkError(
                    "prepared-world attachment cleanup could not read closed lifecycle rows: "
                    f"{error}"
                )
                idle_after_cleanup = False
            if cleanup_failure is None and not idle_after_cleanup:
                cleanup_failure = BenchmarkError(
                    "prepared-world attachment cleanup did not restore all declared services to absent"
                    + (f": {idle.stderr.strip()}" if idle.stderr.strip() else "")
                )
    if cleanup_failure is not None:
        if failure is None:
            failure = cleanup_failure
        else:
            failure = BenchmarkError(f"{failure}; cleanup also failed: {cleanup_failure}")
    if failure is not None:
        emit(
            f"failure_sample\t{profile}\t{mode}\t{iteration}\t{concurrency}\t{phase}\t"
            f"{tsv_field(failure)}"
        )
    if cleanup_failure is not None:
        raise cleanup_failure


def run_cold_scenario(
    smolvm_bin: Path,
    archive: Path,
    iteration: int,
    concurrency: int,
    profile: str,
    mode: str,
    samples: list[TimingSample],
    waves: list[WaveSample],
    traces: list[TraceSample],
) -> None:
    """Measure one private machine-runtime scenario and remove its exact identities."""

    runtime_root = Path(tempfile.mkdtemp(prefix=f"smw-transition-{secrets.token_hex(6)}.", dir="/tmp"))
    benchmark = SmolvmBenchmark(smolvm_bin, archive, runtime_root)
    names = scenario_names(
        f"smw-bench-{os.getpid()}-{iteration}-{profile[0]}-{mode[0]}-{concurrency}", concurrency
    )
    forkable = profile == "archive_forkable"
    phase = "reserve"
    failure: BenchmarkError | None = None
    try:
        benchmark.reserve_names(names)
        phase = "create"
        emit_wave(
            samples,
            waves,
            profile,
            mode,
            iteration,
            concurrency,
            "create",
            runtime_root,
            lambda: run_wave(
                names,
                mode,
                lambda name: benchmark.create_machine(name, archive),
            ),
        )
        phase = "start"
        emit_wave(
            samples,
            waves,
            profile,
            mode,
            iteration,
            concurrency,
            "start",
            runtime_root,
            lambda: run_wave(names, mode, lambda name: benchmark.start_machine(name, forkable)),
        )
        emit_startup_traces(
            traces,
            benchmark,
            names,
            profile,
            mode,
            iteration,
            concurrency,
        )
        phase = "agent_exec"
        emit_wave(
            samples,
            waves,
            profile,
            mode,
            iteration,
            concurrency,
            "agent_exec",
            runtime_root,
            lambda: run_wave(names, mode, benchmark.verify_guest_agent),
        )
        phase = "mutation"
        emit_wave(
            samples,
            waves,
            profile,
            mode,
            iteration,
            concurrency,
            "mutation",
            runtime_root,
            lambda: run_wave(
                names,
                mode,
                lambda name: benchmark.write_mutation(name, f"{profile}-{iteration}-{name}"),
            ),
        )
    except BenchmarkError as error:
        failure = error
    try:
        benchmark.cleanup()
    except BenchmarkError as cleanup_error:
        if failure is None:
            raise
        raise BenchmarkError(f"{failure}; cleanup also failed: {cleanup_error}") from cleanup_error
    if failure is not None:
        emit(
            f"failure_sample\t{profile}\t{mode}\t{iteration}\t{concurrency}\t{phase}\t"
            f"{tsv_field(failure)}"
        )


def run_fork_reference(
    smolvm_bin: Path,
    archive: Path,
    iteration: int,
    branches: int,
    samples: list[TimingSample],
    waves: list[WaveSample],
) -> None:
    """Retain a one-to-many fork reference against the cold matrix.

    A golden is paused as part of a fork request, so the upstream lifecycle
    contract permits only one request against a golden at a time.  Keep fork
    creation serial and measure clone work separately, rather than turning
    rejected concurrent requests into a misleading performance result.
    """

    runtime_root = Path(
        tempfile.mkdtemp(prefix=f"smw-fork-reference-{secrets.token_hex(6)}.", dir="/tmp")
    )
    benchmark = SmolvmBenchmark(smolvm_bin, archive, runtime_root)
    golden = f"smw-fork-{os.getpid()}-{iteration}-golden"
    clones = scenario_names(f"smw-fork-{os.getpid()}-{iteration}-clone", branches)
    try:
        benchmark.reserve_names([golden])
        emit_wave(
            samples,
            waves,
            "fork",
            "serial",
            iteration,
            1,
            "create_golden",
            runtime_root,
            lambda: run_wave(
                [golden], "serial", lambda name: benchmark.create_machine(name, archive)
            ),
        )
        emit_wave(
            samples,
            waves,
            "fork",
            "serial",
            iteration,
            1,
            "start_golden",
            runtime_root,
            lambda: run_wave([golden], "serial", lambda name: benchmark.start_machine(name, True)),
        )
        benchmark.reserve_names(clones)
        emit_wave(
            samples,
            waves,
            "fork",
            "serial",
            iteration,
            branches,
            "fork",
            runtime_root,
            lambda: run_wave(clones, "serial", lambda clone: benchmark.fork(golden, clone)),
        )
        emit_wave(
            samples,
            waves,
            "fork",
            "parallel",
            iteration,
            branches,
            "mutation",
            runtime_root,
            lambda: run_wave(
                clones,
                "parallel",
                lambda clone: benchmark.write_mutation(clone, f"fork-{iteration}-{clone}"),
            ),
        )
    finally:
        benchmark.cleanup()


def emit_summary(samples: Sequence[TimingSample]) -> None:
    for profile, mode, concurrency, phase, count, p50_ms, p95_ms in summarize_samples(samples):
        emit(
            f"summary\t{profile}\t{mode}\t{concurrency}\t{phase}\t{count}\t"
            f"{p50_ms:.3f}\t{p95_ms:.3f}"
        )

    grouped = {
        (profile, mode, concurrency, phase): p50_ms
        for profile, mode, concurrency, phase, _count, p50_ms, _p95_ms in summarize_samples(samples)
    }
    for profile in ("archive", "archive_forkable"):
        for concurrency in sorted(
            {sample.concurrency for sample in samples if sample.profile == profile}
        ):
            serial = grouped.get((profile, "serial", concurrency, "start"))
            parallel = grouped.get((profile, "parallel", concurrency, "start"))
            if serial is not None and parallel is not None:
                emit(
                    f"comparison\tparallel_over_serial_start\t{profile}\t{concurrency}\t"
                    f"{parallel - serial:.3f}"
                )


def emit_wave_summary(waves: Sequence[WaveSample]) -> None:
    """Emit p50/p95 for barriers that cannot be reduced to one machine sample."""

    emit("wave_summary\tprofile\tmode\tconcurrency\tphase\tsamples\tp50_ms\tp95_ms")
    for profile, mode, concurrency, phase, count, p50_ms, p95_ms in summarize_waves(waves):
        emit(
            f"wave_summary\t{profile}\t{mode}\t{concurrency}\t{phase}\t{count}\t"
            f"{p50_ms:.3f}\t{p95_ms:.3f}"
        )


def emit_trace_summary(traces: Sequence[TraceSample]) -> None:
    """Report nested upstream spans without mixing them into wall-time summaries."""

    emit("trace_summary\tprofile\tmode\tconcurrency\tstage\tsamples\tp50_ms\tp95_ms")
    for profile, mode, concurrency, stage, count, p50_ms, p95_ms in summarize_traces(traces):
        emit(
            f"trace_summary\t{profile}\t{mode}\t{concurrency}\t{stage}\t{count}\t"
            f"{p50_ms:.3f}\t{p95_ms:.3f}"
        )


def main() -> int:
    if os.environ.get("SMOLWORLD_TRANSITION_BENCH") != "1":
        raise BenchmarkError("set SMOLWORLD_TRANSITION_BENCH=1 to run VM-transition measurements")

    smolvm_bin = require_file("SMOLVM_BIN")
    smolworld_bin = require_executable(
        "SMOLWORLD_BIN", Path(__file__).resolve().parents[1] / "target" / "debug" / "smolworld"
    )
    agent_rootfs = require_directory("SMOLVM_AGENT_ROOTFS")
    archive_value = os.environ.get("SMOLWORLD_TRANSITION_ARCHIVE", "")
    archive = require_file("SMOLWORLD_TRANSITION_ARCHIVE") if archive_value else None
    prepared_world = prepared_world_profile_from_environment()
    if archive is None and prepared_world is None:
        raise BenchmarkError(
            "configure SMOLWORLD_TRANSITION_ARCHIVE or both "
            f"{PREPARED_WORLD_VARIABLE} and {ATTACH_SERVICE_VARIABLE}"
        )
    iterations = positive_integer("SMOLWORLD_TRANSITION_ITERATIONS", 3)
    branches = positive_integer("SMOLWORLD_TRANSITION_BRANCHES", 3)
    concurrency = parse_concurrency_levels(
        os.environ.get("SMOLWORLD_TRANSITION_CONCURRENCY", "1,2,4")
    )
    os.environ["SMOLVM_AGENT_ROOTFS"] = str(agent_rootfs)
    trace = configure_trace_environment()

    def interrupt(_signum: int, _frame: object) -> None:
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupt)
    samples: list[TimingSample] = []
    waves: list[WaveSample] = []
    traces: list[TraceSample] = []
    emit("# smolworld transition substrate benchmark v2")
    emit(f"# archive={archive if archive is not None else 'not-requested'}")
    if prepared_world is not None:
        emit(
            f"# prepared_world={prepared_world.config} attach_service={prepared_world.service} "
            f"attach_settle_seconds={prepared_world.attach_settle_seconds:g}"
        )
    direct_branches = str(branches) if archive is not None else "not-requested"
    direct_concurrency = ",".join(map(str, concurrency)) if archive is not None else "not-requested"
    emit(
        f"# iterations={iterations} branches={direct_branches} concurrency={direct_concurrency} "
        f"cpus=1 memory_mib=256 mutation_bytes={MUTATION_BYTES}"
    )
    emit("# archive cache is user-owned and is never cleared by this benchmark")
    emit(f"# upstream_boot_trace={'enabled' if trace else 'disabled'}")
    emit(
        "machine_sample\tprofile\tmode\titeration\tconcurrency\tphase\tmachine\twall_ms"
    )
    emit(
        "wave_sample\tprofile\tmode\titeration\tconcurrency\tphase\twall_ms"
    )
    if trace:
        emit(
            "trace_sample\tprofile\tmode\titeration\tconcurrency\tstage\tmachine\telapsed_ms"
        )
    emit("failure_sample\tprofile\tmode\titeration\tconcurrency\tphase\terror")

    for iteration in range(1, iterations + 1):
        if prepared_world is not None:
            run_prepared_world_profile(
                smolworld_bin,
                smolvm_bin,
                prepared_world,
                iteration,
                samples,
                waves,
            )
        if archive is not None:
            for count in concurrency:
                for mode in ("serial", "parallel"):
                    for profile in ("archive", "archive_forkable"):
                        run_cold_scenario(
                            smolvm_bin,
                            archive,
                            iteration,
                            count,
                            profile,
                            mode,
                            samples,
                            waves,
                            traces,
                        )
                run_world_probe(
                    smolworld_bin,
                    smolvm_bin,
                    archive,
                    iteration,
                    count,
                    samples,
                    waves,
                )
            run_fork_reference(smolvm_bin, archive, iteration, branches, samples, waves)

    emit("summary\tprofile\tmode\tconcurrency\tphase\tsamples\tp50_ms\tp95_ms")
    emit_summary(samples)
    emit_wave_summary(waves)
    if trace:
        emit_trace_summary(traces)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except BenchmarkError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(2)
    except KeyboardInterrupt:
        print("interrupted: cleaned exact benchmark machines and runtime roots", file=sys.stderr)
        raise SystemExit(130)
