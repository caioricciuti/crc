"""Check partitioning, ordering and cleanup without launching native apps."""

import importlib.util
from pathlib import Path
import signal
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location(
    "selftest_parallel", Path(__file__).with_name("selftest-parallel.py")
)
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class LaneTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def scenario(self, name, seconds):
        path = self.root / f"{name}.sh"
        path.write_text(f"wait {seconds}\nCRC_SELFTEST=fixture\n")
        return path

    def test_balances_longest_first_without_duplicating_or_losing_scenarios(self):
        paths = [self.scenario(str(i), seconds) for i, seconds in enumerate([20, 16, 8, 4])]
        lanes = runner.assign(paths, 2)
        self.assertEqual(lanes, [[paths[0], paths[3]], [paths[1], paths[2]]])
        self.assertCountEqual([p for lane in lanes for p in lane], paths)

    def test_caps_lanes_at_selected_scenario_count(self):
        paths = [self.scenario("one", 1), self.scenario("two", 2)]
        lanes = runner.assign(paths, 8)
        self.assertEqual(len(lanes), 2)
        self.assertTrue(all(lanes))
        self.assertEqual(runner.assign([], 4), [])

    def test_recorded_weights_override_estimates(self):
        paths = [self.scenario("01-short", 100), self.scenario("02-long", 1)]
        self.assertEqual(runner.assign(paths, 2, weights={"short": 1, "long": 100}),
                         [[paths[1]], [paths[0]]])

    def test_shuffle_preserves_assignments_and_is_reproducible(self):
        paths = [self.scenario(str(i), i) for i in range(16)]
        serial = runner.assign(paths, 2)
        shuffled = runner.assign(paths, 2, shuffle=True)
        self.assertEqual(shuffled, runner.assign(paths, 2, shuffle=True))
        self.assertNotEqual(serial, shuffled)
        for before, after in zip(serial, shuffled):
            self.assertCountEqual(before, after)

    @patch.object(runner.os, "killpg")
    def test_cleanup_signals_only_live_lane_process_groups(self, killpg):
        finished = Mock(pid=123)
        finished.poll.return_value = 0
        live = Mock(pid=456)
        live.poll.return_value = None
        runner.stop([finished, live])
        killpg.assert_called_once_with(456, signal.SIGTERM)
        finished.wait.assert_called_once()
        live.wait.assert_called_once()

    def test_lane_scenarios_cannot_scan_each_others_files_or_home(self):
        # Drive the actual shell harness with two fixture-only scenarios.
        # No native app or substitute executable is used by this test.
        scripts = self.root / "scripts"
        scenarios = scripts / "selftest"
        scenarios.mkdir(parents=True)
        shutil.copy2(Path(__file__).with_name("selftest.sh"), scripts / "selftest.sh")
        lane = self.root / "lane"
        lane.mkdir()
        (lane / "scenarios.list").write_text("01-seed\n02-clean\n")
        (scenarios / "01-seed.sh").write_text(
            'printf seed > "$T/previous.txt"\n'
            'printf seed > "$HOME/previous.txt"\n'
            'printf "%s\\n" "$T" "$HOME" "$CLAUDE_CONFIG_DIR" "$CRC_HISTORY_DB" > "$lane/first.paths"\n'
        )
        (scenarios / "02-clean.sh").write_text(
            '[ ! -e "$T/previous.txt" ] || failed "previous fixture leaked"\n'
            '[ ! -e "$HOME/previous.txt" ] || failed "previous home leaked"\n'
            'for path in "$T" "$HOME" "$CLAUDE_CONFIG_DIR" "$CRC_HISTORY_DB"; do\n'
            '    if grep -qxF "$path" "$lane/first.paths"; then failed "shared path: $path"; fi\n'
            'done\n'
        )
        result = subprocess.run(["bash", str(scripts / "selftest.sh"), "--lane", str(lane)],
                                capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual((lane / "pass.list").read_text(), "01-seed\n02-clean\n")
        self.assertEqual((lane / "fail.list").read_text(), "")
        self.assertFalse((lane / "fixture").exists())


if __name__ == "__main__":
    unittest.main()
