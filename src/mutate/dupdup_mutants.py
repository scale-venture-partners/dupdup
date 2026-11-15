"""Emit mutmut's mutants for source files as byte-range edits.

Reads a JSON request on stdin, ``{"files": [{"file": path, "covered_lines":
[int] | null}]}``, and prints ``{"files": [{"file": path, "mutants": [...]}]}``.
Each mutant is ``{"id", "start", "end", "replacement", "line", "fn_start",
"fn_end"}``: the byte range of the original source it replaces, the
replacement text, the line it sits on, and the line span of the function that
contains it. Uses mutmut's operators through its library API, so mutmut must be
importable in the interpreter that runs this; no ``mutants/`` copy and no mutmut
run is needed.
"""

import json
import sys

import libcst as cst
from libcst.metadata import MetadataWrapper, PositionProvider
from mutmut.mutation.file_mutation import MutationVisitor, get_ignored_lines, mutation_operators


def _offsets(code):
    """Character offset of the start of each line (1-based line numbers)."""
    starts = [0]
    for line in code.splitlines(keepends=True):
        starts.append(starts[-1] + len(line))
    return starts


def _byte_offset(code, starts, line, column):
    return len(code[: starts[line - 1] + column].encode("utf-8"))


def mutants_for(path, covered_lines):
    with open(path, encoding="utf-8") as handle:
        code = handle.read()
    wrapper = MetadataWrapper(cst.parse_module(code))
    ignored = get_ignored_lines(path, code, wrapper, None)
    visitor = MutationVisitor(
        mutation_operators, ignored, set(covered_lines) if covered_lines is not None else None
    )
    wrapper.visit(visitor)
    positions = wrapper.resolve(PositionProvider)
    starts = _offsets(code)
    blank = cst.Module([])
    out = []
    for index, mutation in enumerate(visitor.mutations):
        original = positions.get(mutation.original_node)
        if original is None:
            continue
        start = _byte_offset(code, starts, original.start.line, original.start.column)
        end = _byte_offset(code, starts, original.end.line, original.end.column)
        # The range must hold exactly the node being replaced; otherwise the
        # position and the rendered text disagree and the edit is not safe.
        if code.encode("utf-8")[start:end].decode("utf-8") != blank.code_for_node(mutation.original_node):
            continue
        function = mutation.contained_by_top_level_function
        # A mutant outside any function (a module-level constant) cannot be
        # applied by swapping function code, so it is not offered.
        if function is None:
            continue
        span = positions.get(function)
        out.append(
            {
                "id": f"{path}:{original.start.line}:MUTMUT:{index}",
                "start": start,
                "end": end,
                "replacement": blank.code_for_node(mutation.mutated_node),
                "line": original.start.line,
                "fn_start": span.start.line if span else original.start.line,
                "fn_end": span.end.line if span else original.end.line,
            }
        )
    return out


def main():
    request = json.load(sys.stdin)
    result = []
    for entry in request["files"]:
        result.append({"file": entry["file"], "mutants": mutants_for(entry["file"], entry.get("covered_lines"))})
    json.dump({"files": result}, sys.stdout)


if __name__ == "__main__":
    main()
