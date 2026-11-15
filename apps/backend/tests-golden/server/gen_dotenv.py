"""Golden vectors for the `.env` parser of the Rust settings (`crates/server/src/config.rs`,
`parse_dotenv`): what python-dotenv, which pydantic-settings uses for `env_file`, reads from one
line. Pure function, no files.

Run:   (cd apps/api && .venv/bin/python -I ../backend/tests-golden/server/gen_dotenv.py)
Output: dotenv_cases.json, {"dotenv_version", "cases": [{"line", "values"}]}; `values` is
`dotenv_values` of that line alone ({} when the line binds nothing; a key without `=` maps to
null). The cases are the hand-written LINES, then every binding line of the repository's
`.env.example` as it was when this ran.
"""

from __future__ import annotations

import io
import json
from pathlib import Path

from importlib.metadata import version

from dotenv import dotenv_values

HERE = Path(__file__).resolve().parent

LINES = [
    # Values and inline comments (the .env.example shape first).
    "PHOTON_API_KEY=      # comment",
    "KEY=\t# comment after a tab",
    "KEY= #",
    "KEY=",
    "KEY= ",
    "KEY=#novalue",
    "KEY=#novalue # comment",
    "KEY=value",
    "KEY=value # comment",
    "KEY=value#notcomment",
    "KEY=value\t#tab comment",
    "KEY=a b c # x",
    "KEY=a #b #c",
    "KEY=a  #b",
    "KEY=trailing spaces   ",
    "KEY=http://localhost:8001/#frag",
    "KEY=a=b",
    "KEY=value  ",
    # Quoted values.
    'KEY="quoted value"',
    'KEY="quoted # not a comment"',
    'KEY="quoted" # comment',
    'KEY="quoted"# comment',
    "KEY='single'",
    "KEY='single # x' # y",
    'KEY="esc \\n \\t \\\\ \\" \\\' end"',
    "KEY='esc \\n \\' \\\\ end'",
    'KEY=""',
    'KEY="" # c',
    "KEY=''",
    'KEY= "spaced quoted"',
    'KEY="a" junk',
    'KEY="unterminated',
    "KEY='unterminated",
    'KEY=un"quoted"',
    # Keys, export, whitespace, comments.
    "export KEY=exported",
    "export   KEY=exported2",
    "exportKEY=x",
    "'QKEY'=v",
    "KEY = spaced",
    "  KEY=indented",
    "KEY",
    "# full comment",
    "   # indented comment",
    "=novalue",
    "KEY#x=1",
]


def example_lines() -> list[str]:
    # Every binding line of the repository's .env.example, copied into the vectors so the test
    # does not depend on that file staying as it is.
    path = HERE.parents[3] / ".env.example"
    if not path.exists():
        return []
    return [line for line in path.read_text().splitlines() if line.strip() and not line.lstrip().startswith("#")]


def main() -> None:
    cases = []
    for line in LINES + example_lines():
        values = dotenv_values(stream=io.StringIO(line + "\n"))
        cases.append({"line": line, "values": dict(values)})
    out = HERE / "dotenv_cases.json"
    out.write_text(
        json.dumps({"dotenv_version": version("python-dotenv"), "cases": cases}, indent=1, ensure_ascii=False)
        + "\n"
    )
    print(f"wrote {out.name}: {len(cases)} cases (python-dotenv {version("python-dotenv")})")


if __name__ == "__main__":
    main()
