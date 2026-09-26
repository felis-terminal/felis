#!/usr/bin/env python3
"""The written report for the cross-terminal benchmark suite.

Presentation only: `loaders.py` turns harness artifacts into `Suite`
objects; this renders them as one Markdown page plus one PNG per suite.

Decisions worth knowing before editing:

- **Markdown, not a self-contained HTML page.** The report is read in an
  editor, a diff and a chat window far more often than in a browser, and
  everything the page bought over a table — a hover tooltip, a details
  toggle — was hiding numbers that the table can simply print.
- **matplotlib**, which reverses this module's founding "no third-party
  libraries" rule. Hand-rolled SVG paid for itself while the charts were
  simple bars; faceting, per-panel scales and text that does not collide
  is chart-library work, and `.#bench` is already a shell with a JDK and
  a Zig toolchain in it. It stays out of the default dev shell.
- **One panel per benchmark, each with its own x-axis.** This is the
  reason for the rewrite: vtebench puts a 2 ms payload beside a 99 ms
  one, and on a shared axis every fast row collapses into a sliver that
  cannot be compared with the sliver beside it.
- **One color per terminal, fixed across every chart**, so felis is the
  same hue everywhere and the eye can carry a comparison between suites.
- **One theme.** A PNG cannot follow the reader's, and a light chart
  pasted into a dark document reads worse than it looks — but a dark one
  in a light document, or printed, is unreadable rather than merely
  bright.
- **Every unit gets its own chart.** Two scales on one axis invent a
  relationship that is not in the data.
"""

from __future__ import annotations

import json
import math
import os
import re
from dataclasses import dataclass, field
from pathlib import Path

# The validated categorical order (blue, orange, aqua, yellow, magenta,
# green, violet, red). Adjacent pairs are the gate that matters for bars.
SLOTS = [
    "#2a78d6",
    "#eb6834",
    "#1baf7a",
    "#eda100",
    "#e87ba4",
    "#008300",
    "#4a3aa7",
    "#e34948",
]
# One color slot per entity, in this order, so felis is the same hue in
# every chart. ghostty-tip sits beside ghostty because the pair is what a
# reader compares.
TERM_ORDER = [
    "felis",
    "kitty",
    "alacritty",
    "wezterm",
    "ghostty",
    "ghostty-tip",
    "foot",
]

INK = "#0b0b0b"
INK_2 = "#52514e"
INK_3 = "#77766f"
LINE = "#e6e5e0"
SURFACE = "#ffffff"

# Two columns of panels. One is a column of stripes on a wide screen;
# three squeezes the terminal names out of the y axis.
FACET_COLS = 2
PANEL_W = 5.6
DPI = 144


@dataclass
class Point:
    """One bar: the drawn value, its spread, and how it was arrived at.

    `err` is dispersion inside a leg (hyperfine's runs, vtebench's
    samples); `lo`/`hi` are the smallest and largest per-round values a
    multi-round run saw. They answer different questions, so a chart
    draws one of them and the table prints both.
    """

    value: float
    err: float | None = None
    note: str | None = None
    lo: float | None = None
    hi: float | None = None
    # Rounds this terminal actually produced a value in, which is not
    # the run's round count: a leg can time out in one round and not
    # another.
    rounds: int = 1


def median(values: list[float]) -> float:
    ordered = sorted(values)
    middle = len(ordered) // 2
    if len(ordered) % 2:
        return ordered[middle]
    return (ordered[middle - 1] + ordered[middle]) / 2


def aggregate(points: list[Point]) -> Point:
    """One bar out of one leg per round.

    The median rather than the mean, and the range rather than a
    dispersion estimate: with the three rounds a publishable run takes,
    a mean follows the one round that ran beside a background job and a
    stddev over three points is decoration.
    """
    if len(points) == 1:
        return points[0]
    values = [p.value for p in points]
    errs = [p.err for p in points if p.err is not None]
    notes = [p.note for p in points if p.note]
    return Point(
        median(values),
        median(errs) if errs else None,
        notes[0] if notes else None,
        lo=min(values),
        hi=max(values),
        rounds=len(points),
    )


@dataclass
class Suite:
    key: str
    title: str
    subtitle: str
    unit: str
    better: str  # "lower" | "higher"
    categories: list[str] = field(default_factory=list)
    notes: list[str] = field(default_factory=list)
    # Rounds the run asked for. A terminal that produced fewer of them
    # is the exception the chart labels, so the count belongs here and
    # not only in the points.
    rounds: int = 1
    # term → category → round → the leg that round measured.
    samples: dict[str, dict[str, dict[int, Point]]] = field(default_factory=dict)
    # Display names for terms whose key must stay a terminal name, which
    # the ratio and the color slots are keyed by.
    labels: dict[str, str] = field(default_factory=dict)
    # A second bar of one terminal (felis's warm launch): drawn beside its
    # parent in the parent's color, and never "the best other".
    variant_of: dict[str, str] = field(default_factory=dict)

    @property
    def data(self) -> dict[str, dict[str, Point]]:
        """The presented bars, aggregated over whatever rounds arrived."""
        return {
            term: {
                cat: aggregate(list(by_round.values()))
                for cat, by_round in per_cat.items()
            }
            for term, per_cat in self.samples.items()
        }

    def terminals(self) -> list[str]:
        base = [t for t in self.samples if t not in self.variant_of]
        known = [t for t in TERM_ORDER if t in base]
        ordered = known + sorted(t for t in base if t not in TERM_ORDER)
        out = []
        for term in ordered:
            out.append(term)
            out += sorted(
                v for v, p in self.variant_of.items() if p == term and v in self.samples
            )
        return out

    def label(self, term: str) -> str:
        return self.labels.get(term, term)

    def put(self, term: str, cat: str, point: Point, round_index: int = 1) -> None:
        if cat not in self.categories:
            self.categories.append(cat)
        self.samples.setdefault(term, {}).setdefault(cat, {})[round_index] = point


def fmt(v: float, unit: str) -> str:
    if unit in ("ms", "MB/s", "fps"):
        text = f"{v:,.0f}" if v >= 100 else f"{v:,.1f}"
    elif unit == "MB":
        text = f"{v:,.1f}"
    else:
        text = f"{v:,.2f}"
    return f"{text} {unit}"


def slug(text: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")


# Why a row prints no ratio, in the words the report footnotes it with.
SINGLE_ROUND = "single round, no noise floor"
OVERLAP = "the two ranges overlap"


def ratio(suite: Suite, cat: str) -> tuple[str, str | None]:
    """felis against the best other terminal, and why it went unprinted.

    A ratio is a claim that two bars differ, so it is printed only where
    the run can support one: two decimals over a single leg each is a
    number that a re-run moves, and two overlapping min-max ranges are
    a difference this run did not resolve.
    """
    data = suite.data
    felis = data.get("felis", {}).get(cat)
    others = [
        point
        for term, per_cat in data.items()
        if term != "felis"
        and term not in suite.variant_of
        and (point := per_cat.get(cat))
        and point.value > 0
    ]
    if felis is None or not others or felis.value <= 0:
        return "—", None
    lower = suite.better == "lower"
    best = (
        min(others, key=lambda p: p.value)
        if lower
        else max(others, key=lambda p: p.value)
    )
    if felis.rounds < 2 or best.rounds < 2:
        return "—", SINGLE_ROUND
    if felis.lo <= best.hi and best.lo <= felis.hi:
        return "—", OVERLAP
    value = best.value / felis.value if lower else felis.value / best.value
    return f"{value:.2f}x", None


def ratio_text(suite: Suite, cat: str) -> str:
    return ratio(suite, cat)[0]


# ── charts ───────────────────────────────────────────────────────────


def pyplot():
    """matplotlib, imported here rather than at module scope.

    `crossterm.py run` imports this module before it measures anything,
    and a report-time dependency must not be what stops a two-hour run
    from starting.
    """
    import matplotlib

    matplotlib.use("Agg")  # no display: this runs from a headless driver
    import matplotlib.pyplot as plt

    return plt


def draw(suite: Suite, path: Path) -> None:
    """One figure per suite, one panel per benchmark."""
    plt = pyplot()
    terms = suite.terminals()
    cats = suite.categories
    cols = 1 if len(cats) == 1 else min(FACET_COLS, len(cats))
    rows = math.ceil(len(cats) / cols)
    panel_h = 0.28 * len(terms) + 1.0
    fig, axes = plt.subplots(
        rows,
        cols,
        figsize=(PANEL_W * cols, panel_h * rows),
        squeeze=False,
    )
    fig.patch.set_facecolor(SURFACE)

    for idx, ax in enumerate(axes.flat):
        ax.set_facecolor(SURFACE)
        if idx >= len(cats):
            ax.axis("off")
            continue
        panel(ax, suite, cats[idx], terms)

    fig.suptitle(suite.title, fontsize=12, color=INK, x=0.01, ha="left")
    fig.supxlabel(
        f"{suite.unit} — {suite.better} is better", fontsize=9, color=INK_3, y=0.01
    )
    fig.tight_layout(rect=(0, 0.02, 1, 0.97))
    path.parent.mkdir(parents=True, exist_ok=True)
    fig.savefig(path, dpi=DPI, facecolor=SURFACE)
    plt.close(fig)


def whiskers(
    suite: Suite, points: list[Point | None]
) -> tuple[list[float], list[float]]:
    """One meaning per chart: the between-round range, or the within-leg spread.

    A bar with fewer than two rounds gets no whisker rather than a
    borrowed one — a range needs two values, and drawing the within-leg
    spread there would put two quantities on one axis.
    """
    if suite.rounds < 2:
        errs = [0.0 if p is None or not p.err else p.err for p in points]
        return errs, errs
    spread = [
        (0.0, 0.0) if p is None or p.rounds < 2 else (p.value - p.lo, p.hi - p.value)
        for p in points
    ]
    return [low for low, _ in spread], [high for _, high in spread]


def bar_label(suite: Suite, point: Point) -> str:
    text = fmt(point.value, suite.unit)
    if point.rounds < suite.rounds:
        text += f"  {point.rounds}/{suite.rounds} rounds"
    return text


def panel(ax, suite: Suite, cat: str, terms: list[str]) -> None:
    from matplotlib.colors import to_rgba

    points = [suite.data.get(t, {}).get(cat) for t in terms]
    values = [0.0 if p is None else p.value for p in points]
    lows, highs = whiskers(suite, points)
    ys = range(len(terms))
    # Keyed by TERM_ORDER, not by position among the bars drawn: a chart
    # that leaves one terminal out would otherwise hand its hue to the
    # next one, and kitty would be drawn in felis's blue.
    colors: dict[str, str] = {}
    extra = 0
    for term in terms:
        if term in suite.variant_of:
            continue
        if term in TERM_ORDER:
            colors[term] = SLOTS[TERM_ORDER.index(term) % len(SLOTS)]
        else:
            colors[term] = SLOTS[(len(TERM_ORDER) + extra) % len(SLOTS)]
            extra += 1
    ax.barh(
        list(ys),
        values,
        xerr=[lows, highs] if any(lows) or any(highs) else None,
        color=[
            to_rgba(
                colors[suite.variant_of.get(t, t)],
                0.55 if t in suite.variant_of else 1.0,
            )
            for t in terms
        ],
        height=0.68,
        error_kw={"ecolor": INK_3, "elinewidth": 1, "capsize": 2},
    )
    # Room for the value labels: a bar that ends at the axis puts its own
    # number outside the figure.
    span = max((v + e for v, e in zip(values, highs)), default=1.0) or 1.0
    ax.set_xlim(0, span * 1.28)
    for y, point in enumerate(points):
        if point is None:
            ax.text(
                span * 0.02, y, "not measured", va="center", fontsize=8, color=INK_3
            )
            continue
        ax.text(
            point.value + highs[y] + span * 0.03,
            y,
            bar_label(suite, point),
            va="center",
            fontsize=8,
            color=INK_2,
        )
    ax.set_yticks(list(ys), [suite.label(t) for t in terms], fontsize=9, color=INK_2)
    for term, label in zip(terms, ax.get_yticklabels()):
        # The one terminal the reader came for.
        if suite.variant_of.get(term, term) == "felis":
            label.set_color(INK)
            label.set_fontweight("bold")
    ax.invert_yaxis()
    ax.set_title(cat, fontsize=10, color=INK, loc="left", pad=6)
    ax.tick_params(axis="x", labelsize=8, colors=INK_3)
    ax.grid(axis="x", color=LINE, linewidth=0.8)
    ax.set_axisbelow(True)
    for side in ("top", "right", "left"):
        ax.spines[side].set_visible(False)
    ax.spines["bottom"].set_color(LINE)


# ── provenance ───────────────────────────────────────────────────────


def section(meta: dict, key: str) -> dict:
    """A meta.json block, tolerating a results root written by an older run."""
    value = meta.get(key)
    return value if isinstance(value, dict) else {}


# The parameters that describe the condition every terminal was held
# to, as opposed to the workload knobs.
PINNED_KEYS = (
    "GRID_COLS",
    "GRID_ROWS",
    "FONT_FAMILY",
    "FONT_PT",
    "FONT_PX",
    "FONT_SCALE",
)


def pinned_field(params: dict) -> str:
    """Grid and font, as one line — "120x30 cells · Menlo 9pt"."""
    parts = []
    if params.get("GRID_COLS") and params.get("GRID_ROWS"):
        parts.append(f"{params['GRID_COLS']}x{params['GRID_ROWS']} cells")
    if params.get("FONT_FAMILY") and params.get("FONT_PT"):
        font = f"{params['FONT_FAMILY']} {params['FONT_PT']}pt"
        # felis is pinned through `font.size`, which the field now sets
        # to the point size itself. Shown only when the two disagree, so
        # a run recorded before that — when the pin was pre-multiplied
        # by the backing scale — still reads as the condition it was.
        px = params.get("FONT_PX")
        if px and px != params["FONT_PT"]:
            font += f" (felis {px}px)"
        parts.append(font)
    return " · ".join(parts)


def machine_rows(meta: dict) -> list[tuple[str, str]]:
    machine = section(meta, "machine")
    os_info = section(meta, "os")
    rows: list[tuple[str, str]] = []
    if host := meta.get("host"):
        rows.append(("host", host))
    cpu = machine.get("cpu")
    if cpu:
        cores = machine.get("cpu_logical")
        perf, eff = (
            machine.get("cpu_performance_cores"),
            machine.get("cpu_efficiency_cores"),
        )
        detail = f" · {cores} cores" if cores else ""
        if perf and eff:
            detail = f" · {perf}P + {eff}E cores"
        rows.append(("cpu", f"{cpu}{detail}"))
    if mem := machine.get("memory_bytes"):
        rows.append(("memory", f"{mem / 1024**3:.0f} GB"))
    for gpu in machine.get("gpus") or []:
        label = gpu.get("model") or "?"
        if gpu.get("cores"):
            label += f" · {gpu['cores']} cores"
        rows.append(("gpu", label))
    for display in machine.get("displays") or []:
        label = display.get("resolution") or display.get("name") or "?"
        if scale := display.get("backing_scale"):
            label += f" · {scale:g}x"
        rows.append(("display", label))
    if os_info.get("name"):
        version = " ".join(filter(None, [os_info.get("version"), os_info.get("build")]))
        rows.append(("os", f"{os_info['name']} {version}".strip()))
    power = machine.get("power") if isinstance(machine.get("power"), dict) else {}
    if power.get("source"):
        label = power["source"]
        if power.get("battery_percent") is not None:
            label += f" ({power['battery_percent']}%)"
        # A speed limit below 100 means the machine was throttling, which
        # is the first thing to check when a run disagrees with an older one.
        if (limit := power.get("cpu_speed_limit")) is not None and limit < 100:
            label += f" · CPU speed limit {limit}%"
        rows.append(("power", label))
    if machine.get("governor"):
        rows.append(("governor", machine["governor"]))
    if (load := machine.get("load_average")) is not None:
        cores = machine.get("cpu_logical") or 0
        # Flagged rather than merely printed: a run started under load
        # produces numbers that look like a regression and are not.
        busy = " — the machine was busy; re-run" if cores and load > cores / 2 else ""
        rows.append(("load at start", f"{load:g}{busy}"))
    felis = section(meta, "felis")
    if felis.get("revision"):
        built = f" · built {felis['built']}" if felis.get("built") else ""
        # Flagged in the table, not only on the console the run printed
        # to: the report outlives that scrollback, and a chart credited
        # to a commit that does not contain the change is worse than one
        # credited to nothing.
        drift = f" — !! {felis['drift']}" if felis.get("drift") else ""
        rows.append(("felis", f"{felis['revision']}{built}{drift}"))
    params = section(meta, "params")
    # The pinned condition gets its own row rather than being buried in
    # the parameter dump: it is what makes the bars comparable at all,
    # and a reader checking "was this a fair test" should not have to
    # find it among the workload knobs.
    if pinned := pinned_field(params):
        rows.append(("pinned field", pinned))
    if rest := {k: v for k, v in params.items() if k not in PINNED_KEYS}:
        rows.append(
            ("suite params", " ".join(f"{k}={v}" for k, v in sorted(rest.items())))
        )
    if date := meta.get("date"):
        rows.append(("run", date))
    return rows


# ── markdown ─────────────────────────────────────────────────────────


def cell(text: str) -> str:
    """A pipe in a value would end the column it is standing in."""
    return text.replace("|", "\\|")


def table(head: list[str], rows: list[list[str]]) -> str:
    out = [
        "| " + " | ".join(cell(h) for h in head) + " |",
        "|" + "|".join(["---"] * len(head)) + "|",
    ]
    out += ["| " + " | ".join(cell(c) for c in row) + " |" for row in rows]
    return "\n".join(out)


def suite_table(suite: Suite) -> str:
    terms = suite.terminals()
    rows = []
    for cat in suite.categories:
        row = [cat]
        for term in terms:
            p = suite.data.get(term, {}).get(cat)
            if p is None:
                row.append("—")
                continue
            text = fmt(p.value, suite.unit)
            if p.lo is not None and p.hi is not None:
                text += f" [{p.lo:,.1f}–{p.hi:,.1f}]"
            if p.err:
                text += f" ± {p.err:,.1f}"
            if suite.rounds > 1:
                text += f" · {p.rounds}/{suite.rounds} rounds"
            if p.note:
                text += f" ({p.note})"
            row.append(text)
        row.append(ratio_text(suite, cat))
        rows.append(row)
    return table(["", *(suite.label(t) for t in terms), "felis vs best other"], rows)


def ratio_footnotes(suite: Suite) -> list[str]:
    """Why the ratio column is empty where it is, said once per reason."""
    reasons = {why for cat in suite.categories if (why := ratio(suite, cat)[1])}
    out = []
    if SINGLE_ROUND in reasons:
        note = (
            "No ratio where felis or its best competitor measured only one "
            f"round ({SINGLE_ROUND})."
        )
        if suite.rounds < 2:
            note += " Re-run with ROUNDS=3 to earn one."
        out.append(note)
    if OVERLAP in reasons:
        out.append(
            "No ratio where the two min–max ranges overlap: a difference this "
            "run did not resolve."
        )
    return out


def tools_table(meta: dict) -> str:
    tools = meta.get("tools") or []
    if not tools:
        return ""
    rows = []
    for tool in tools:
        if not tool.get("found"):
            rows.append([tool["name"], "not found", "—"])
            continue
        origin = "pinned"
        if tool.get("source") != "flake":
            # The digest beside the marker, because the version column is
            # only as precise as the binary's own banner: a local build of
            # anything can print a release number.
            short = (tool.get("sha256") or "")[:12]
            origin = f"unpinned · {short}" if short else "unpinned"
        rows.append([tool["name"], tool.get("version") or "—", origin])
    out = ["### Tools", "", table(["tool", "version", "source"], rows)]
    if not meta.get("pinned_shell", True):
        out += [
            "",
            "> Run outside `nix develop .#bench`: the field came from whatever "
            "this host had installed, so another machine will not reproduce it.",
        ]
    return "\n".join(out)


def render_markdown(
    suites: list[Suite], meta: dict, title: str, images: dict[str, str]
) -> str:
    out = [
        f"# {title}",
        "",
        "Every terminal ran the same payloads at the same grid on this machine.",
        "felis is always the first color.",
    ]
    if rows := machine_rows(meta):
        out += ["", "## Run", "", table(["", ""], [[k, v] for k, v in rows])]
    if tools := tools_table(meta):
        out += ["", tools]
    for suite in suites:
        out += ["", f"## {suite.title}", "", suite.subtitle]
        if image := images.get(suite.key):
            out += ["", f"![{suite.title}]({image})"]
        out += ["", suite_table(suite), ""]
        out += [f"- {note}" for note in suite.notes]
        out.append(
            "- felis vs best other: above 1.00x means felis leads the field on that row."
        )
        out += [f"- {note}" for note in ratio_footnotes(suite)]
    return "\n".join(out) + "\n"


def write(
    root: Path,
    suites: list[Suite],
    out: Path,
    png_dir: Path | None,
    title: str,
) -> None:
    meta = {}
    if (root / "meta.json").exists():
        try:
            meta = json.loads((root / "meta.json").read_text())
        except json.JSONDecodeError:
            pass
    images: dict[str, str] = {}
    if png_dir:
        try:
            for suite in suites:
                path = png_dir / f"{slug(suite.key)}.png"
                draw(suite, path)
                images[suite.key] = Path(os.path.relpath(path, out.parent)).as_posix()
        except ModuleNotFoundError:
            # The tables are the report; the charts are how it is read.
            # Say which half is missing rather than failing the run that
            # produced the numbers.
            print(
                "  charts skipped: matplotlib is missing. Re-render inside "
                "`nix develop .#bench` to get them.",
            )
            images = {}
    out.write_text(render_markdown(suites, meta, title, images))
    print(f"report: {out}")
    for suite in suites:
        if image := images.get(suite.key):
            print(f"  png: {out.parent / image}")
    for suite in suites:
        print(
            f"  {suite.key}: {len(suite.categories)} rows x {len(suite.terminals())} terminals"
        )
