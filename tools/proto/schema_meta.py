#!/usr/bin/env python3
"""Generate felis-protocol's schema metadata from felis.proto's options.

Usage: schema_meta.py [--check]

Reads the descriptor `buf build` compiles `crates/felis-protocol/proto`
to, and writes `crates/felis-protocol/src/schema_meta.rs`: every family
wrapper's arm-routing table and arm field numbers, each closed enum's
`since_minor`, and the fields added after the base schema. The schema's
shape rules the Rust side relies on are checked here, so a schema that
breaks one fails generation. `--check` exits 1 when the committed file
is stale instead of rewriting it.

"""

import json
import re
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
CRATE = REPO / "crates" / "felis-protocol"
OUT = CRATE / "src" / "schema_meta.rs"
PACKAGE = ".felis.v1."

DIRECTIONS = {
    "DIRECTION_TO_DAEMON": "Direction::ToDaemon",
    "DIRECTION_TO_CLIENT": "Direction::ToClient",
}
CORRELATIONS = {
    "CORRELATION_UNCORRELATED": "CorrelationClass::Uncorrelated",
    "CORRELATION_REQUEST_OPENER": "CorrelationClass::RequestOpener",
    "CORRELATION_REQUEST_REPLY": "CorrelationClass::RequestReply",
    "CORRELATION_STREAM_OPENER": "CorrelationClass::StreamOpener",
    "CORRELATION_STREAM_ITEM": "CorrelationClass::StreamItem",
}
MODES = {
    "MODE_WINDOW": "ModeSet::WINDOW",
    "MODE_OPS": "ModeSet::OPS",
    "MODE_OBSERVER": "ModeSet::OBSERVER",
}
PHASES = {
    "PHASE_HANDSHAKE": "PhaseSet::HANDSHAKE",
    "PHASE_SETUP": "PhaseSet::SETUP",
    "PHASE_ATTACHED": "PhaseSet::ATTACHED",
    "PHASE_OBSERVING": "PhaseSet::OBSERVING",
}
ARM = "[felis.v1.arm]"
FIELD_SINCE = "[felis.v1.field_since]"
VALUE_SINCE = "[felis.v1.value_since]"
WRAPPER = re.compile(r"^(?P<family>[A-Z][A-Za-z]*?)(ToDaemon|ToClient)?Msg$")
CORRELATION_FIELD = 100


class SchemaError(Exception):
    pass


def descriptor():
    image = subprocess.run(
        ["buf", "build", "--exclude-source-info", "-o", "-#format=json"],
        cwd=CRATE,
        check=True,
        capture_output=True,
    ).stdout
    files = json.loads(image)["file"]
    return next(f for f in files if f["name"] == "felis.proto")


def pascal(snake):
    return "".join(part.capitalize() for part in snake.split("_"))


def snake(name):
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


def screaming(name):
    return snake(name).upper()


def walk_messages(messages, prefix=""):
    for message in messages:
        path = prefix + message["name"]
        yield path, message
        yield from walk_messages(message.get("nestedType", []), path + ".")


def walk_enums(proto):
    for enum in proto.get("enumType", []):
        yield enum["name"], enum
    for path, message in walk_messages(proto.get("messageType", [])):
        for enum in message.get("enumType", []):
            yield f"{path}.{enum['name']}", enum


def is_wrapper(message):
    oneofs = message.get("oneofDecl", [])
    return (
        bool(oneofs) and oneofs[0]["name"] == "msg" and WRAPPER.match(message["name"])
    )


def field_since(path, field):
    """The minor a field's `(felis.v1.field_since)` option names, 0 when it
    carries none, and whether it is skippable."""
    option = field.get("options", {}).get(FIELD_SINCE)
    if option is None:
        return 0, False
    minor = int(option.get("minor", 0))
    if minor == 0:
        raise SchemaError(f"{path} names minor 0, the base schema")
    skippable = bool(option.get("skippable", False))
    if skippable and "oneofIndex" in field and not field.get("proto3Optional"):
        raise SchemaError(
            f"{path} is a oneof member: a peer that skips it reads the oneof as "
            "unset, so it cannot be skippable"
        )
    return minor, skippable


def arm_row(wrapper, field):
    option = field.get("options", {}).get(ARM)
    if option is None:
        raise SchemaError(
            f"{wrapper}.{field['name']} is an arm without a (felis.v1.arm) option"
        )
    try:
        direction = DIRECTIONS[option["direction"]]
        correlation = CORRELATIONS[option["correlation"]]
    except KeyError as missing:
        raise SchemaError(f"{wrapper}.{field['name']} names no {missing}") from None
    modes = option.get("modes", [])
    phases = option.get("phases", [])
    if not modes or not phases:
        raise SchemaError(
            f"{wrapper}.{field['name']} is legal on no mode or in no phase"
        )
    since, _ = field_since(f"{wrapper}.{field['name']}", field)
    return {
        "field": field["name"],
        "number": field["number"],
        "direction": direction,
        "correlation": correlation,
        "modes": "ModeSet::NONE" + "".join(f".union({MODES[m]})" for m in modes),
        "phases": "PhaseSet::NONE" + "".join(f".union({PHASES[p]})" for p in phases),
        "since": since,
    }


def wrappers(proto):
    """Every family wrapper's arms, checked against the shape rules the
    driver relies on: a wrapper carries its arms and the correlation
    envelope and nothing else, and a family numbers its arms 1..=n once
    across both directions, in declaration order."""
    found = []
    for message in proto["messageType"]:
        if not is_wrapper(message):
            continue
        name = message["name"]
        family = WRAPPER.match(name)["family"]
        arms = []
        for field in message.get("field", []):
            if field.get("oneofIndex") == 0 and not field.get("proto3Optional"):
                arms.append(arm_row(name, field))
            elif (
                field["number"] == CORRELATION_FIELD and field["name"] == "correlation"
            ):
                continue
            else:
                raise SchemaError(f"{name} declares {field['name']} besides its arms")
        if len(message["oneofDecl"]) != 1:
            raise SchemaError(f"{name} declares a oneof besides msg")
        numbers = [arm["number"] for arm in arms]
        if numbers != sorted(numbers):
            raise SchemaError(f"{name} declares its arms out of number order")
        for arm in arms:
            arm["name"] = f"{family}::{pascal(arm['field'])}"
        found.append((name, family, arms))
    for family in {family for _, family, _ in found}:
        numbers = sorted(
            arm["number"] for _, owner, arms in found if owner == family for arm in arms
        )
        if numbers != list(range(1, len(numbers) + 1)):
            raise SchemaError(f"{family}'s arms do not number 1..={len(numbers)} once")
    return found


def added_fields(proto):
    """Every field carrying `(felis.v1.field_since)`, as the ledger spells
    it, with its minor and whether a present value is withheld from an
    older peer."""
    rows = []
    for path, message in walk_messages(proto["messageType"]):
        if is_wrapper(message):
            continue
        for field in message.get("field", []):
            name = f"{path}.{field['name']}"
            minor, skippable = field_since(name, field)
            if minor:
                rows.append((name, minor, skippable))
    return rows


def payload_oneofs(proto, added):
    """Every payload oneof's members, less the added ones, as
    `Message.oneof: member member …`."""
    added = {name for name, _, _ in added}
    rows = []
    for path, message in walk_messages(proto["messageType"]):
        if is_wrapper(message):
            continue
        for index, oneof in enumerate(message.get("oneofDecl", [])):
            members = [
                field["name"]
                for field in message.get("field", [])
                if field.get("oneofIndex") == index
                and not field.get("proto3Optional")
                and f"{path}.{field['name']}" not in added
            ]
            if members:
                rows.append(f"{path}.{oneof['name']}: {' '.join(members)}")
    return rows


def closed_enums(proto):
    """Every enum a frame can carry, with each value's minor. The routing
    option's vocabulary and the frame-header kind are not field types of
    any frame, so they are left out."""
    rows = []
    for path, enum in walk_enums(proto):
        if path.startswith("ArmRouting.") or path == "FrameKind":
            continue
        prefix = screaming(enum["name"]) + "_"
        values = []
        for value in enum["value"]:
            if not value["name"].startswith(prefix):
                raise SchemaError(
                    f"{path} value {value['name']} lacks the {prefix} prefix"
                )
            minor = int(value.get("options", {}).get(VALUE_SINCE, 0))
            suffix = value["name"][len(prefix) :]
            values.append((pascal(suffix.lower()), suffix, minor))
        *parents, name = path.split(".")
        rust = "::".join(["v1", *(snake(parent) for parent in parents), name])
        rows.append((rust, enum["name"], values))
    return rows


def render(proto):
    out = [
        "// @generated by tools/proto/schema_meta.py from proto/felis.proto (`just proto`).",
        "",
        "use crate::messages::{ArmMeta, CorrelationClass, Direction, ModeSet, PhaseSet};",
        "use crate::minor::Requires;",
        "use crate::wire::v1;",
        "",
    ]
    families = wrappers(proto)
    for name, _, arms in families:
        const = screaming(name)
        out.append(f"pub(crate) const {const}_ARMS: &[ArmMeta] = &[")
        for arm in arms:
            out.append(
                f'    ArmMeta {{ name: "{arm["name"]}", direction: {arm["direction"]}, '
                f"correlation: {arm['correlation']}, modes: {arm['modes']}, "
                f"phases: {arm['phases']}, since_minor: {arm['since']} }},"
            )
        out.append("];")
        numbers = ", ".join(str(arm["number"]) for arm in arms)
        out.append(f"pub(crate) const {const}_ARM_FIELDS: &[u32] = &[{numbers}];")
        out.append("")
    added = added_fields(proto)
    out.append("/// Added fields whose present value is withheld from an older peer.")
    gated = [(name, minor) for name, minor, skippable in added if not skippable]
    for name, minor in gated:
        const = screaming(name.replace(".", "_"))
        out.append(
            f'pub(crate) const {const}: Requires = Requires::new({minor}, "{name}");'
        )
    consts = ", ".join(screaming(name.replace(".", "_")) for name, _ in gated)
    out.append("#[cfg(test)]")
    out.append(f"pub(crate) const GATED_FIELDS: &[Requires] = &[{consts}];")
    out.append("")
    members = "".join(f'\n    "{row}",' for row in payload_oneofs(proto, added))
    out.append("#[cfg(test)]")
    out.append(f"pub(crate) const PAYLOAD_ONEOFS: &[&str] = &[{members}\n];")
    enums = closed_enums(proto)
    additions = (
        [(arm["name"], arm["since"]) for _, _, arms in families for arm in arms]
        + [(name, minor) for name, minor, _ in added]
        + [
            (f"{name}::{ledger}", minor)
            for _, name, rows in enums
            for _, ledger, minor in rows
            if ledger != "UNSPECIFIED"
        ]
    )
    rows = "".join(f'\n    ("{name}", {minor}),' for name, minor in additions)
    out.append("")
    out.append("/// Every arm, added field and closed-enum value, as the ledger spells")
    out.append("/// it, with the minor that added it.")
    out.append("#[cfg(test)]")
    out.append(f"pub(crate) const ADDITIONS: &[(&str, u16)] = &[{rows}\n];")
    annotated = ", ".join(
        f'"{name}"' for _, name, rows in enums if any(minor for _, _, minor in rows)
    )
    out.append("#[cfg(test)]")
    out.append(f"pub(crate) const ANNOTATED_ENUMS: &[&str] = &[{annotated}];")
    for rust, _, values in enums:
        out.append("")
        out.append(f"impl {rust} {{")
        out.append("    /// The protocol minor that defined this value.")
        out.append("    #[must_use]")
        out.append("    pub const fn since_minor(self) -> u16 {")
        out.append("        match self {")
        by_minor = {}
        for variant, _, minor in values:
            by_minor.setdefault(minor, []).append(f"Self::{variant}")
        for minor, variants in sorted(by_minor.items()):
            out.append(f"            {' | '.join(variants)} => {minor},")
        out.append("        }")
        out.append("    }")
        out.append("}")
    source = "\n".join(out) + "\n"
    return subprocess.run(
        ["rustfmt", "--edition", "2024", "--emit", "stdout", "--quiet"],
        input=source.encode(),
        check=True,
        capture_output=True,
    ).stdout.decode()


def main(argv):
    check = argv[1:] == ["--check"]
    try:
        proto = descriptor()
        source = render(proto)
    except SchemaError as error:
        print(f"felis.proto: {error}", file=sys.stderr)
        return 1
    if check:
        if OUT.read_text(encoding="utf-8") != source:
            print(
                f"{OUT.relative_to(REPO)} is stale; run `just proto`", file=sys.stderr
            )
            return 1
        return 0
    OUT.write_text(source, encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
