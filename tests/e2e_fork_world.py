#!/usr/bin/env python3
"""Exercise Smolworld external-NIC fork and durable checkpoint contracts.

This is an opt-in macOS/Apple-Silicon integration gate with two mutually
exclusive modes. The live-fork mode does not benchmark a durable world
checkpoint: SmolVM freezes one forkable runner as a live CoW base and boots one
non-forkable child from its in-memory checkpoint. It proves the two
reconnections that matter for that substrate:

* the runner reconnects to Smolworld's Unix-stream NIC after being restarted as
  forkable; and
* the restored child reconnects both its agent (vsock) and that same external
  NIC, then resolves and reaches Redis through Smolworld's private switch.

The default fork gate prints the live transition wall time plus two filesystem sharing proxies:
allocated blocks addressed under its private SMOLVM_RUNTIME_ROOT and the used
bytes on that volume. The former double-counts APFS clonefile sharing; the
latter sees physical CoW sharing but also unrelated host writes. Neither
measures proportional guest-RAM use, which macOS does not expose stably.

Required environment:
    SMOLWORLD_FORK_E2E=1
    SMOLWORLD_SMOLVM=/absolute/path/to/patched/smolvm
    SMOLVM_AGENT_ROOTFS=/absolute/path/to/agent-rootfs
    SMOLVM_LIB_DIR=/absolute/path/to/matching/libkrun

Optional environment:
    SMOLWORLD_REDIS_ARCHIVE=/absolute/path/to/prepared/redis.tar

Set ``SMOLWORLD_DURABLE_E2E=1`` instead of ``SMOLWORLD_FORK_E2E=1``
to prove the coordinated two-machine checkpoint path. It writes runner
workspace and Redis state, captures both machines through the world supervisor,
waits for that supervisor to exit, restores under fresh listeners, then checks
the state and performs exact release.
"""

from __future__ import annotations

import os
import platform
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Callable, Sequence


class E2EError(Exception):
    """A test precondition or observed external contract failed."""


def emit(line: str) -> None:
    print(line, flush=True)


def required_file(variable: str) -> Path:
    value = os.environ.get(variable, "")
    path = Path(value)
    if not value or not path.is_file():
        raise E2EError(f"{variable} must name a regular file: {value}")
    return path.resolve()


def required_directory(variable: str) -> Path:
    value = os.environ.get(variable, "")
    path = Path(value)
    if not value or not path.is_dir():
        raise E2EError(f"{variable} must name a directory: {value}")
    return path.resolve()


def command_text(arguments: Sequence[str]) -> str:
    return " ".join(arguments)


def output_text(completed: subprocess.CompletedProcess[str]) -> str:
    return (completed.stdout or "") + (completed.stderr or "")


def run(
    arguments: Sequence[str],
    environment: dict[str, str],
    *,
    timeout: float = 60.0,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    try:
        completed = subprocess.run(
            list(arguments),
            env=environment,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=timeout,
            check=False,
        )
    except FileNotFoundError as error:
        raise E2EError(f"command is unavailable: {command_text(arguments)}: {error}") from error
    except subprocess.TimeoutExpired as error:
        raise E2EError(f"command timed out after {timeout:.0f}s: {command_text(arguments)}") from error
    if os.environ.get("SMOLWORLD_FORK_TRACE") == "1" and completed.stderr:
        sys.stderr.write(
            f"\n# {command_text(arguments)} stderr\n{completed.stderr}\n"
        )
    if check and completed.returncode != 0:
        raise E2EError(
            f"command exited {completed.returncode}: {command_text(arguments)}\n"
            f"{output_text(completed).strip()}"
        )
    return completed


def retry(
    description: str,
    action: Callable[[], None],
    *,
    attempts: int = 120,
    delay_seconds: float = 0.25,
) -> None:
    last_error: Exception | None = None
    for _ in range(attempts):
        try:
            action()
            return
        except (E2EError, subprocess.TimeoutExpired) as error:
            last_error = error
            time.sleep(delay_seconds)
    raise E2EError(f"timed out waiting for {description}: {last_error}")


def accounted_file_blocks_bytes(root: Path) -> int:
    """Count blocks addressed by all files in the isolated runtime root."""

    total = 0
    for directory, subdirectories, filenames in os.walk(root, followlinks=False):
        entries = [Path(directory)]
        entries.extend(Path(directory, name) for name in subdirectories)
        entries.extend(Path(directory, name) for name in filenames)
        for entry in entries:
            try:
                total += entry.lstat().st_blocks * 512
            except FileNotFoundError:
                continue
    return total


def volume_used_bytes(root: Path) -> int:
    filesystem = os.statvfs(root)
    return (filesystem.f_blocks - filesystem.f_bavail) * filesystem.f_frsize


def state_machine_names(state_file: Path) -> dict[str, str]:
    names: dict[str, str] = {}
    for line in state_file.read_text().splitlines():
        fields = line.split("\t")
        if len(fields) >= 5 and fields[0] == "machine":
            names[fields[1]] = fields[4]
    if not names:
        raise E2EError(f"world state has no machine records: {state_file}")
    return names


def world_state_file(home: Path) -> Path:
    candidates = sorted(
        path for path in (home / ".smolworld").rglob("state") if path.is_file()
    )
    if len(candidates) != 1:
        rendered = ", ".join(str(candidate) for candidate in candidates)
        raise E2EError(f"expected one world state file, found {len(candidates)}: {rendered}")
    return candidates[0]


class ForkWorld:
    def __init__(
        self,
        project: Path,
        smolworld: Path,
        smolvm: Path,
        archive: Path,
        agent_rootfs: Path,
        lib_dir: Path,
    ) -> None:
        self.project = project
        self.smolworld = smolworld
        self.smolvm = smolvm
        self.archive = archive
        self.temporary = Path(tempfile.mkdtemp(prefix="smw-fork-e2e.", dir="/tmp"))
        self.home = self.temporary / "home"
        self.world_dir = self.temporary / "world"
        self.runtime_root = Path(
            tempfile.mkdtemp(prefix="smw-fork-runtime.", dir="/tmp")
        )
        self.world_file = self.world_dir / ".smolworld"
        self.up_log_path = self.temporary / "up.log"
        self.up_log: object | None = None
        self.up_process: subprocess.Popen[str] | None = None
        self.state_file: Path | None = None
        self.machine_names: dict[str, str] = {}
        self.clone_name: str | None = None
        self.checkpoint_root: Path | None = None
        self.checkpoint_released = False
        self.environment = dict(os.environ)
        self.environment.update(
            {
                "HOME": str(self.home),
                "SMOLWORLD_SMOLVM": str(self.smolvm),
                "SMOLVM_AGENT_ROOTFS": str(agent_rootfs),
                "SMOLVM_LIB_DIR": str(lib_dir),
                "SMOLVM_RUNTIME_ROOT": str(self.runtime_root),
            }
        )

    def smolworld_command(self, arguments: Sequence[str]) -> list[str]:
        return [str(self.smolworld), "-f", str(self.world_file), *arguments]

    def smolvm_command(self, arguments: Sequence[str]) -> list[str]:
        return [str(self.smolvm), *arguments]

    def prepare_fixture(self) -> None:
        fixture = self.project / "examples" / "redis"
        self.home.mkdir(parents=True)
        self.world_dir.mkdir(parents=True)
        shutil.copy2(fixture / ".smolworld", self.world_file)
        (self.world_dir / "smol").mkdir()
        shutil.copy2(fixture / "smol" / "redis.Smolfile", self.world_dir / "smol" / "redis.Smolfile")
        shutil.copy2(fixture / "smol" / "runner.Smolfile", self.world_dir / "smol" / "runner.Smolfile")
        os.symlink(self.archive, self.world_dir / "redis.tar")

    def prepare_and_check(self) -> None:
        run(self.smolworld_command(["prepare"]), self.environment, timeout=60.0)
        if (self.home / ".smolworld").exists():
            raise E2EError("smolworld prepare unexpectedly allocated world runtime state")
        run(self.smolworld_command(["check"]), self.environment, timeout=60.0)
        if (self.home / ".smolworld").exists():
            raise E2EError("smolworld check unexpectedly allocated world runtime state")

    def start_supervisor(self, arguments: Sequence[str], ready_message: str) -> None:
        if self.up_process is not None:
            raise E2EError("world supervisor is already running")
        if self.up_log is not None:
            self.up_log.close()  # type: ignore[union-attr]
            self.up_log = None
        self.up_log = self.up_log_path.open("w", encoding="utf-8")
        self.up_process = subprocess.Popen(
            self.smolworld_command(arguments),
            env=self.environment,
            stdin=subprocess.DEVNULL,
            stdout=self.up_log,
            stderr=subprocess.STDOUT,
            text=True,
        )
        deadline = time.monotonic() + 60.0
        while time.monotonic() < deadline:
            if ready_message in self.up_log_path.read_text(encoding="utf-8"):
                self.state_file = world_state_file(self.home)
                self.machine_names = state_machine_names(self.state_file)
                return
            if self.up_process.poll() is not None:
                raise E2EError(
                    "smolworld up exited before the world became ready:\n"
                    + self.up_log_path.read_text(encoding="utf-8")
                )
            time.sleep(0.25)
        raise E2EError(
            "timed out waiting for smolworld up:\n"
            + self.up_log_path.read_text(encoding="utf-8")
        )

    def up(self) -> None:
        self.start_supervisor(["up"], "world is up; press Ctrl-C")

    def restore(self) -> None:
        if self.checkpoint_root is None:
            raise E2EError("cannot restore without a durable checkpoint root")
        self.start_supervisor(
            ["restore", "--checkpoint", str(self.checkpoint_root)],
            "restored world is up; press Ctrl-C",
        )

    def stop_up(self) -> None:
        if self.up_process is None:
            return
        if self.up_process.poll() is None:
            self.up_process.send_signal(signal.SIGINT)
        try:
            status = self.up_process.wait(timeout=30.0)
        except subprocess.TimeoutExpired:
            self.up_process.kill()
            self.up_process.wait(timeout=10.0)
            raise E2EError("smolworld up did not stop after SIGINT")
        finally:
            self.up_process = None
        if status != 0:
            raise E2EError(
                f"smolworld up stopped with {status}:\n"
                + self.up_log_path.read_text(encoding="utf-8")
            )

    def checkpoint(self) -> float:
        if self.up_process is None:
            raise E2EError("cannot checkpoint without a running world supervisor")
        self.checkpoint_root = self.temporary / "checkpoint"
        started = time.monotonic_ns()
        run(
            self.smolworld_command(
                ["checkpoint", "--output", str(self.checkpoint_root)]
            ),
            self.environment,
            timeout=180.0,
        )
        elapsed = (time.monotonic_ns() - started) / 1_000_000
        try:
            status = self.up_process.wait(timeout=30.0)
        except subprocess.TimeoutExpired as error:
            raise E2EError(
                "world supervisor did not exit after a successful checkpoint"
            ) from error
        self.up_process = None
        if status != 0:
            raise E2EError(
                f"world supervisor stopped with {status} after checkpoint:\n"
                + self.up_log_path.read_text(encoding="utf-8")
            )
        if not self.checkpoint_root.is_dir():
            raise E2EError(f"checkpoint root was not published: {self.checkpoint_root}")
        receipt = self.checkpoint_root / "smolworld-checkpoint"
        receipt_text = receipt.read_text(encoding="utf-8")
        required = ["switch-epoch\t", "switch-queue\t0", "machine\trunner\t", "machine\tredis\t"]
        if any(marker not in receipt_text for marker in required):
            raise E2EError(f"checkpoint receipt is missing expected world cut: {receipt_text!r}")
        machine_receipts = {}
        for line in receipt_text.splitlines():
            fields = line.split("\t")
            if len(fields) == 3 and fields[0] == "machine-receipt":
                machine_receipts[fields[1]] = fields[2]
        if set(machine_receipts) != {"runner", "redis"} or any(
            not digest.startswith("blake3:") or len(digest) != len("blake3:") + 64
            for digest in machine_receipts.values()
        ):
            raise E2EError(
                "checkpoint receipt does not contain bounded digests for both machine receipts"
            )
        for machine in ("runner", "redis"):
            machine_receipt = (
                self.checkpoint_root
                / "machines"
                / machine
                / "smolvm-checkpoint.json"
            )
            if not machine_receipt.is_file():
                raise E2EError(f"machine checkpoint receipt is missing: {machine_receipt}")

        # Exercise the recovery guard before any VM is relaunched: changing an
        # opaque machine receipt must be rejected, while restoring the exact
        # original bytes must remain possible from the retained source records.
        runner_machine_receipt = (
            self.checkpoint_root
            / "machines"
            / "runner"
            / "smolvm-checkpoint.json"
        )
        original_machine_receipt = runner_machine_receipt.read_bytes()
        runner_machine_receipt.write_bytes(original_machine_receipt + b"\n")
        try:
            rejected = run(
                self.smolworld_command(
                    ["restore", "--checkpoint", str(self.checkpoint_root)]
                ),
                self.environment,
                timeout=60.0,
                check=False,
            )
            if rejected.returncode == 0 or "receipt digest" not in output_text(rejected):
                raise E2EError(
                    "restore accepted a tampered world receipt:\n"
                    + output_text(rejected).strip()
                )
            release_rejected = run(
                self.smolworld_command(
                    ["release", "--checkpoint", str(self.checkpoint_root)]
                ),
                self.environment,
                timeout=60.0,
                check=False,
            )
            if release_rejected.returncode == 0 or "receipt digest" not in output_text(
                release_rejected
            ):
                raise E2EError(
                    "release accepted a tampered world receipt:\n"
                    + output_text(release_rejected).strip()
                )
        finally:
            runner_machine_receipt.write_bytes(original_machine_receipt)
        return elapsed

    def release_checkpoint(self) -> None:
        if self.checkpoint_root is None or self.checkpoint_released:
            return
        supervisor_error: E2EError | None = None
        try:
            self.stop_up()
        except E2EError as error:
            # A restore may have already exited after a failed launch. Its
            # supervisor result is evidence, but it must not prevent exact
            # release of the known source records and checkpoint root.
            supervisor_error = error
        if os.environ.get("SMOLWORLD_E2E_KEEP") == "1":
            if supervisor_error is not None:
                raise supervisor_error
            return
        run(
            self.smolworld_command(
                ["release", "--checkpoint", str(self.checkpoint_root)]
            ),
            self.environment,
            timeout=60.0,
        )
        if self.checkpoint_root.exists():
            raise E2EError(f"release retained checkpoint root: {self.checkpoint_root}")
        self.checkpoint_released = True
        if supervisor_error is not None:
            raise supervisor_error

    def guest_via_world(self, machine: str, command: Sequence[str]) -> str:
        completed = run(
            self.smolworld_command(["exec", machine, "--", *command]),
            self.environment,
            timeout=30.0,
        )
        return completed.stdout

    def guest_via_smolvm(self, machine: str, command: Sequence[str]) -> str:
        completed = run(
            self.smolvm_command(["machine", "exec", "--name", machine, "--", *command]),
            self.environment,
            timeout=30.0,
        )
        return completed.stdout

    def assert_private_redis_via_world(self, machine: str) -> None:
        hosts = self.guest_via_world(machine, ["getent", "hosts", "redis"])
        if "10.89.0." not in hosts:
            raise E2EError(f"{machine} did not resolve Redis through world DNS: {hosts!r}")
        pong = self.guest_via_world(machine, ["redis-cli", "-h", "redis", "ping"])
        if pong.strip() != "PONG":
            raise E2EError(f"{machine} did not reach Redis through external NIC: {pong!r}")

    def assert_private_redis_via_smolvm(self, machine: str) -> None:
        script = "set -eu\ngetent hosts redis | grep -F '10.89.0.'\nredis-cli -h redis ping"
        pong = self.guest_via_smolvm(machine, ["/bin/sh", "-ceu", script])
        if pong.strip().splitlines()[-1:] != ["PONG"]:
            raise E2EError(
                f"{machine} did not reconnect agent and private NIC after fork: {pong!r}"
            )

    def make_runner_forkable(self, runner: str) -> None:
        run(
            self.smolvm_command(["machine", "stop", "--name", runner]),
            self.environment,
            timeout=30.0,
        )
        run(
            self.smolvm_command(["machine", "start", "--name", runner, "--forkable"]),
            self.environment,
            timeout=60.0,
        )
        retry(
            "forkable runner agent reconnect",
            lambda: self.guest_via_smolvm(runner, ["/bin/true"]),
        )

    def reach_forkpoint(self, runner: str) -> None:
        run(
            self.smolvm_command(
                [
                    "machine",
                    "exec",
                    "--name",
                    runner,
                    "--detach",
                    "--",
                    "/usr/local/bin/smolvm-fork-ready",
                ]
            ),
            self.environment,
            timeout=30.0,
        )
        retry(
            "runner forkpoint marker",
            lambda: self.guest_via_smolvm(
                runner, ["/bin/sh", "-ceu", "test -f /run/smolvm/forkpoint/ready"]
            ),
        )

    def fork(self, runner: str) -> tuple[float, int, int]:
        self.clone_name = f"smw-fork-e2e-{os.getpid()}"
        accounted_before = accounted_file_blocks_bytes(self.runtime_root)
        volume_before = volume_used_bytes(self.runtime_root)
        started = time.monotonic_ns()
        run(
            self.smolvm_command(
                [
                    "machine",
                    "fork",
                    "--golden",
                    runner,
                    "--name",
                    self.clone_name,
                    "--wait-ready",
                    "--ready-timeout",
                    "30s",
                ]
            ),
            self.environment,
            timeout=90.0,
        )
        finished = time.monotonic_ns()
        accounted_after = accounted_file_blocks_bytes(self.runtime_root)
        volume_after = volume_used_bytes(self.runtime_root)
        return (
            (finished - started) / 1_000_000,
            accounted_after - accounted_before,
            volume_after - volume_before,
        )

    def cleanup(self) -> None:
        failures: list[str] = []
        if self.clone_name is not None:
            completed = run(
                self.smolvm_command(
                    ["machine", "delete", "--name", self.clone_name, "--force"]
                ),
                self.environment,
                timeout=30.0,
                check=False,
            )
            if completed.returncode != 0:
                failures.append(f"delete clone: {output_text(completed).strip()}")
        if self.checkpoint_root is not None:
            try:
                self.release_checkpoint()
            except E2EError as error:
                failures.append(str(error))
        else:
            try:
                self.stop_up()
            except E2EError as error:
                failures.append(str(error))
        if self.world_file.exists() and self.checkpoint_root is None:
            completed = run(
                self.smolworld_command(["down"]),
                self.environment,
                timeout=30.0,
                check=False,
            )
            if completed.returncode != 0:
                failures.append(f"world down: {output_text(completed).strip()}")
        if self.state_file is not None:
            listed = run(
                self.smolvm_command(["machine", "ls", "--json"]),
                self.environment,
                timeout=30.0,
                check=False,
            )
            if listed.returncode != 0:
                failures.append(f"list machines: {output_text(listed).strip()}")
            else:
                output = listed.stdout
                remaining = [
                    name for name in self.machine_names.values() if name in output
                ]
                if self.clone_name is not None and self.clone_name in output:
                    remaining.append(self.clone_name)
                if remaining:
                    failures.append(f"cleanup left exact test machines: {remaining}")
        if self.up_log is not None:
            self.up_log.close()  # type: ignore[union-attr]
            self.up_log = None
        if failures and os.environ.get("SMOLWORLD_E2E_KEEP") == "1":
            failures.append(
                f"preserved diagnostic roots {self.temporary} and {self.runtime_root}"
            )
        else:
            shutil.rmtree(self.runtime_root, ignore_errors=True)
            shutil.rmtree(self.temporary, ignore_errors=True)
        if failures:
            raise E2EError("; ".join(failures))


def main() -> int:
    fork_gate = os.environ.get("SMOLWORLD_FORK_E2E") == "1"
    durable_gate = os.environ.get("SMOLWORLD_DURABLE_E2E") == "1"
    if fork_gate == durable_gate:
        emit(
            "SKIP: set exactly one of SMOLWORLD_FORK_E2E=1 or "
            "SMOLWORLD_DURABLE_E2E=1"
        )
        return 0
    if platform.system() != "Darwin" or platform.machine() != "arm64":
        raise E2EError("Smolworld E2E requires macOS on Apple Silicon")
    for forbidden in ("DOCKER_HOST", "DOCKER_CONTEXT", "DOCKER_SOCKET", "ORBCTL_HOST"):
        if os.environ.get(forbidden):
            raise E2EError(f"Smolworld E2E must run without {forbidden}")

    project = Path(__file__).resolve().parent.parent
    smolvm = required_file("SMOLWORLD_SMOLVM")
    agent_rootfs = required_directory("SMOLVM_AGENT_ROOTFS")
    if not (agent_rootfs / "usr/local/bin/smolvm-agent").is_file():
        raise E2EError(f"agent rootfs lacks usr/local/bin/smolvm-agent: {agent_rootfs}")
    lib_dir = required_directory("SMOLVM_LIB_DIR")
    if not (lib_dir / "libkrun.dylib").is_file():
        raise E2EError(f"SMOLVM_LIB_DIR lacks libkrun.dylib: {lib_dir}")
    fixture_archive = project / "examples" / "redis" / "redis.tar"
    archive = Path(os.environ.get("SMOLWORLD_REDIS_ARCHIVE", fixture_archive))
    if not archive.is_file():
        raise E2EError(f"prepared Redis archive is missing: {archive}")

    run(["cargo", "build", "--manifest-path", str(project / "Cargo.toml"), "--quiet"], dict(os.environ))
    smolworld = project / "target/debug/smolworld"
    if not smolworld.is_file():
        raise E2EError(f"cargo did not produce smolworld: {smolworld}")

    world = ForkWorld(project, smolworld, smolvm, archive.resolve(), agent_rootfs, lib_dir)
    primary_error: Exception | None = None
    try:
        world.prepare_fixture()
        world.prepare_and_check()
        world.up()
        runner = world.machine_names.get("runner")
        if runner is None:
            raise E2EError(f"world state did not include runner: {world.machine_names}")

        retry(
            "initial runner private DNS and Redis traffic",
            lambda: world.assert_private_redis_via_world("runner"),
        )
        if fork_gate:
            world.make_runner_forkable(runner)
            retry(
                "forkable runner private NIC reconnect",
                lambda: world.assert_private_redis_via_world("runner"),
            )
            world.reach_forkpoint(runner)
            transition_ms, accounted_delta, volume_delta = world.fork(runner)
            assert world.clone_name is not None
            reconnect_started = time.monotonic_ns()
            retry(
                "restored clone agent and private NIC reconnect",
                lambda: world.assert_private_redis_via_smolvm(world.clone_name or ""),
            )
            reconnect_ms = (time.monotonic_ns() - reconnect_started) / 1_000_000

            emit("# smolworld external-NIC fork E2E")
            emit(f"# smolvm={smolvm}")
            emit(f"# archive={archive.resolve()}")
            emit("metric\tvalue\tunit")
            emit(f"fork_transition_wall\t{transition_ms:.3f}\tms")
            emit(f"clone_agent_and_private_nic_ready\t{reconnect_ms:.3f}\tms")
            emit(f"fork_accounted_file_blocks_delta\t{accounted_delta}\tbytes")
            emit(f"fork_volume_used_delta\t{volume_delta}\tbytes")
            emit("PASS: private NIC traffic, agent reconnect, and fork sharing measurement")
        else:
            world.guest_via_world(
                "runner",
                [
                    "/bin/sh",
                    "-ceu",
                    "printf durable > /workspace/smolworld-durable-marker\n"
                    "redis-cli -h redis set smolworld-durable-key durable",
                ],
            )
            capture_ms = world.checkpoint()
            world.restore()
            reconnect_started = time.monotonic_ns()
            retry(
                "restored runner private DNS and Redis traffic",
                lambda: world.assert_private_redis_via_world("runner"),
            )
            reconnect_ms = (time.monotonic_ns() - reconnect_started) / 1_000_000
            restored = world.guest_via_world(
                "runner",
                [
                    "/bin/sh",
                    "-ceu",
                    "test \"$(cat /workspace/smolworld-durable-marker)\" = durable\n"
                    "test \"$(redis-cli -h redis get smolworld-durable-key)\" = durable",
                ],
            )
            if restored.strip():
                raise E2EError(f"unexpected durable-state verification output: {restored!r}")
            world.release_checkpoint()

            emit("# smolworld coordinated durable-world E2E")
            emit(f"# smolvm={smolvm}")
            emit(f"# archive={archive.resolve()}")
            emit("metric\tvalue\tunit")
            emit(f"world_checkpoint_wall\t{capture_ms:.3f}\tms")
            emit(f"restored_runner_private_nic_ready\t{reconnect_ms:.3f}\tms")
            emit("PASS: durable workspace/Redis state, fresh agent/NIC handles, and exact release")
    except (Exception, KeyboardInterrupt) as error:
        # Ctrl-C is common for an opt-in hardware integration gate. It must
        # still remove only the exact machines and temporary roots this run
        # created before the outer handler reports the interruption.
        primary_error = error
    try:
        world.cleanup()
    except Exception as cleanup_error:
        if primary_error is None:
            primary_error = cleanup_error
        else:
            primary_error = E2EError(f"{primary_error}; cleanup: {cleanup_error}")
    if primary_error is not None:
        raise primary_error
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except E2EError as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(2)
    except KeyboardInterrupt:
        print("interrupted", file=sys.stderr)
        raise SystemExit(130)
