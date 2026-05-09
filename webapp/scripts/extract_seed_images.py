#!/usr/bin/env python3
"""Extract fixed campaign images from seed.sql into JPEG files.

Usage:
    python3 webapp/scripts/extract_seed_images.py webapp/sql/seed.sql webapp/images/seed

The output files are named <campaign_id>.jpg. The script is intentionally
standard-library only so it can run on competition servers.
"""

from __future__ import annotations

import argparse
import binascii
from pathlib import Path


def iter_campaign_insert_statements(sql: str):
    marker = "INSERT INTO `campaigns`"
    pos = 0
    while True:
        start = sql.find(marker, pos)
        if start < 0:
            return
        in_quote = False
        i = start
        while i < len(sql):
            ch = sql[i]
            if ch == "'":
                if in_quote and i + 1 < len(sql) and sql[i + 1] == "'":
                    i += 2
                    continue
                in_quote = not in_quote
            elif ch == ";" and not in_quote:
                yield sql[start : i + 1]
                pos = i + 1
                break
            i += 1
        else:
            raise ValueError("unterminated INSERT INTO `campaigns` statement")


def parse_sql_string(token: str) -> str:
    token = token.strip()
    if not (token.startswith("'") and token.endswith("'")):
        raise ValueError(f"expected SQL string, got {token[:80]!r}")
    return token[1:-1].replace("''", "'").replace("\\'", "'")


def parse_hex_blob(token: str) -> bytes:
    token = token.strip()
    if len(token) < 3 or token[0].lower() != "x" or token[1] != "'" or token[-1] != "'":
        raise ValueError(f"expected hex blob, got {token[:80]!r}")
    return binascii.unhexlify(token[2:-1])


def iter_tuples(values_sql: str):
    i = 0
    while i < len(values_sql):
        if values_sql[i] != "(":
            i += 1
            continue
        i += 1
        fields: list[str] = []
        buf: list[str] = []
        in_quote = False
        while i < len(values_sql):
            ch = values_sql[i]
            if ch == "'":
                buf.append(ch)
                if in_quote and i + 1 < len(values_sql) and values_sql[i + 1] == "'":
                    buf.append(values_sql[i + 1])
                    i += 2
                    continue
                in_quote = not in_quote
            elif ch == "," and not in_quote:
                fields.append("".join(buf).strip())
                buf.clear()
            elif ch == ")" and not in_quote:
                fields.append("".join(buf).strip())
                yield fields
                i += 1
                break
            else:
                buf.append(ch)
            i += 1
        else:
            raise ValueError("unterminated tuple in campaign INSERT")


def extract(seed_sql: Path, out_dir: Path) -> int:
    sql = seed_sql.read_text(encoding="utf-8")
    out_dir.mkdir(parents=True, exist_ok=True)
    count = 0
    for statement in iter_campaign_insert_statements(sql):
        values_idx = statement.upper().find(" VALUES")
        if values_idx < 0:
            continue
        values_sql = statement[values_idx + len(" VALUES") :]
        for fields in iter_tuples(values_sql):
            if len(fields) < 6:
                raise ValueError(f"campaign tuple has {len(fields)} fields, expected >= 6")
            campaign_id = parse_sql_string(fields[0])
            image = parse_hex_blob(fields[5])
            (out_dir / f"{campaign_id}.jpg").write_bytes(image)
            count += 1
    return count


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("seed_sql", type=Path)
    parser.add_argument("out_dir", type=Path)
    args = parser.parse_args()
    count = extract(args.seed_sql, args.out_dir)
    print(f"extracted {count} campaign images to {args.out_dir}")


if __name__ == "__main__":
    main()
