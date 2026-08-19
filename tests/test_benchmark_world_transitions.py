#!/usr/bin/env python3
"""Unit coverage for the opt-in transition benchmark's pure helpers."""

from __future__ import annotations

import io
import importlib.util
import shutil
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest import mock
from pathlib import Path


BENCHMARK_PATH = Path(__file__).with_name("benchmark_world_transitions.py")
SPEC = importlib.util.spec_from_file_location("benchmark_world_transitions", BENCHMARK_PATH)
if SPEC is None or SPEC.loader is None:  # pragma: no cover - import machinery failure
    raise RuntimeError(f"cannot load {BENCHMARK_PATH}")
benchmark = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = benchmark
SPEC.loader.exec_module(benchmark)


class BenchmarkConfigurationTests(unittest.TestCase):
    def test_parses_distinct_concurrency_levels_in_ascending_order(self) -> None:
        self.assertEqual(benchmark.parse_concurrency_levels("4, 1, 2"), [1, 2, 4])

    def test_rejects_empty_duplicate_and_too_large_concurrency_levels(self) -> None:
        for value in ("", "1,1", "0", "251"):
            with self.subTest(value=value):
                with self.assertRaises(benchmark.BenchmarkError):
                    benchmark.parse_concurrency_levels(value)

    def test_failure_messages_stay_on_one_tsv_record(self) -> None:
        self.assertEqual(
            benchmark.tsv_field("database is locked\nretry\tagain"),
            "database is locked retry again",
        )

    def test_prepared_world_profile_requires_an_exact_declared_service(self) -> None:
        with tempfile.TemporaryDirectory(prefix="smolworld-transition-profile-") as directory:
            config = Path(directory) / ".smolworld"
            config.write_text("format: 2\n", encoding="utf-8")
            with mock.patch.dict(
                benchmark.os.environ,
                {benchmark.PREPARED_WORLD_VARIABLE: str(config)},
                clear=True,
            ):
                with self.assertRaises(benchmark.BenchmarkError):
                    benchmark.prepared_world_profile_from_environment()
            with mock.patch.dict(
                benchmark.os.environ,
                {
                    benchmark.PREPARED_WORLD_VARIABLE: str(config),
                    benchmark.ATTACH_SERVICE_VARIABLE: "runner",
                    benchmark.ATTACH_SETTLE_SECONDS_VARIABLE: "0.25",
                },
                clear=True,
            ):
                profile = benchmark.prepared_world_profile_from_environment()

        self.assertEqual(profile.config, config)
        self.assertEqual(profile.service, "runner")
        self.assertEqual(profile.attach_settle_seconds, 0.25)

    def test_prepared_world_profile_rejects_a_partial_or_invalid_delay(self) -> None:
        with mock.patch.dict(
            benchmark.os.environ,
            {benchmark.ATTACH_SERVICE_VARIABLE: "runner"},
            clear=True,
        ):
            with self.assertRaises(benchmark.BenchmarkError):
                benchmark.prepared_world_profile_from_environment()
        with tempfile.TemporaryDirectory(prefix="smolworld-transition-profile-") as directory:
            config = Path(directory) / ".smolworld"
            config.write_text("format: 2\n", encoding="utf-8")
            with mock.patch.dict(
                benchmark.os.environ,
                {
                    benchmark.PREPARED_WORLD_VARIABLE: str(config),
                    benchmark.ATTACH_SERVICE_VARIABLE: "runner",
                    benchmark.ATTACH_SETTLE_SECONDS_VARIABLE: "-1",
                },
                clear=True,
            ):
                with self.assertRaises(benchmark.BenchmarkError):
                    benchmark.prepared_world_profile_from_environment()


class PreparedWorldLifecycleTests(unittest.TestCase):
    def test_prepared_world_lifecycle_events_use_only_supervisor_boundaries(self) -> None:
        self.assertEqual(
            benchmark.prepared_world_lifecycle_event("smolworld: created runner"),
            ("machine_created", "runner"),
        )
        self.assertEqual(
            benchmark.prepared_world_lifecycle_event("smolworld: started runner"),
            ("machine_started", "runner"),
        )
        self.assertEqual(
            benchmark.prepared_world_lifecycle_event("smolworld: attached runner"),
            ("nic_attach", "runner"),
        )
        self.assertIsNone(benchmark.prepared_world_lifecycle_event("Created machine: runner"))
        self.assertIsNone(benchmark.prepared_world_lifecycle_event("smolworld: started runner now"))

    def test_closed_ps_rows_distinguish_absence_from_running_visibility(self) -> None:
        absent_rows = (
            '{"service":"database","ip":"10.0.0.2","mac":"02:00:00:00:00:02","status":"absent"}\n'
            '{"service":"runner","ip":"10.0.0.3","mac":"02:00:00:00:00:03","status":"absent"}\n'
        )
        running_row = (
            '{"service":"runner","ip":"10.0.0.3","mac":"02:00:00:00:00:03","status":"running"}\n'
        )

        self.assertTrue(benchmark.prepared_world_is_idle(absent_rows))
        self.assertTrue(benchmark.service_is_running_in_ps_json(running_row, "runner"))
        self.assertFalse(benchmark.service_is_running_in_ps_json(absent_rows.splitlines()[1], "runner"))

    def test_rejects_non_closed_or_unexpected_ps_rows(self) -> None:
        with self.assertRaises(benchmark.BenchmarkError):
            benchmark.prepared_world_is_idle('{"service":"runner","status":"absent"}\n')
        with self.assertRaises(benchmark.BenchmarkError):
            benchmark.service_is_running_in_ps_json(
                '{"service":"other","ip":"10.0.0.3","mac":"02:00:00:00:00:03","status":"running"}\n',
                "runner",
            )

    def test_attachment_profile_never_prepares_or_removes_external_material(self) -> None:
        absent = '{"service":"runner","ip":"10.0.0.3","mac":"02:00:00:00:00:03","status":"absent"}\n'
        running = '{"service":"runner","ip":"10.0.0.3","mac":"02:00:00:00:00:03","status":"running"}\n'
        commands: list[tuple[str, ...]] = []

        class FakeSupervisor:
            def __init__(self) -> None:
                self.returncode: int | None = None
                self.stdout = io.StringIO("")
                self.stderr = io.StringIO(
                    "smolworld: created runner\n"
                    "smolworld: started runner\n"
                    "smolworld: attached runner\n"
                    "smolworld: world is up; press Ctrl-C to stop it\n"
                )

            def poll(self) -> int | None:
                return self.returncode

            def send_signal(self, _signal: int) -> None:
                self.returncode = 0

            def wait(self, timeout: float | None = None) -> int:
                self.returncode = 0
                return 0

            def kill(self) -> None:
                self.returncode = -9

        def fake_command(
            _smolworld_bin: Path,
            _config: Path,
            arguments: list[str],
            _environment: dict[str, str],
        ) -> subprocess.CompletedProcess[str]:
            commands.append(tuple(arguments))
            if arguments == ["config", "--quiet"] or arguments == ["check"]:
                return subprocess.CompletedProcess([], 0, "", "")
            if arguments == ["ps", "--all", "--format", "json"]:
                return subprocess.CompletedProcess([], 0, absent, "")
            if arguments == ["ps", "--format", "json", "runner"]:
                return subprocess.CompletedProcess([], 0, running, "")
            if arguments == ["exec", "runner", "--", "/bin/true"]:
                return subprocess.CompletedProcess([], 0, "", "")
            self.fail(f"unexpected command: {arguments!r}")

        samples: list[benchmark.TimingSample] = []
        waves: list[benchmark.WaveSample] = []
        profile = benchmark.PreparedWorldProfile(Path("/fixture/.smolworld"), "runner", 0.0)
        with (
            mock.patch.object(benchmark, "smolworld_command", side_effect=fake_command),
            mock.patch.object(benchmark.subprocess, "Popen", return_value=FakeSupervisor()),
            mock.patch.object(benchmark, "emit"),
        ):
            benchmark.run_prepared_world_profile(
                Path("/smolworld"), Path("/smolvm"), profile, 1, samples, waves
            )

        self.assertNotIn(("prepare",), commands)
        self.assertIn(("exec", "runner", "--", "/bin/true"), commands)
        self.assertEqual(
            {sample.phase for sample in samples},
            {
                "config",
                "check",
                "host_visible",
                "command_attach",
                "attached_command",
                "machine_created",
                "machine_started",
                "created_to_started",
                "nic_attach",
                "started_to_nic_attach",
            },
        )
        self.assertEqual([wave.phase for wave in waves], ["world_ready"])


class BenchmarkSummaryTests(unittest.TestCase):
    def test_percentile_uses_the_nearest_rank_for_small_samples(self) -> None:
        self.assertEqual(benchmark.percentile([1.0, 2.0, 3.0, 4.0], 0.95), 4.0)
        self.assertEqual(benchmark.percentile([1.0, 2.0, 3.0, 4.0], 0.50), 2.0)

    def test_summarize_groups_by_profile_mode_concurrency_and_phase(self) -> None:
        samples = [
            benchmark.TimingSample("archive", "parallel", 1, 2, "start", "one", 10.0),
            benchmark.TimingSample("archive", "parallel", 2, 2, "start", "two", 20.0),
            benchmark.TimingSample("bare", "serial", 1, 1, "start", "one", 5.0),
        ]

        self.assertEqual(
            benchmark.summarize_samples(samples),
            [
                ("archive", "parallel", 2, "start", 2, 15.0, 20.0),
                ("bare", "serial", 1, "start", 1, 5.0, 5.0),
            ],
        )

    def test_summarize_waves_keeps_world_ready_barriers_visible(self) -> None:
        waves = [
            benchmark.WaveSample("world", "parallel", 1, 2, "world_ready", 30.0),
            benchmark.WaveSample("world", "parallel", 2, 2, "world_ready", 40.0),
            benchmark.WaveSample("archive", "serial", 1, 1, "create", 10.0),
        ]

        self.assertEqual(
            benchmark.summarize_waves(waves),
            [
                ("archive", "serial", 1, "create", 1, 10.0, 10.0),
                ("world", "parallel", 2, "world_ready", 2, 35.0, 40.0),
            ],
        )

    def test_summarize_traces_keeps_nested_boot_spans_out_of_wall_samples(self) -> None:
        traces = [
            benchmark.TraceSample("archive", "parallel", 1, 2, "agent_ready", "one", 100.0),
            benchmark.TraceSample("archive", "parallel", 2, 2, "agent_ready", "two", 120.0),
        ]

        self.assertEqual(
            benchmark.summarize_traces(traces),
            [("archive", "parallel", 2, "agent_ready", 2, 110.0, 120.0)],
        )


class StartupTraceTests(unittest.TestCase):
    def test_parses_parent_and_boot_helper_stages(self) -> None:
        trace = benchmark.parse_startup_trace(
            "[proc] fds closed               4ms\n"
            "[boot] libkrun started           19ms\n"
            "2026-08-18T12:00:00Z INFO elapsed_ms=23 boot: disks ready\n"
            "2026-08-18T12:00:00Z INFO elapsed_ms=25 boot: config written\n"
            "2026-08-18T12:00:00Z INFO spawn_ms=4 boot: subprocess spawned\n"
            "2026-08-18T12:00:00Z DEBUG elapsed_ms=141 agent ready (doorbell)\n"
            "2026-08-18T12:00:00Z INFO boot_ms=143.5 agent VM is ready\n"
        )

        self.assertEqual(
            trace,
            {
                "proc_fds_closed": 4.0,
                "boot_libkrun_started": 19.0,
                "launch_disks_ready": 23.0,
                "launch_config_written": 25.0,
                "launch_subprocess_spawn": 4.0,
                "agent_ready": 141.0,
                "agent_boot_complete": 143.5,
            },
        )

    def test_trace_environment_is_explicit_and_preserves_existing_filters(self) -> None:
        with mock.patch.dict(
            benchmark.os.environ,
            {
                benchmark.TRACE_ENVIRONMENT_VARIABLE: "1",
                "RUST_LOG": "smolvm=info",
            },
            clear=True,
        ):
            self.assertTrue(benchmark.configure_trace_environment())
            self.assertEqual(
                benchmark.os.environ["RUST_LOG"], "smolvm=info,smolvm::agent=debug"
            )
            self.assertEqual(benchmark.os.environ["SMOLVM_BOOT_DEBUG"], "1")

    def test_derives_local_layer_materialization_from_timestamped_progress(self) -> None:
        trace = benchmark.parse_startup_trace(
            "2026-08-19T01:43:45.100000Z DEBUG agent ready (doorbell) elapsed_ms=100\n"
            "2026-08-19T01:43:45.125000Z  INFO detached start progress extracting local image layers\n"
            "2026-08-19T01:43:45.725000Z  INFO detached start progress preparing persistent overlay\n"
            "2026-08-19T01:43:45.727000Z  INFO detached start progress starting detached container\n"
        )

        self.assertEqual(trace["agent_ready_to_layer_materialization"], 25.0)
        self.assertEqual(trace["layer_materialization_to_overlay"], 600.0)
        self.assertEqual(trace["agent_ready_to_workload_start"], 627.0)


class ForkReferenceTests(unittest.TestCase):
    def test_serializes_forks_from_a_single_golden(self) -> None:
        class FakeBenchmark:
            fork_threads: list[int] = []

            def __init__(self, _smolvm_bin: Path, _archive: Path, runtime_root: Path) -> None:
                self.runtime_root = runtime_root

            def reserve_names(self, _names: list[str]) -> None:
                pass

            def create_machine(self, _name: str, _archive: Path) -> None:
                pass

            def start_machine(self, _name: str, _forkable: bool) -> None:
                pass

            def fork(self, _golden: str, _clone: str) -> None:
                self.fork_threads.append(threading.get_ident())

            def write_mutation(self, _name: str, _marker: str) -> None:
                pass

            def cleanup(self) -> None:
                shutil.rmtree(self.runtime_root)

        samples: list[benchmark.TimingSample] = []
        waves: list[benchmark.WaveSample] = []
        with (
            mock.patch.object(benchmark, "SmolvmBenchmark", FakeBenchmark),
            mock.patch.object(benchmark, "emit"),
        ):
            benchmark.run_fork_reference(
                Path("/smolvm"), Path("/archive.tar"), 1, 3, samples, waves
            )

        fork_samples = [sample for sample in samples if sample.phase == "fork"]
        self.assertEqual([sample.mode for sample in fork_samples], ["serial"] * 3)
        self.assertEqual(FakeBenchmark.fork_threads, [threading.get_ident()] * 3)


class BenchmarkCleanupTests(unittest.TestCase):
    def test_removes_private_root_when_a_reserved_machine_was_never_created(self) -> None:
        runtime_root = Path(tempfile.mkdtemp(prefix="smolworld-transition-test-"))
        runner = benchmark.SmolvmBenchmark(Path("/smolvm"), Path("/archive.tar"), runtime_root)
        runner.owned_names = ["smw-benchmark-missing"]

        with (
            mock.patch.object(
                runner, "command", side_effect=benchmark.BenchmarkError("delete failed")
            ),
            mock.patch.object(runner, "machine_is_absent", return_value=True),
        ):
            runner.cleanup()

        self.assertEqual(runner.owned_names, [])
        self.assertFalse(runtime_root.exists())

    def test_reads_only_valid_exact_machine_names_from_generated_state(self) -> None:
        state_dir = Path(tempfile.mkdtemp(prefix="smolworld-transition-state-test-"))
        state_file = state_dir / "state"
        state_file.write_text(
            "version\t2\n"
            "machine\trunner\t10.89.0.18\t02:00:00:00:00:18\tsmw-abcdef-0123\n",
            encoding="utf-8",
        )
        try:
            self.assertEqual(
                benchmark.recorded_world_machine_names(state_dir), ["smw-abcdef-0123"]
            )
        finally:
            state_file.unlink()
            state_dir.rmdir()

    def test_rejects_an_unsafe_machine_name_in_generated_state(self) -> None:
        state_dir = Path(tempfile.mkdtemp(prefix="smolworld-transition-state-test-"))
        state_file = state_dir / "state"
        state_file.write_text(
            "machine\trunner\t10.89.0.18\t02:00:00:00:00:18\tother-world\n",
            encoding="utf-8",
        )
        try:
            with self.assertRaises(benchmark.BenchmarkError):
                benchmark.recorded_world_machine_names(state_dir)
        finally:
            state_file.unlink()
            state_dir.rmdir()


if __name__ == "__main__":
    unittest.main()
