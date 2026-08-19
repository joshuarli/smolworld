#!/usr/bin/env python3
"""Unit coverage for the opt-in transition benchmark's pure helpers."""

from __future__ import annotations

import importlib.util
import shutil
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
