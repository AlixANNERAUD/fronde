import ast, io, json, math, sys, traceback
from contextlib import redirect_stdout
from datetime import date, datetime, time
from decimal import Decimal
from fractions import Fraction
from zoneinfo import ZoneInfo

KEEP = 16 * 1024  # characters of printed output kept
REPLY = 60 * 1024  # bytes of reply, under the host's 64 KiB pipe
REPR = 1000
EXACT = 2**53  # beyond this, a JSON number read as a double loses digits
AGGREGATE = "return an aggregate (counts, sums, a few rows), not the rows"
FIX = "fix the code and run again; only the standard library is available"
STDLIB = (
    "only the standard library is available, no numpy or pandas: use statistics, math, decimal, "
    "fractions, collections, itertools, csv or datetime instead"
)
MEMORY = "use less memory (the limit is 256 MiB): " + AGGREGATE + " and avoid huge lists"


class Capped(io.TextIOBase):
    """Prints, cut at KEEP characters so that a flood cannot fill the memory."""

    def __init__(self):
        self.parts, self.size, self.cut = [], 0, False

    def writable(self):
        return True

    def write(self, s):
        room = KEEP - self.size
        if len(s) > room:
            s, self.cut = s[:room], True
        self.parts.append(s)
        self.size += len(s)
        return len(s)

    def text(self):
        return "".join(self.parts)


def plain(v):
    if v is None or isinstance(v, (bool, str)):
        return v
    if isinstance(v, int):
        return v if -EXACT <= v <= EXACT else str(v)
    if isinstance(v, float):
        return v if math.isfinite(v) else str(v)
    if isinstance(v, (Decimal, Fraction)):
        return str(v)
    if isinstance(v, (datetime, date, time)):
        return v.isoformat()
    if isinstance(v, dict):
        return {k if isinstance(k, str) else str(plain(k)): plain(x) for k, x in v.items()}
    if isinstance(v, (list, tuple, set, frozenset)):
        return [plain(x) for x in v]
    return repr(v)[:REPR]


def describe(e):
    # The exception is the code's: its __str__ or attributes may raise in turn.
    name = type(e).__name__
    try:
        if isinstance(e, SyntaxError) and e.filename == "<code>":
            return f"{name}: {str(e.msg)[:REPR]} (line {int(e.lineno)})"
        message = str(e)[:REPR]
        text = f"{name}: {message}" if message else name
    except BaseException:
        text = name
    try:
        lines = [f.lineno for f in traceback.extract_tb(e.__traceback__) if f.filename == "<code>"]
    except BaseException:
        lines = []
    return f"{text} (line {lines[-1]})" if lines else text


def hint(e):
    if isinstance(e, ModuleNotFoundError):
        return STDLIB
    if isinstance(e, MemoryError):
        return MEMORY
    return FIX


def main(out):
    envelope = json.loads(sys.stdin.buffer.read())
    try:
        tz = ZoneInfo(envelope.get("timezone") or "UTC")
    except Exception:
        tz = ZoneInfo("UTC")
    scope = {"__name__": "__main__", "__builtins__": __builtins__, "data": envelope.get("data"), "TZ": tz}
    printed = Capped()
    reply = {}
    try:
        with redirect_stdout(printed):
            try:
                tree = ast.parse(envelope["code"], "<code>")
                last = None
                if tree.body and isinstance(tree.body[-1], ast.Expr):
                    last = ast.Expression(tree.body.pop().value)
                exec(compile(tree, "<code>", "exec"), scope)
                value = eval(compile(last, "<code>", "eval"), scope) if last else scope.get("result")
            except SystemExit as e:
                if e.code not in (None, 0):
                    raise
                value = scope.get("result")
        reply["value"] = plain(value)
    except BaseException as e:
        reply = {"error": describe(e), "next": hint(e)}
    reply["stdout"] = printed.text()
    if printed.cut:
        reply["stdout_truncated"] = True
    try:
        text = json.dumps(reply, ensure_ascii=False, allow_nan=False)
    except BaseException as e:
        text = json.dumps({"error": f"the result cannot be returned as JSON: {describe(e)}", "next": FIX})
    size = len(text.encode())
    if size > REPLY:
        text = json.dumps(
            {"error": f"the result is {size} bytes, over the limit of {REPLY}", "next": AGGREGATE}
        )
    # On a line of its own, whatever the code left unterminated on stdout.
    out.write(b"\n" + text.encode() + b"\n")
    out.flush()


# The reply goes to the real standard output, saved before the code runs. The host reads its last
# line, so anything the code writes there directly comes before it and is ignored.
main(sys.stdout.buffer)
