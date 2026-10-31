"""Versioned job plans with literal environment values and declared resources."""

from __future__ import annotations

from dataclasses import dataclass, field, fields
from pathlib import PurePosixPath
import re
from string import Formatter


def _record(kind, value):
    if not isinstance(value, dict) or set(value) - {item.name for item in fields(kind)}:
        raise ValueError(f"invalid {kind.__name__} fields")
    return kind(**value)


def _environment(value):
    if not isinstance(value, dict) or any(
        not isinstance(key, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z_0-9]*", key)
        or not isinstance(text, str) or "\0" in text for key, text in value.items()
    ):
        raise ValueError("environment entries require names and literal NUL-free strings")


def _directory(value):
    if not isinstance(value, str) or "\0" in value or PurePosixPath(value).is_absolute() or ".." in PurePosixPath(value).parts:
        raise ValueError("working directories must stay inside the workspace")


def _template(value):
    try:
        for _, name, specification, conversion in Formatter().parse(value):
            if name is not None and (name not in {"cpus", "name", "worktree", "service", "network"}
                                     or specification or conversion):
                raise ValueError("unknown resource template field")
    except (ValueError, TypeError) as error:
        raise ValueError("resource templates accept only named execution resources") from error


@dataclass(frozen=True)
class Step:
    """A shell step whose output channels cannot execute environment-file content."""

    run: str
    directory: str = ""
    environment: dict[str, str] = field(default_factory=dict)

    def validate(self):
        if not isinstance(self.run, str) or not self.run.strip() or "\0" in self.run:
            raise ValueError("each step requires a NUL-free command")
        _directory(self.directory)
        _environment(self.environment)


@dataclass(frozen=True)
class Service:
    """An owned service container with a bounded health wait."""

    alias: str
    image: str
    environment: dict[str, str] = field(default_factory=dict)
    options: list[str] = field(default_factory=list)
    command: list[str] = field(default_factory=list)
    health_timeout: int = 120

    def validate(self):
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", self.alias) or not self.image:
            raise ValueError("services need an alias and image")
        _environment(self.environment)
        for argv in (self.options, self.command):
            if not isinstance(argv, list) or any(not isinstance(arg, str) or "\0" in arg for arg in argv):
                raise ValueError("service arguments must be literal strings")
        if type(self.health_timeout) is not int or not 1 <= self.health_timeout <= 3600:
            raise ValueError("service health_timeout must be between 1 and 3600 seconds")


@dataclass(frozen=True)
class Mount:
    """A declared cache or dependency mount with explicit write ownership."""

    host: str
    inside: str
    read_only: bool = False

    def validate(self):
        if not all(isinstance(path, str) and path.startswith("/") and ":" not in path and "\0" not in path for path in (self.host, self.inside)):
            raise ValueError("mounts require absolute host and container paths without colons")
        if type(self.read_only) is not bool:
            raise ValueError("read_only must be a boolean")


@dataclass(frozen=True)
class Job:
    """One host or container job with dependencies and private execution resources."""

    name: str
    steps: list[Step]
    after: list[str] = field(default_factory=list)
    image: str | None = None
    service: Service | None = None
    environment: dict[str, str] = field(default_factory=dict)
    resource_environment: dict[str, str] = field(default_factory=dict)
    mounts: list[Mount] = field(default_factory=list)
    cpus: int = 1
    timeout: int = 3600
    host_scope: bool = False
    owned_containers: list[str] = field(default_factory=list)
    tmpfs: dict[str, str] = field(default_factory=dict)

    def validate(self):
        if not isinstance(self.name, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", self.name):
            raise ValueError("job names require letters, digits, dots, underscores or dashes")
        if not isinstance(self.steps, list) or not self.steps:
            raise ValueError("a job requires its actual steps")
        for step in self.steps:
            step.validate()
        _environment(self.environment)
        _environment(self.resource_environment)
        for text in self.resource_environment.values():
            _template(text)
        if type(self.cpus) is not int or self.cpus < 1 or type(self.timeout) is not int or not 1 <= self.timeout <= 86400:
            raise ValueError("jobs need positive CPUs and a timeout between 1 and 86400 seconds")
        if not isinstance(self.after, list) or any(not isinstance(name, str) for name in self.after):
            raise ValueError("job dependencies must be names")
        if type(self.host_scope) is not bool:
            raise ValueError("host_scope must be a boolean")
        if self.image is not None and (not isinstance(self.image, str) or not self.image or self.image.startswith('-') or '\0' in self.image):
            raise ValueError("container jobs require an image")
        if self.service:
            self.service.validate()
            if not self.image:
                raise ValueError("service dependencies require an isolated container job")
        for mount in self.mounts:
            mount.validate()
        if not isinstance(self.owned_containers, list):
            raise ValueError("owned containers must be a list of resource templates")
        for name in self.owned_containers:
            if not isinstance(name,str) or not re.fullmatch(r"\{name\}-[A-Za-z0-9_.-]+",name):
                raise ValueError("owned container names must begin with the job's {name}- prefix")
        if not isinstance(self.tmpfs, dict) or any(not isinstance(path,str) or not path.startswith('/')
            or ':' in path or '..' in PurePosixPath(path).parts or not isinstance(options,str) or '\0' in path+options
            for path,options in self.tmpfs.items()):
            raise ValueError("tmpfs entries require absolute container paths and literal options")


@dataclass(frozen=True)
class Plan:
    """A complete plan; unknown fields and missing dependencies prevent execution."""

    version: int
    jobs: list[Job]
    reserved_cpus: int = 2
    cgroup_parent: str | None = None

    @classmethod
    def parse(cls, value):
        if not isinstance(value, dict):
            raise ValueError("a job plan must be an object")
        value = dict(value)
        jobs = []
        for entry in value.get("jobs", []):
            entry = dict(entry)
            entry["steps"] = [_record(Step, step) for step in entry.get("steps", [])]
            entry["mounts"] = [_record(Mount, mount) for mount in entry.get("mounts", [])]
            if entry.get("service") is not None:
                entry["service"] = _record(Service, entry["service"])
            jobs.append(_record(Job, entry))
        value["jobs"] = jobs
        plan = _record(cls, value)
        plan.validate()
        return plan

    def validate(self):
        if type(self.version) is not int or self.version != 1:
            raise ValueError("unsupported job-plan version")
        if not self.jobs or type(self.reserved_cpus) is not int or self.reserved_cpus < 0:
            raise ValueError("a plan needs jobs and a nonnegative reserved CPU count")
        if self.cgroup_parent is not None and not re.fullmatch(r"[A-Za-z0-9_.@/-]+", self.cgroup_parent):
            raise ValueError("invalid container cgroup parent")
        names = {job.name for job in self.jobs}
        if len(names) != len(self.jobs):
            raise ValueError("job names must be distinct")
        for job in self.jobs:
            job.validate()
            if job.name in job.after or not set(job.after) <= names:
                raise ValueError("job dependencies must name other jobs in this plan")
        pending = {job.name: set(job.after) for job in self.jobs}
        done = set()
        while pending:
            ready = {name for name, after in pending.items() if after <= done}
            if not ready:
                raise ValueError("job dependencies contain a cycle")
            done |= ready
            pending = {name: after for name, after in pending.items() if name not in ready}
