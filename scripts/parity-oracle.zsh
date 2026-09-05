# Shared oracle resolution for the parity scripts. Sourced, never executed.
#
# `scripts/capture-parity.sh` mints frozen records from the reference toolchain
# and `scripts/reverify-parity.sh` re-mints every record already frozen and
# diffs it. Both are only as trustworthy as the reference they ran against, and
# both have to answer the SAME question about it, so the gate lives here once
# rather than in two copies that can drift apart.
#
# ─────────────────────────── THE ORACLE IS THREE THINGS ────────────────────────
#
# `scala` alone does not name a reference. What a Scala program prints is decided
# by the compiler version, the JVM it runs on, and the default locale, and all
# three drift independently on a developer machine.
#
# 1. THE JVM, and `/opt/homebrew/bin/scala` picks it from the AMBIENT JAVA_HOME.
#    The launcher is a shell script whose first act is
#    `JAVA_HOME="${JAVA_HOME:-…}" exec …`, so a `jenv` shim or a per-project
#    `.java-version` silently re-points the reference. Measured on this machine:
#
#      JAVA_HOME=~/.jenv/versions/17   JVM 17.0.4.1   1.0e23 -> 9.999999999999999E22
#      JAVA_HOME=…/openjdk/26.0.2.1    JVM 26.0.2.1   1.0e23 -> 1.0E23
#
#    `Double.toString` was reimplemented in JDK 19 (JDK-4511638, the shortest
#    decimal that round-trips), and Double notation is one of the axes the fuzzer
#    and the corpus are most densely biased toward.
#
#    UNSETTING `JAVA_HOME` IS NOT THE FIX. Measured: with it unset the launcher
#    resolves through `/usr/libexec/java_home`, which on this machine registers
#    only Corretto 17 and 11 — Homebrew's openjdk is not registered there at all
#    — and the run died in `Future timed out after [30 seconds]` starting its
#    compilation server. The JVM has to be NAMED, which is what CAPTURE_JAVA_HOME
#    does.
#
#    The floor is JDK 21, not 19. The corpus pins
#    `java.lang.StringIndexOutOfBoundsException: Range [0, 9) out of bounds for
#    length 2`, which is the `Preconditions.checkFromToIndex` wording the String
#    index faults moved onto in JDK 21; an older JVM spells the same fault
#    `begin 0, end 9, length 2`.
#
# 2. THE LOCALE. `String.format`'s `%f`/`%e`/`%,d` take their decimal and
#    grouping separators from `Locale.getDefault`, and `toUpperCase`/`toLowerCase`
#    take their case-mapping rules from it. The corpus freezes `%.2f`, `%.3e`,
#    `%05d` and `toUpperCase` results, so a capture under `de_DE` would freeze
#    `0,13` for `0.13` and one under `tr` would freeze `Hİ` for `HI`, with nothing
#    in the record to mark it as locale-derived. Pinned to en_US below, which is
#    what scalars itself always formats as.
#
#    The two CONSOLE streams are pinned separately from `file.encoding`, which
#    stopped covering them in JDK 19: with `LANG=C` and only `file.encoding` set,
#    stdout falls back to US-ASCII and non-ASCII output is written as `?`.
#
# 3. THE SCALA COMPILER VERSION, which `scala --version` reports on its SECOND
#    line — the first is the scala-cli runner version and says nothing about the
#    language. Both scripts print it, because a record minted under one compiler
#    and replayed against another is exactly what `reverify-parity.sh` detects,
#    and a run that does not name its compiler cannot be re-run later.
#
# Overrides: SCALARS_ORACLE_SCALA (the launcher), CAPTURE_JAVA_HOME (its JVM).

# The `-D`s that pin the RUN JVM's locale and both console streams. `--java-opt`
# is how the launcher forwards one to the program's JVM.
typeset -ga PARITY_JVMFLAGS=(
    --java-opt -Duser.language=en
    --java-opt -Duser.country=US
    --java-opt -Dfile.encoding=UTF-8
    --java-opt -Dstdout.encoding=UTF-8
    --java-opt -Dstderr.encoding=UTF-8
)

# The plain `java -D` spelling of the same set, for a run that goes straight to
# the JVM rather than through the launcher.
typeset -ga PARITY_JAVAFLAGS=(
    -Duser.language=en
    -Duser.country=US
    -Dfile.encoding=UTF-8
    -Dstdout.encoding=UTF-8
    -Dstderr.encoding=UTF-8
)

# Resolve, gate and NAME the reference toolchain. `$1` is the tag the caller
# prefixes its diagnostics with. Exits 2 on any contaminated axis — a wrong
# oracle is a configuration error, never a warning, because every record it
# touches would be wrong in a way that reads exactly like a real finding.
#
# Sets: PARITY_SCALA, PARITY_JAVA, PARITY_JVM, PARITY_SCALA_VERSION, JAVA_HOME.
parity_resolve_oracle() {
    local tag=${1:?parity_resolve_oracle needs a tag}
    local vbanner probe work
    local -a pl

    PARITY_SCALA=${SCALARS_ORACLE_SCALA:-/opt/homebrew/bin/scala}
    [[ -x $PARITY_SCALA ]] || {
        print -u2 "$tag: $PARITY_SCALA is not executable (set SCALARS_ORACLE_SCALA)"
        exit 2
    }

    # An explicitly named JVM beats an inherited one. There is no third option:
    # an unnamed JAVA_HOME does not mean "the launcher's default", it means
    # whatever the ambient shell exported, which is the failure this file exists
    # to stop.
    export JAVA_HOME=${CAPTURE_JAVA_HOME:-/opt/homebrew/opt/openjdk}
    PARITY_JAVA=$JAVA_HOME/bin/java
    [[ -x $PARITY_JAVA ]] || {
        print -u2 "$tag: no java at $PARITY_JAVA"
        print -u2 "$tag: set CAPTURE_JAVA_HOME to a JDK 21+ home"
        exit 2
    }
    # `java -version` writes to stderr and spells the feature release first.
    local jver
    jver=$("$PARITY_JAVA" -version 2>&1 | command perl -ne 'print $1 and last if /version "(\d+)/')
    if [[ -z $jver ]] || (( jver < 21 )); then
        print -u2 "$tag: $PARITY_JAVA is JDK ${jver:-unknown}; the corpus needs 21 or newer"
        exit 2
    fi

    vbanner=$($PARITY_SCALA --version 2>&1)
    PARITY_SCALA_VERSION=$(print -r -- "$vbanner" |
        command perl -ne 'print $1 and last if /Scala version \(default\):\s*(\S+)/')
    [[ -n $PARITY_SCALA_VERSION ]] || {
        print -u2 "$tag: could not read a Scala version out of \`$PARITY_SCALA --version\`:"
        print -u2 "$vbanner"
        exit 2
    }

    # ── The oracle answers for itself before it is trusted ────────────────────
    # Every claim above is re-measured THROUGH THE LAUNCHER on each run, because
    # the launcher is what actually picks the JVM, and every inference about
    # which one it picked is the thing that would fail silently.
    work=$(mktemp -d) || exit 2
    cat > $work/probe.scala <<'PROBE'
object P {
  def main(args: Array[String]): Unit = {
    println(1.0e23)
    println(System.getProperty("java.version"))
    println(String.format("%,.2f", java.lang.Double.valueOf(1234.5)))
    println("hi".toUpperCase + "I".toLowerCase)
    println(java.util.Locale.getDefault.toString)
  }
}
PROBE
    probe=$($PARITY_SCALA $PARITY_JVMFLAGS $work/probe.scala 2>/dev/null)
    rm -rf -- $work
    pl=(${(f)probe})
    if (( ${#pl} < 5 )); then
        print -u2 "$tag: the oracle at $PARITY_SCALA could not run a program at all"
        print -u2 "$tag: JAVA_HOME=$JAVA_HOME — it cannot be a reference"
        exit 2
    fi
    PARITY_JVM=${pl[2]}
    if [[ ${pl[1]} != 1.0E23 ]]; then
        print -u2 "$tag: the oracle at $PARITY_SCALA runs on JVM $PARITY_JVM (JAVA_HOME=$JAVA_HOME),"
        print -u2 "$tag: whose Double.toString is the pre-JDK-19 one — it prints 1.0e23 as"
        print -u2 "$tag: ${pl[1]}, not 1.0E23. Every Double-notation record it touched would be"
        print -u2 "$tag: spurious. Point CAPTURE_JAVA_HOME at a JDK 21+ home and re-run."
        exit 2
    fi
    if [[ ${pl[3]} != 1,234.50 || ${pl[4]} != HIi ]]; then
        print -u2 "$tag: the oracle at $PARITY_SCALA runs under locale ${pl[5]}, which formats and"
        print -u2 "$tag: case-maps differently from the one the corpus was captured under — it"
        print -u2 "$tag: answers ${pl[3]} / ${pl[4]} where 1,234.50 / HIi is expected. Every %f,"
        print -u2 "$tag: %,d and toUpperCase record it touched would be spurious."
        exit 2
    fi
    print -u2 "$tag: oracle $PARITY_SCALA — Scala $PARITY_SCALA_VERSION on JVM $PARITY_JVM, locale ${pl[5]}"
}

# The throwable LINE a failing reference run reports, read out of the stderr file
# named by `$1`, or empty if the run did not throw.
#
# A failing `scala` run writes ANSI build chatter, then a stack trace whose
# frames name a temp file, and wraps anything raised from an `extends App` body
# in an `ExceptionInInitializerError` — so what is comparable is the INNERMOST
# `Caused by:` when there is one and the `Exception in thread "main"` line
# otherwise, with the marker and the frames stripped. Nothing else in that stderr
# is stable enough to freeze.
#
# Emptiness is meaningful and is the caller's signal that a non-zero exit was a
# COMPILE error rather than a thrown exception: recording that as an expected
# failure would freeze "both sides refuse this program", a comparison that never
# happened.
parity_throwable() {
    command perl -ne '
        s/\e\[[0-9;]*m//g;
        if (/^Caused by:\s*(\S+(?:Exception|Error)\b.*)$/) { $last = $1 }
        elsif (!defined($first) && /^Exception in thread "[^"]*"\s*(\S+(?:Exception|Error)\b.*)$/) { $first = $1 }
        END { my $t = defined($last) ? $last : $first; if (defined $t) { $t =~ s/\s+$//; print $t } }
    ' "$1"
}
