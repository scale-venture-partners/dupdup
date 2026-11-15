"""pytest plugin: run mutants by forking, one worker per mutant.

pytest imports the project and collects once. Each mutant then runs in a forked
child that swaps the mutated function's code object into the live function,
runs the candidate tests in process, and reports which failed. No file is
rewritten and no interpreter starts per mutant, so the cost per mutant is a
fork and the tests, not the project's import time.

dupdup writes a jobs file and passes it with ``--dupdup-jobs``; results go to
``--dupdup-out``. Needs Python 3.11+ (``co_qualname``).
"""

import json
import os
import select
import signal
import sys
import time
import types


def pytest_addoption(parser):
    group = parser.getgroup("dupdup")
    group.addoption("--dupdup-jobs", default=None)
    group.addoption("--dupdup-out", default=None)
    group.addoption("--dupdup-workers", type=int, default=4)
    group.addoption("--dupdup-timeout", type=float, default=60.0)
    group.addoption("--dupdup-deadline", type=float, default=None)


def _code_by_qualname(code, out):
    for const in code.co_consts:
        if isinstance(const, types.CodeType):
            out[const.co_qualname] = const
            _code_by_qualname(const, out)
    return out


def _live_function(file, qualname):
    """The function object for ``qualname`` defined in ``file``, or None."""
    module = None
    for mod in list(sys.modules.values()):
        path = getattr(mod, "__file__", None)
        if path and os.path.realpath(path) == os.path.realpath(file):
            module = mod
            break
    if module is None:
        return None
    obj = module
    for part in qualname.split("."):
        if part == "<locals>":
            return None
        # A class's own dict holds the raw function, staticmethod or property.
        raw = obj.__dict__.get(part) if hasattr(obj, "__dict__") else None
        obj = raw if raw is not None else getattr(obj, part, None)
        if obj is None:
            return None
    for attr in ("__func__", "fget"):
        obj = getattr(obj, attr, obj)
    while hasattr(obj, "__wrapped__"):
        obj = obj.__wrapped__
    return obj if isinstance(obj, types.FunctionType) else None


def _apply(job):
    """Swap every changed function of the mutant into the live module.

    Returns None on success, or a reason it could not be applied.
    """
    file = job["file"]
    with open(file, "rb") as handle:
        original = handle.read()
    mutated = original[: job["start"]] + job["replacement"].encode() + original[job["end"] :]
    original_codes = _code_by_qualname(compile(original, file, "exec"), {})
    mutated_codes = _code_by_qualname(compile(mutated, file, "exec"), {})
    changed = [
        q for q, c in mutated_codes.items()
        if q not in original_codes or original_codes[q].co_code != c.co_code
        or original_codes[q].co_consts != c.co_consts or original_codes[q].co_names != c.co_names
    ]
    if not changed:
        return "the mutation did not change any function"
    for qualname in changed:
        fn = _live_function(file, qualname)
        if fn is None:
            return f"could not find {qualname} in the live module"
        new = mutated_codes[qualname]
        if len(new.co_freevars) != len(fn.__code__.co_freevars):
            return f"{qualname} closes over a different set of variables"
        fn.__code__ = new
    return None


class _Timeout(Exception):
    pass


def _run_job(job, items_by_id, timeout):
    """Runs in the forked child. Returns the result dict."""
    reason = _apply(job)
    if reason:
        return {"id": job["id"], "outcome": "unpatchable", "reason": reason, "killers": [], "evaluated": []}

    from _pytest.runner import runtestprotocol

    killers, evaluated = [], []

    def on_alarm(signum, frame):
        raise _Timeout()

    signal.signal(signal.SIGALRM, on_alarm)
    signal.alarm(max(1, int(timeout)))
    try:
        for test_id in job["tests"]:
            for item in items_by_id.get(test_id, []):
                evaluated.append(item.nodeid)
                reports = runtestprotocol(item, nextitem=None, log=False)
                if any(r.failed for r in reports):
                    killers.append(item.nodeid)
    except _Timeout:
        return {"id": job["id"], "outcome": "timeout", "killers": killers, "evaluated": evaluated}
    finally:
        signal.alarm(0)
    return {
        "id": job["id"],
        "outcome": "killed" if killers else "survived",
        "killers": killers,
        "evaluated": evaluated,
    }


def pytest_runtestloop(session):
    config = session.config
    jobs_path = config.getoption("--dupdup-jobs")
    if not jobs_path:
        return None
    with open(jobs_path) as handle:
        jobs = json.load(handle)["mutants"]
    workers = max(1, config.getoption("--dupdup-workers"))
    timeout = config.getoption("--dupdup-timeout")

    # Collected ids include parametrize ids; a job names a test without them,
    # which selects every case of it.
    items_by_id = {}
    for item in session.items:
        for key in {item.nodeid.split("[", 1)[0], item.nodeid}:
            items_by_id.setdefault(key, []).append(item)

    results = []
    running = {}
    queue = list(jobs)
    deadline = config.getoption("--dupdup-deadline")
    while queue or running:
        if deadline is not None and time.time() > deadline:
            queue.clear()
        while queue and len(running) < workers:
            job = queue.pop(0)
            read_fd, write_fd = os.pipe()
            pid = os.fork()
            if pid == 0:
                os.close(read_fd)
                try:
                    result = _run_job(job, items_by_id, timeout)
                except BaseException as exc:  # noqa: BLE001
                    result = {"id": job["id"], "outcome": "unpatchable", "reason": repr(exc), "killers": [], "evaluated": []}
                with os.fdopen(write_fd, "w") as out:
                    json.dump(result, out)
                os._exit(0)
            os.close(write_fd)
            running[read_fd] = (pid, job, [])
        ready, _, _ = select.select(list(running), [], [], 1.0)
        for fd in ready:
            pid, job, chunks = running[fd]
            data = os.read(fd, 65536)
            if data:
                chunks.append(data)
                continue
            os.close(fd)
            os.waitpid(pid, 0)
            del running[fd]
            try:
                results.append(json.loads(b"".join(chunks)))
            except ValueError:
                results.append({"id": job["id"], "outcome": "unpatchable", "reason": "worker crashed", "killers": [], "evaluated": []})
    with open(config.getoption("--dupdup-out"), "w") as handle:
        json.dump(results, handle)
    return True
