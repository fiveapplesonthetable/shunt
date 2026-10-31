"""Durable delivery claims whose uncertain outcomes are never retried automatically."""

from contextlib import contextmanager
from enum import Enum
import json
import os
from pathlib import Path
import sqlite3
import time


class Status(str, Enum):
    """The evidence available about one notification's delivery."""

    SENT = "sent"
    ALREADY_SENT = "already_sent"
    IN_PROGRESS = "in_progress"
    UNCERTAIN = "uncertain"
    NOT_SENT = "not_sent"


class NotSent(Exception):
    """A transport's explicit guarantee that the notification was not accepted."""


def _identity(pid):
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rpartition(")")[2].split()
        return fields[19]
    except (OSError, IndexError):
        return None


@contextmanager
def _journal(home):
    home = Path(home)
    home.mkdir(mode=0o700, parents=True, exist_ok=True)
    path = home / "delivery.sqlite"
    descriptor = os.open(path, os.O_CREAT | os.O_RDWR | getattr(os, "O_NOFOLLOW", 0), 0o600)
    os.close(descriptor)
    connection = sqlite3.connect(path, timeout=5)
    connection.row_factory = sqlite3.Row
    try:
        connection.execute("pragma synchronous=FULL")
        connection.execute("""create table if not exists delivery (
            key text primary key, status text not null, pid integer, starttime text,
            started_at integer, sent_at integer, error_kind text)""")
        connection.commit()
        yield connection
    finally:
        connection.close()


def _active(row):
    return row["starttime"] is not None and _identity(row["pid"]) == row["starttime"]


def _finish(home, key, status, error_kind=None):
    with _journal(home) as connection:
        connection.execute("update delivery set status=?, sent_at=?, error_kind=? where key=?",
                           (status.value, int(time.time()) if status == Status.SENT else None, error_kind, key))
        connection.commit()


def deliver_once(home, key, send):
    """Calls the transport once per key, retaining ambiguous outcomes for explicit resolution."""
    if not isinstance(key, str) or not key or "\0" in key:
        raise ValueError("a notification requires a nonempty key")
    with _journal(home) as connection:
        connection.execute("begin immediate")
        row = connection.execute("select * from delivery where key=?", (key,)).fetchone()
        if row:
            status = Status(row["status"])
            if status == Status.SENT:
                return Status.ALREADY_SENT
            if status == Status.UNCERTAIN:
                return status
            if status == Status.IN_PROGRESS:
                if _active(row):
                    return status
                connection.execute("update delivery set status=? where key=?", (Status.UNCERTAIN.value, key))
                connection.commit()
                return Status.UNCERTAIN
        connection.execute("""insert into delivery(key,status,pid,starttime,started_at)
            values(?,?,?,?,?) on conflict(key) do update set status=excluded.status,
            pid=excluded.pid,starttime=excluded.starttime,started_at=excluded.started_at,
            sent_at=null,error_kind=null""",
                           (key, Status.IN_PROGRESS.value, os.getpid(), _identity(os.getpid()), int(time.time())))
        connection.commit()
    try:
        send()
    except NotSent:
        _finish(home, key, Status.NOT_SENT)
        return Status.NOT_SENT
    except BaseException as error:
        _finish(home, key, Status.UNCERTAIN, type(error).__name__)
        if not isinstance(error, Exception):
            raise
        return Status.UNCERTAIN
    _finish(home, key, Status.SENT)
    return Status.SENT


def inspect_delivery(home, key):
    """Returns the recorded outcome and process identity for an operator."""
    with _journal(home) as connection:
        row = connection.execute("select * from delivery where key=?", (key,)).fetchone()
        return dict(row) if row else None


def resolve_uncertain(home, key, *, sent):
    """Records an operator's delivery finding without invoking the transport."""
    if type(sent) is not bool:
        raise ValueError("resolution requires an explicit delivery finding")
    with _journal(home) as connection:
        connection.execute("begin immediate")
        row = connection.execute("select * from delivery where key=?", (key,)).fetchone()
        if not row or Status(row["status"]) not in {Status.UNCERTAIN, Status.IN_PROGRESS}:
            raise ValueError("notification has no uncertain delivery to resolve")
        if Status(row["status"]) == Status.IN_PROGRESS and _active(row):
            raise ValueError("notification transport is still active")
        status = Status.SENT if sent else Status.NOT_SENT
        connection.execute("update delivery set status=?,sent_at=?,error_kind=null where key=?",
                           (status.value, int(time.time()) if sent else None, key))
        connection.commit()
        return status


def main(argv=None):
    """Inspects a receipt or records an explicit finding about uncertain delivery."""
    import argparse
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--home", required=True, type=Path)
    commands = parser.add_subparsers(dest="command", required=True)
    status = commands.add_parser("status")
    status.add_argument("key")
    resolve = commands.add_parser("resolve")
    resolve.add_argument("key")
    finding = resolve.add_mutually_exclusive_group(required=True)
    finding.add_argument("--sent", action="store_true")
    finding.add_argument("--not-sent", action="store_true")
    args = parser.parse_args(argv)
    try:
        if args.command == "resolve":
            resolve_uncertain(args.home, args.key, sent=args.sent)
        print(json.dumps(inspect_delivery(args.home, args.key)))
        return 0
    except (ValueError, OSError, sqlite3.Error) as error:
        parser.exit(2, f"shunt-notify: {error}\n")
