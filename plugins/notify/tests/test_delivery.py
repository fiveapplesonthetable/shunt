"""Delivery properties across acknowledgment loss, concurrent callers and process death."""

import multiprocessing
import os
from pathlib import Path
import sqlite3
import tempfile
import unittest

from shunt_notify import NotSent, Status, deliver_once, inspect_delivery, resolve_uncertain


def _send_while_held(home, marker, ready, release):
    def send():
        Path(marker).write_text("accepted")
        ready.set()
        if not release.wait(10):
            raise TimeoutError("test did not release its transport")
    deliver_once(home, "notice", send)


def _lose_acknowledgment(home, marker):
    def send():
        Path(marker).write_text("accepted")
        os._exit(9)
    deliver_once(home, "notice", send)


class Delivery(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="shunt-notify-tests-")
        self.addCleanup(self.temporary.cleanup)
        self.home = Path(self.temporary.name) / "journal"

    def test_an_acknowledged_key_never_invokes_its_transport_twice(self):
        calls = []
        self.assertEqual(deliver_once(self.home, "notice", lambda: calls.append("accepted")), Status.SENT)
        self.assertEqual(deliver_once(self.home, "notice", lambda: calls.append("duplicate")), Status.ALREADY_SENT)
        self.assertEqual(calls, ["accepted"])
        self.assertIsNotNone(inspect_delivery(self.home, "notice")["sent_at"])

    def test_an_ambiguous_transport_error_requires_an_explicit_finding(self):
        calls = []
        def uncertain():
            calls.append("attempted")
            raise TimeoutError("acceptance is unknown")
        self.assertEqual(deliver_once(self.home, "notice", uncertain), Status.UNCERTAIN)
        self.assertEqual(deliver_once(self.home, "notice", uncertain), Status.UNCERTAIN)
        self.assertEqual(calls, ["attempted"])
        self.assertEqual(inspect_delivery(self.home, "notice")["error_kind"], "TimeoutError")
        self.assertEqual(resolve_uncertain(self.home, "notice", sent=True), Status.SENT)
        self.assertEqual(deliver_once(self.home, "notice", uncertain), Status.ALREADY_SENT)

    def test_an_explicitly_unaccepted_message_can_be_attempted_again(self):
        def refused():
            raise NotSent()
        self.assertEqual(deliver_once(self.home, "notice", refused), Status.NOT_SENT)
        self.assertEqual(deliver_once(self.home, "notice", lambda: None), Status.SENT)

    @unittest.skipUnless(Path("/proc/self/stat").exists(), "requires kernel process start identities")
    def test_concurrent_callers_do_not_duplicate_an_active_transport(self):
        context = multiprocessing.get_context("fork")
        ready, release = context.Event(), context.Event()
        marker = Path(self.temporary.name) / "accepted"
        child = context.Process(target=_send_while_held, args=(self.home, marker, ready, release))
        child.start()
        try:
            self.assertTrue(ready.wait(10))
            duplicate = []
            self.assertEqual(deliver_once(self.home, "notice", lambda: duplicate.append(True)), Status.IN_PROGRESS)
            self.assertEqual(duplicate, [])
            with self.assertRaisesRegex(ValueError, "still active"):
                resolve_uncertain(self.home, "notice", sent=False)
        finally:
            release.set()
            child.join(10)
            if child.is_alive():
                child.kill()
                child.join()
        self.assertEqual(child.exitcode, 0)
        self.assertEqual(inspect_delivery(self.home, "notice")["status"], Status.SENT.value)

    def test_a_process_lost_after_acceptance_is_not_automatically_retried(self):
        context = multiprocessing.get_context("fork")
        marker = Path(self.temporary.name) / "accepted"
        child = context.Process(target=_lose_acknowledgment, args=(self.home, marker))
        child.start()
        child.join(10)
        self.assertFalse(child.is_alive())
        self.assertEqual(child.exitcode, 9)
        duplicate = []
        self.assertEqual(deliver_once(self.home, "notice", lambda: duplicate.append(True)), Status.UNCERTAIN)
        self.assertEqual(duplicate, [])
        self.assertTrue(marker.exists())
        self.assertEqual(resolve_uncertain(self.home, "notice", sent=False), Status.NOT_SENT)
        self.assertEqual(deliver_once(self.home, "notice", lambda: duplicate.append(True)), Status.SENT)
        self.assertEqual(duplicate, [True])

    @unittest.skipUnless(Path("/proc/self/stat").exists(), "requires kernel process start identities")
    def test_a_reused_process_number_cannot_keep_a_dead_claim_active(self):
        deliver_once(self.home, "notice", lambda: None)
        with sqlite3.connect(self.home / "delivery.sqlite") as connection:
            connection.execute("update delivery set status=?,pid=?,starttime=? where key=?",
                               (Status.IN_PROGRESS.value, os.getpid(), "different start identity", "notice"))
        duplicate = []
        self.assertEqual(deliver_once(self.home, "notice", lambda: duplicate.append(True)), Status.UNCERTAIN)
        self.assertEqual(duplicate, [])
