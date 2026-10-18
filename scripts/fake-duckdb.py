#!/usr/bin/env python3
"""A duckdb command small enough to read, for the editor's tests.

Takes the same arguments the real one does (`-no-init -csv -nullvalue ''`
and `-c <sql>` any number of times) and answers a few statements with
canned CSV, so the tests and the GUI self-test exercise the whole path
without DuckDB installed. The `CREATE VIEW here AS ...` statement the editor
sends first is accepted and ignored. Deliberately plain: no library, so it
depends on nothing but python3, which ships with the Xcode command line
tools.

Statements it knows:
  --version                  prints a version line
  ... big ...                20,000 rows, for the row cap
  ... FROM here ...          three rows under the header `a,b`
  SELECT 42                  the header `42` and one row `42`
anything else fails like a missing table does.
"""

import os
import sys


def write(text):
    try:
        sys.stdout.write(text)
        sys.stdout.flush()
    except BrokenPipeError:
        # The editor stopped reading (the row cap): leave quietly, with
        # stdout pointed at nowhere so the interpreter's final flush does
        # not complain again.
        os.dup2(os.open(os.devnull, os.O_WRONLY), sys.stdout.fileno())
        sys.exit(0)


def main(argv):
    if "--version" in argv:
        write("v0.0.0 (fake) 0000000000\n")
        return 0
    statements = []
    skip = False
    for index, arg in enumerate(argv):
        if skip:
            skip = False
            continue
        if arg == "-c" and index + 1 < len(argv):
            statements.append(argv[index + 1])
            skip = True
        elif arg == "-nullvalue":
            skip = True
    for sql in statements:
        if sql.lstrip().upper().startswith("CREATE VIEW"):
            continue
        if "big" in sql:
            write("n,word\n")
            for n in range(20000):
                write("%d,row %d\n" % (n, n))
        elif "SELECT name FROM here" in sql:
            write("name\npeople\n")
        elif "FROM here" in sql:
            write("a,b\n1,x\n2,y\n3,z\n")
        elif sql.strip().rstrip(";").upper() == "SELECT 42":
            write("42\n42\n")
        else:
            sys.stderr.write("Catalog Error: Table with name nope does not exist!\n")
            sys.stderr.write("LINE 1: SELECT * FROM nope\n")
            sys.stderr.flush()
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
