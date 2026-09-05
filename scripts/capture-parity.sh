#!/bin/zsh
# Mint frozen parity records from the REAL Scala toolchain.
#
#   scripts/capture-parity.sh new-programs.txt >> tests/data/parity_expected.txt
#
# Reads one program per line from the file named by $1, in the same
# backslash-n encoding `tests/data/parity_expected.txt` uses, runs each through
# the reference `scala`, and writes records to stdout in that same encoding:
#
#   program<TAB>stdout                 the program SUCCEEDED
#   program<TAB>stdout<TAB>exception   the program ABORTED
#
# The third field is the reference's throwable LINE (`<fqcn>: <message>`), not
# its stderr — see the module comment in `tests/parity.rs` for why that is the
# only comparable part of a failing reference run, and `well_formed_exception`
# there for the shape this script must emit.
#
# This script NEVER invokes the scalars binary. The corpus is a record of what
# the reference toolchain does; anything our own frontend produced would make it
# a record of our own output instead, and `tests/parity.rs` — which replays the
# corpus with no Scala installed — would then be asserting a tautology forever.
# Append its stdout to the corpus; never rewrite existing lines by hand.
#
# The reference is resolved, gated and NAMED by `scripts/parity-oracle.zsh`; the
# three axes that decide whether a minted record means anything (the JVM, the
# locale, the compiler version) are documented there.
emulate -L zsh
set -uo pipefail

here=${0:A:h}
source $here/parity-oracle.zsh
src=${1:?usage: capture-parity.sh PROGRAMS-FILE}
[[ -r $src ]] || { print -u2 "capture-parity: cannot read $src"; exit 2 }

parity_resolve_oracle capture-parity

work=$(mktemp -d) || exit 2
trap 'rm -rf -- $work' EXIT

typeset -i n=0 failing=0 bad=0
while IFS= read -r line; do
    [[ -z ${line// } ]] && continue
    printf '%s' "$line" | command perl -pe 's/\\n/\n/g' > $work/T.scala
    # OUTPUT GOES TO A FILE, NOT TO `$(...)`. Command substitution strips EVERY
    # trailing newline; putting one back assumes each program ends in exactly one
    # `println`, and nothing enforces that. `print("x")` would be recorded as
    # `x\n` and three trailing `println()`s as one — a frozen line that is
    # unfalsifiable from the replay side, and whose only way to pass is to break
    # the frontend to match it. Reading the bytes back off disk carries zero, one
    # or many trailing newlines through exactly as written.
    $PARITY_SCALA $PARITY_JVMFLAGS $work/T.scala > $work/o.txt 2> $work/e.txt
    rc=$?
    if (( rc == 0 )); then
        printf '%s\t' "$line"
        command perl -0pe 's/\n/\\n/g' < $work/o.txt
        print
        (( n++ ))
        continue
    fi
    # A non-zero exit is only a RECORD if the program actually ran and threw. A
    # program the compiler REJECTED also exits non-zero and prints nothing, and
    # recording that as an expected failure would freeze "this frontend and the
    # reference both refuse it" — a comparison that never happened.
    exc=$(parity_throwable $work/e.txt)
    if [[ -z $exc ]]; then
        print -u2 "capture-parity: the reference exited $rc without throwing (compile error?): $line"
        command perl -ne 's/\e\[[0-9;]*m//g; print "  $_" if /error/i' $work/e.txt | head -3 >&2
        (( bad++ ))
        continue
    fi
    printf '%s\t' "$line"
    command perl -0pe 's/\n/\\n/g' < $work/o.txt
    printf '\t%s\n' "$exc"
    (( n++ )); (( failing++ ))
done < $src

print -u2 "capture-parity: $n record(s) captured ($failing expected-failure), $bad rejected"
(( bad == 0 ))
