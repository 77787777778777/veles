#!/usr/bin/env perl
# scripts/i18n-wrap-rs.pl — mechanically wrap Rust user-visible strings.
#
# WHY PERL: the transform is regex-heavy over a language whose literals
# contain braces and quotes, and Tcl re-quotes both (two rounds have
# already been lost to "unbalanced brace in comment"). Perl's regexes
# plus a hand-rolled scanner are the tool this job wanted.
#
# usage: i18n-wrap-rs.pl <repo root> [--dry]
#
# SCOPE — an ALLOWLIST of call sites, not a global rewrite. A blanket
# `format!` rewrite would touch URLs, config-file text, process names
# and cache keys, which are not UI strings. Only the argument of
# `push_msg_status(` is rewritten, and only when its shape can be
# proven:
#
#   push_msg_status("Text".into())          -> lookup("Text", &[])
#   push_msg_status(format!("T {a}", a))    -> lookup("T %{a}", &[("a", a.to_string())])
#
# Multi-line calls are handled by paren scanning, not by a greedy
# regexp: `push_msg_status(format!(\n  "…"\n))` is common in this file.
# Anything else (a format spec like {:<20}, a method call as an
# argument, an escape this script cannot interpret) is REPORTED and
# left alone — a half-wrapped literal is worse than an unwrapped one.

use strict;
use warnings;

my ($root, $dry) = (shift @ARGV, 0);
for my $a (@ARGV) { $dry = 1 if $a eq '--dry'; }
die "usage: i18n-wrap-rs.pl <root> [--dry]\n" unless defined $root && -d $root;

my @files;
sub walk {
    my ($dir) = @_;
    for my $e (sort glob("$dir/*")) {
        next if $e =~ /\/target(\/|$)/;
        if (-d $e) { walk($e) }
        elsif ($e =~ /\.rs$/) { push @files, $e }
    }
}
walk("$root/crates");

my ($wrapped, $touched) = (0, 0);
my @skipped;
my $simple = qr/[a-zA-Z_][a-zA-Z0-9_.:]*/;

sub lookup_expr {
    my ($lit, $pairs) = @_;
    my $key = $lit;
    $key =~ s/\{([a-zA-Z_][a-zA-Z0-9_]*)\}/%{$1}/g;
    return "crate::i18n::lookup(\"$key\", &[])" unless @$pairs;
    my $p = join ", ", map { "(\"" . $_->[0] . "\", " . $_->[1] . ".to_string())" } @$pairs;
    return "crate::i18n::lookup(\"$key\", &[$p])";
}

# Find the matching close paren for the open paren at $start (index of
# '('). Skips string literals and char literals so a paren inside text
# cannot fool it.
sub match_paren {
    my ($s, $start) = @_;
    my $d = 0;
    my $i = $start;
    my $n = length $s;
    while ($i < $n) {
        my $c = substr($s, $i, 1);
        if ($c eq '"') {
            $i++;
            while ($i < $n) {
                my $d2 = substr($s, $i, 1);
                if ($d2 eq '\\') { $i += 2; next }
                last if $d2 eq '"';
                $i++;
            }
            $i++;
            next;
        }
        if ($c eq "'") {
            # char literal like '\'' or 'x'
            $i++;
            $i += 2 if substr($s, $i, 1) eq '\\';
            $i++;
            $i++ if substr($s, $i, 1) eq "'";
            next;
        }
        $d++ if $c eq '(';
        $d-- if $c eq ')';
        return $i if $d == 0;
        $i++;
    }
    return -1;
}

# Rewrite one push_msg_status argument. Returns replacement source, or
# the input unchanged when the shape cannot be proven.
#
# EVERY value becomes a string at this boundary, so the lookup's
# replace sees the same rendering Rust's format! produced:
#
#   {a}      -> ("a", a.to_string())
#   {a:#}    -> ("a", format!("{a:#}"))     (format SPECs are kept)
#   {:>12}   -> ("1", format!("{:>12}", arg))
#   {}       -> ("1", arg.to_string())
#
# Named placeholders keep their names in the key (`... %{a} ...`);
# anonymous ones become positional `%s` and are numbered so several in
# one string resolve in order.
sub rewrite_call {
    my ($expr, $f) = @_;
    $expr =~ s/^\s+|\s+$//g;

    if ($expr =~ /^\s*"((?:[^"\\]|\\.)*)"\s*\.into\(\)\s*$/s) {
        my $lit = $1;
        if ($lit =~ /\\(?!n|t|"|\\)/) {
            push @skipped, "$f: escape in \"$lit\"";
            return undef;
        }
        $wrapped++;
        return lookup_expr($lit, []);
    }

    return undef unless $expr =~ /^\s*format!\(\s*"((?:[^"\\]|\\.)*)"\s*((?:,\s*(?:$simple|"[^"]*"))*)\s*\)\s*$/s;
    my ($lit, $argtxt) = ($1, $2);
    my @args = grep { length } map { s/^\s+|\s+$//gr } split /,/, $argtxt;
    return undef if grep { !/^(?:$simple|"[^"]*")$/ } @args;

    # Find every placeholder, in order, with its name and spec.
    my @slots;
    my $scan = $lit;
    while ($scan =~ /\{([a-zA-Z_][a-zA-Z0-9_]*)?(:[^}]*)?\}/g) {
        push @slots, [defined $1 ? $1 : undef, defined $2 ? $2 : undef];
    }
    # Any brace that is NOT a placeholder means the literal contains a
    # format spec this script would have to interpret, or an escaped
    # brace; leave it for a hand edit.
    my $stripped = $lit;
    $stripped =~ s/\{[a-zA-Z_][a-zA-Z0-9_]*?(?::[^}]*)?\}//g;
    return undef if $stripped =~ /[{}]/;

    # Rust semantics decide where each value comes from: a NAMED
    # placeholder captures from the enclosing scope, and only an
    # anonymous one consumes a positional argument (in order). Requiring
    # slot-count == arg-count would refuse the most common shape in the
    # tree — `format!("could not open {url}: {e}")`, which has two named
    # slots and no arguments at all.
    my $want_args = grep { !defined $_->[0] || !length $_->[0] } @slots;
    return undef unless $want_args == @args;

    # Rewrite the literal into the %{..} / %s key form and collect the
    # value pairs at the same time.
    my $key = '';
    my $rest = $lit;
    my @named;
    my $posi = 0;
    while ($rest =~ /\{([a-zA-Z_][a-zA-Z0-9_]*)?(:[^}]*)?\}/g) {
        my ($name, $spec) = ($1, $2);
        $key .= substr($rest, 0, $-[0]);
        if (defined $name && length $name) {
            $key .= "%{$name}";
            my $val = (defined $spec && length $spec)
                ? "format!(\"{$name$spec}\")" : $name;
            push @named, "(\"$name\", $val.to_string())";
        } else {
            $key .= "%s";
            my $arg = shift @args;
            my $val = (defined $spec && length $spec)
                ? "format!(\"{$spec}\", $arg)" : $arg;
            $posi++;
            push @named, "(\"$posi\", $val.to_string())";
        }
        $rest = substr($rest, $+[0]);
    }
    $key .= $rest;

    $wrapped++;
    return @named
        ? "crate::i18n::lookup(\"$key\", &[" . join(", ", @named) . "])"
        : "crate::i18n::lookup(\"$key\", &[])";
}

for my $f (@files) {
    open my $fh, '<', $f or next;
    local $/; my $src = <$fh>; close $fh;
    next unless $src =~ /push_msg_status/;

    my $out = '';
    my $pos = 0;
    my $changed = 0;
    while (1) {
        my $at = index($src, 'push_msg_status(', $pos);
        last if $at < 0;
        my $open = index($src, '(', $at);
        # The name must be a whole word.
        my $before = $at > 0 ? substr($src, $at - 1, 1) : '';
        if ($before =~ /[A-Za-z0-9_]/) { $out .= substr($src, $pos, $at + 1 - $pos); $pos = $at + 1; next }

        my $close = match_paren($src, $open);
        last if $close < 0;
        my $arg = substr($src, $open + 1, $close - $open - 1);
        my $new = rewrite_call($arg, $f);
        $out .= substr($src, $pos, $open - $pos + 1);
        if (defined $new) {
            $out .= $new;
            $changed = 1;
        } else {
            $out .= $arg;
        }
        $pos = $close;
    }
    $out .= substr($src, $pos);
    next unless $changed;
    if (!$dry) {
        open my $w, '>', $f or die "write $f: $!";
        print $w $out; close $w;
    }
    $touched++;
}

print "wrapped $wrapped call sites in $touched files\n";
if (@skipped) {
    print "needs a hand edit (", scalar @skipped, "):\n";
    print "  $_\n" for @skipped;
}