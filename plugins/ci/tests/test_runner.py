"""Behavior of actual job processes and independent resource leases."""

from __future__ import annotations

from contextlib import redirect_stdout
import io
import json
import os
from pathlib import Path
import shlex
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

from shunt_ci.runner import allocations, execute
from shunt_ci.spec import Plan
from shunt_ci.resources import cache_slot


def plan(jobs):
    return Plan.parse({"version": 1, "reserved_cpus": 0, "jobs": jobs})


def job(name, command, **values):
    return {"name": name, "steps": [{"run": command}], **values}


class JobExecution(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="shunt-ci-tests-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name) / "workspace with spaces"
        self.root.mkdir()
        self.output = Path(self.temporary.name) / "report"
        self.cpus = sorted(os.sched_getaffinity(0))[:3]

    def run_plan(self, value, **arguments):
        with redirect_stdout(io.StringIO()):
            return execute(value, self.root, self.output, available_cpus=self.cpus, **arguments)

    def test_jobs_inherit_only_explicit_controller_settings(self):
        command = shlex.join([sys.executable, "-c", "import json,os,pathlib; pathlib.Path('environment.json').write_text(json.dumps({key:os.environ.get(key) for key in ('TACHO_HOME','TACHO_BATCH_CLAIMED','TACHO_GH_REPO','TACHO_TEST_MODE','PROJECT_VALUE')}))"])
        with patch.dict(os.environ, {"TACHO_HOME":"controller-home", "TACHO_BATCH_CLAIMED":"1", "TACHO_GH_REPO":"owner/controller", "PROJECT_VALUE":"retained"}):
            receipt = self.run_plan(plan([job("isolated", command, environment={"TACHO_TEST_MODE":"1"})]))
        self.assertTrue(receipt["ok"])
        self.assertEqual(json.loads((self.root / "environment.json").read_text()), {
            "TACHO_HOME":None,"TACHO_BATCH_CLAIMED":None,"TACHO_GH_REPO":None,
            "TACHO_TEST_MODE":"1","PROJECT_VALUE":"retained",
        })

    def test_environment_channels_preserve_literal_values_without_executing_them(self):
        marker = self.root / "must-not-exist"
        dangerous = f"$(touch {shlex.quote(str(marker))})"
        commands = [
            f"printf '%s\\n' {shlex.quote('VALUE=' + dangerous)} 'LINES<<END' first second END 'GITHUB_WORKSPACE=overridden' >> \"$GITHUB_ENV\"",
            shlex.join([sys.executable, "-c", "import json,os,pathlib; pathlib.Path('observed.json').write_text(json.dumps({key:os.environ.get(key) for key in ('VALUE','LINES','GITHUB_WORKSPACE')}))"]),
        ]
        specification = plan([{"name": "literal", "environment": {"GITHUB_WORKSPACE": str(self.root)},
                               "steps": [{"run": command} for command in commands]}])
        receipt = self.run_plan(specification)
        self.assertTrue(receipt["ok"])
        self.assertFalse(marker.exists())
        self.assertEqual(json.loads((self.root / "observed.json").read_text()),
                         {"VALUE": dangerous, "LINES": "first\nsecond", "GITHUB_WORKSPACE": str(self.root)})

    def test_an_unclosed_multiline_environment_value_fails_the_job(self):
        value = plan([job("malformed", "printf '%s\\n' 'LINES<<END' missing-close >> \"$GITHUB_ENV\"")])
        receipt = self.run_plan(value)
        self.assertFalse(receipt["ok"])
        self.assertEqual(receipt["jobs"]["malformed"]["exit_code"], 1)

    def test_failed_dependencies_never_run_or_become_a_passing_receipt(self):
        value = plan([job("compile", "exit 9"), job("dependent", "touch should-not-run", after=["compile"])])
        receipt = self.run_plan(value)
        self.assertFalse(receipt["ok"])
        self.assertFalse((self.root / "should-not-run").exists())
        self.assertEqual(receipt["jobs"]["compile"]["exit_code"], 9)
        self.assertEqual(receipt["jobs"]["dependent"]["status"], "not_started")
        self.assertEqual(json.loads((self.output / "receipt.json").read_text()), receipt)

    def test_independent_jobs_overlap_without_sharing_a_cpu(self):
        if len(self.cpus) < 2:
            self.skipTest("requires two available CPUs")
        rendezvous = threading.Barrier(2)
        observed = []
        lock = threading.Lock()

        def executor(job, _root, _name, cpus, _parent, _log, _cancellation):
            with lock:
                observed.append(set(cpus))
            rendezvous.wait(timeout=5)
            return 0

        receipt = self.run_plan(plan([job("first", "true", cpus=2), job("second", "true", cpus=2)]), executor=executor)
        self.assertTrue(receipt["ok"])
        self.assertEqual(len(observed), 2)
        self.assertTrue(observed[0].isdisjoint(observed[1]))

    def test_a_completed_gate_releases_its_cpus_to_the_dependent_job(self):
        observed = {}

        def executor(job, _root, _name, cpus, _parent, _log, _cancellation):
            observed[job.name] = set(cpus)
            return 0

        receipt = self.run_plan(plan([job("gate", "true", cpus=3), job("work", "true", cpus=3, after=["gate"])]), executor=executor)
        self.assertTrue(receipt["ok"])
        self.assertEqual(observed["gate"], set(self.cpus))
        self.assertEqual(observed["work"], set(self.cpus))

    def test_a_failure_cancels_the_foreground_processes_of_another_job(self):
        if len(self.cpus) < 2:
            self.skipTest("requires two available CPUs")
        os.mkfifo(self.root / "started")
        value = plan([
            job("slow", "sleep 120 & child=$!; printf '%s' \"$child\" > child.pid; printf 'ready\\n' > started; wait \"$child\""),
            job("failure", "read -r ready < started; exit 9"),
        ])
        receipt = self.run_plan(value)
        self.assertFalse(receipt["ok"])
        self.assertEqual(receipt["jobs"]["failure"]["exit_code"], 9)
        self.assertEqual(receipt["jobs"]["slow"]["status"], "stopped")
        pid = int((self.root / "child.pid").read_text())
        stat = Path(f"/proc/{pid}/stat")
        self.assertTrue(not stat.exists() or stat.read_text().split(")", 1)[1].strip().startswith("Z"))

    def test_a_timeout_is_a_failed_receipt_with_a_live_log(self):
        receipt = self.run_plan(plan([job("timeout", "sleep 120", timeout=1)]))
        self.assertFalse(receipt["ok"])
        result = receipt["jobs"]["timeout"]
        self.assertEqual(result["exit_code"], 124)
        self.assertIn("exceeded", Path(result["log"]).read_text())

    def test_ambiguous_or_invalid_plans_are_refused_before_execution(self):
        for invalid in (
            {"version": 2, "jobs": [job("work", "true")]},
            {"version": 1, "jobs": [job("work", "true", after=["missing"])]},
            {"version": 1, "jobs": [job("one", "true", after=["two"]), job("two", "true", after=["one"])]},
            {"version": 1, "jobs": [job("same", "true"), job("same", "true")]},
            {"version": 1, "jobs": [job("work", "true", ignored_control=True)]},
        ):
            with self.subTest(invalid=invalid), self.assertRaises(ValueError):
                Plan.parse(invalid)

    def test_a_symlink_cannot_move_a_step_outside_its_workspace(self):
        (self.root / "outside").symlink_to(self.root.parent, target_is_directory=True)
        value = plan([{"name": "escape", "steps": [{"run": "true", "directory": "outside"}]}])
        with self.assertRaises(ValueError):
            self.run_plan(value)

    def test_cpu_admission_never_allocates_an_unavailable_or_shared_cpu(self):
        selected = allocations({"first": 8, "second": 8, "third": 8}, {2, 5})
        self.assertEqual(selected, {"first": (2,), "second": (5,)})

    def test_persistent_cache_leases_never_share_a_directory_and_retain_its_content(self):
        root=self.root/'caches'
        with cache_slot(root,slots=2,timeout=0) as first:
            (first/'cached').write_text('retained')
            with cache_slot(root,slots=2,timeout=0) as second:
                self.assertNotEqual(first,second)
                with self.assertRaises(TimeoutError):
                    with cache_slot(root,slots=2,timeout=0):
                        self.fail('a busy cache was leased twice')
        with cache_slot(root,slots=2,timeout=0) as reused:
            self.assertEqual((reused/'cached').read_text(),'retained')

    def test_resource_templates_expose_only_the_admitted_execution_context(self):
        value=plan([job('context', 'printf "%s|%s" "$LIMIT" "$WORK" > context',
            resource_environment={'LIMIT':'{cpus}','WORK':'{worktree}'})])
        receipt=self.run_plan(value)
        self.assertTrue(receipt['ok'])
        self.assertEqual((self.root/'context').read_text(),f'1|{self.root}')
        with self.assertRaises(ValueError):
            plan([job('invalid','true',resource_environment={'LIMIT':'{cpus.__class__}'})])
        with self.assertRaises(ValueError):
            plan([job('invalid','true',owned_containers=['an-unrelated-container'])])


if __name__ == "__main__":
    unittest.main()
