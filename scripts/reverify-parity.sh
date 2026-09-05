#!/bin/zsh
# Re-mint every frozen parity record from the LIVE reference toolchain and
# compare it byte for byte against what `tests/data/parity_expected.txt` holds.
#
#   scripts/reverify-parity.sh [CORPUS]
#
# WHY THIS EXISTS. `tests/parity.rs` replays the corpus through the scalars
# frontend and never runs the oracle, so it cannot tell a captured expectation
# from an invented one: a line written from memory rather than measured passes
# there forever, and the only way to make it pass is to break the frontend to
# match it. The JVM, locale and compiler gates in `scripts/parity-oracle.zsh`
# guard the oracle a capture RAN AGAINST; nothing else checks whether a given
# line ever came from one. This does, and it is the check to run before trusting
# any pin a previous round left behind.
#
# It is also the only thing that notices the reference MOVING. The corpus was
# minted under one Scala release and the machine's toolchain updates on its own;
# a record that was right when it was captured and is wrong now looks identical
# to a fabricated one from inside `tests/parity.rs`. The run banner names the
# compiler and the JVM so a disagreement is attributable to a version rather
# than argued about.
#
# This script NEVER invokes the scalars binary, for the same reason
# `capture-parity.sh` does not: an answer from our own frontend would make the
# comparison a tautology.
#
# TWO STAGES, because the fast one has a known artifact. Almost every record is
# an `object T`, so they cannot share a compilation unit — stage one puts each in
# its OWN PACKAGE, compiles the lot in a single `scala --power compile`, and then
# runs each record's own entry class straight through `java -cp`, which is what makes
# this minutes instead of hours: the launcher spends ~6s per program on
# build-server round-trips, a bare `java` run costs ~0.2s. But a package is
# observable — a user class extending a throwable prints its own qualified name,
# so a record whose output contains `Boom: x` re-mints as `p87.Boom: x` — and so
# is the class loader, which the launcher and a bare `java -cp` spell
# differently. Stage two re-captures each FLAGGED record alone, in the default
# package, through `capture-parity.sh` itself — the exact path that minted the
# corpus — and a record that agrees there is clean. Only a record that disagrees
# in stage two is a real finding.
#
# Both the JVM and the locale are pinned by `scripts/parity-oracle.zsh`; the
# reasons are documented there.
emulate -L zsh
set -uo pipefail

here=${0:A:h}
source $here/parity-oracle.zsh
corpus=${1:-$here/../tests/data/parity_expected.txt}
[[ -r $corpus ]] || { print -u2 "reverify-parity: cannot read $corpus"; exit 2 }

parity_resolve_oracle reverify-parity

work=$(mktemp -d) || exit 2
trap 'rm -rf -- $work' EXIT
mkdir -p $work/src

typeset -a prog expect wantexc entry
typeset -i i=0
while IFS= read -r line; do
    [[ -z ${line// } ]] && continue
    (( i++ ))
    typeset -a f
    f=("${(@s:	:)line}")
    (( ${#f} == 2 || ${#f} == 3 )) || {
        print -u2 "reverify-parity: record $i has ${#f} TAB-separated fields, want 2 or 3"
        exit 2
    }
    prog[$i]=${f[1]}
    expect[$i]=${f[2]}
    wantexc[$i]=${f[3]:-}
    # The ENTRY POINT is not always `T`. The corpus holds three shells — a
    # `@main def go`, an `object … extends App`, and an `object … { def main }`
    # — and stage one has to name a class for `java` to run. Guessing `T` for all
    # of them made two records report `ClassNotFoundException: p16.T` as if the
    # corpus were stale, which is the shape of a false finding this whole script
    # exists not to produce. Derived per record, and a record whose entry cannot
    # be derived is a hard error rather than a run that fails for the wrong
    # reason.
    entry[$i]=$(print -r -- "${prog[$i]}" | command perl -ne '
        if (/\@main\s+def\s+(\w+)/)                        { print $1; exit }
        if (/object\s+(\w+)\s+extends\s+App/)              { print $1; exit }
        if (/object\s+(\w+)[^}]*def\s+main\s*\(/)          { print $1; exit }
    ')
    [[ -n ${entry[$i]} ]] || {
        print -u2 "reverify-parity: record $i names no entry point (no \`@main def\`, no"
        print -u2 "reverify-parity: \`object … extends App\`, no \`def main\`): ${prog[$i]}"
        exit 2
    }
    { print -r -- "package p$i"
      print -r -- "${prog[$i]}" | command perl -pe 's/\\n/\n/g' } > $work/src/R$i.scala
done < $corpus
(( i )) || { print -u2 "reverify-parity: $corpus holds no records"; exit 2 }
print -u2 "reverify-parity: $i record(s); compiling as one batch"

# `--print-class-path` writes the classpath on its last stdout line; the build
# chatter goes to stderr.
cp=$($PARITY_SCALA --power compile --print-class-path $work/src 2>$work/kc.txt | tail -1)
if [[ -z $cp ]]; then
    print -u2 "reverify-parity: the batch compile FAILED — the corpus holds a program"
    print -u2 "reverify-parity: Scala $PARITY_SCALA_VERSION rejects, which is itself a finding:"
    command perl -ne 's/\e\[[0-9;]*m//g; print "  $_" if /error/i' $work/kc.txt | head -20 >&2
    exit 3
fi

typeset -a flagged
typeset -i n=0
for j in {1..$i}; do
    "$PARITY_JAVA" $PARITY_JAVAFLAGS -cp "$cp" p$j.${entry[$j]} >$work/o.txt 2>$work/e.txt
    rc=$?
    (( n++ ))
    # Read off DISK. A command substitution strips every trailing newline, which
    # is exactly how a fabricated-looking pin gets minted from a real run.
    got=$(command perl -0pe 's/\n/\\n/g' < $work/o.txt)
    gotexc=$(parity_throwable $work/e.txt)
    if [[ -n ${wantexc[$j]} ]]; then
        # An expected-FAILURE record: the program must still abort, with the same
        # throwable, after the same output.
        if (( rc == 0 )); then
            print -r -- "RECORD $j: frozen as ABORTING with ${wantexc[$j]}, but it now succeeds"
            print -r -- "  program: ${prog[$j]}"
            flagged+=$j
        elif [[ $gotexc != ${wantexc[$j]} || $got != ${expect[$j]} ]]; then
            print -r -- "RECORD $j: re-minted failure differs"
            print -r -- "  program: ${prog[$j]}"
            print -r -- "  frozen:  ${expect[$j]} | ${wantexc[$j]}"
            print -r -- "  live:    $got | ${gotexc:-<no throwable>}"
            flagged+=$j
        fi
    elif (( rc != 0 )); then
        print -r -- "RECORD $j: frozen as SUCCEEDING, but the reference exited $rc"
        print -r -- "  program: ${prog[$j]}"
        print -r -- "  live:    ${gotexc:-<no throwable>}"
        flagged+=$j
    elif [[ $got != ${expect[$j]} ]]; then
        print -r -- "RECORD $j: re-minted output differs"
        print -r -- "  program: ${prog[$j]}"
        print -r -- "  frozen:  ${expect[$j]}"
        print -r -- "  live:    $got"
        flagged+=$j
    fi
done

(( ${#flagged} == 0 )) && {
    print -u2 "reverify-parity: $n record(s) re-minted on Scala $PARITY_SCALA_VERSION / JVM $PARITY_JVM, all agree byte for byte"
    exit 0
}

# STAGE TWO. Re-capture each flagged record ALONE and in the default package,
# through the very script that minted the corpus, so neither stage one's package
# prefix nor its bare-`java` class loader can be mistaken for a bad pin.
print -u2 "reverify-parity: ${#flagged} record(s) flagged; re-capturing each in the default package"
typeset -i real=0
for j in $flagged; do
    print -r -- "${prog[$j]}" > $work/one.txt
    if ! $here/capture-parity.sh $work/one.txt > $work/one.rec 2>$work/one.err; then
        print -r -- "RECORD $j CONFIRMED: capture-parity.sh could not mint it"
        command perl -ne 'print "  $_"' $work/one.err | tail -4
        (( real++ ))
        continue
    fi
    live=$(command perl -pe 's/^.*?\t//' $work/one.rec)
    frozen=${expect[$j]}
    [[ -n ${wantexc[$j]} ]] && frozen="${frozen}	${wantexc[$j]}"
    if [[ $live == $frozen ]]; then
        print -r -- "record $j: clean — stage one's package prefix or class loader, not a bad pin"
    else
        print -r -- "RECORD $j CONFIRMED FABRICATED OR STALE"
        print -r -- "  program: ${prog[$j]}"
        print -r -- "  frozen:  ${frozen}"
        print -r -- "  live:    ${live}"
        (( real++ ))
    fi
done

print -u2 "reverify-parity: $n re-minted, ${#flagged} flagged, $real confirmed"
(( real == 0 ))
