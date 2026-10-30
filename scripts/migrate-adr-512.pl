#!/usr/bin/perl
# ADR-512 migration (every parameter list is a list; a vector is data): inside every
# `(impl …)`, `(defability …)` and `(defbehaviour …)` form, a method's or op spec's
# parameter VECTOR — the second element of a direct child list, `(op [a b] …)` — becomes a
# LIST, `(op (a b) …)`. Only those two brackets change; everything else is left
# byte for byte. Works in .blsp (code, docstrings, comments), .md (fenced blocks and inline
# spans on one line) and .rs (Brood source inside Rust string literals).
#
#   perl scripts/migrate-adr-512.pl [--dry] FILE…   prints "FILE: N" for each file it changes
#
# For a downstream project: run it over every file that mentions the three forms, then
# `nest format` and `nest test`. It does not see a method form a macro BUILDS — a
# `:derive-record` recipe returning `(op [r] …)`, or `(list op ['self])` — so grep those.
#   perl ../brood/scripts/migrate-adr-512.pl $(grep -rlE "\((impl|defability|defbehaviour) " --include=*.blsp --include=*.md .)
use strict;
use warnings;

my $dry = 0;
if (@ARGV && $ARGV[0] eq '--dry') { $dry = 1; shift @ARGV; }

for my $file (@ARGV) {
    open(my $in, '<:raw', $file) or die "$file: $!";
    local $/;
    my $text = <$in>;
    close $in;
    my @regions = regions($file, $text);
    my %swap;    # position => replacement char
    for my $region (@regions) {
        my ($start, $end, $delim, $open) = @$region;
        # A form starting in plain code may run past its region (a docstring inside it ends
        # the region); the scan skips strings itself, so it is bounded by the file.
        my $limit = $open ? length($text) : $end;
        my $at = $start;
        while ($at < $end) {
            my $found = index($text, '(', $at);
            last if $found < 0 || $found >= $end;
            if (substr($text, $found, 14) =~ /^\((impl|defability|defbehaviour)[\s]/) {
                scan_form(\$text, $found, $limit, $delim, \%swap);
            }
            $at = $found + 1;
        }
    }
    next unless %swap;
    print "$file: " . (scalar(keys %swap) / 2) . "\n";
    next if $dry;
    for my $pos (keys %swap) { substr($text, $pos, 1) = $swap{$pos}; }
    open(my $out, '>:raw', $file) or die "$file: $!";
    print $out $text;
    close $out;
}

# The spans of FILE that hold Brood source, as [start, end, string-delimiter]. The
# delimiter is how a Brood string literal is spelled there: `"` in plain source, `\"`
# inside a string (a docstring, or a Rust string literal holding Brood code).
sub regions {
    my ($file, $text) = @_;
    my $n = length $text;
    my @out;
    if ($file =~ /\.blsp$/) {
        my $i = 0;
        my $code_start = 0;
        while ($i < $n) {
            my $c = substr($text, $i, 1);
            if ($c eq ';') {
                my $eol = index($text, "\n", $i);
                $eol = $n if $eol < 0;
                push @out, [$code_start, $i, '"', 1];
                push @out, [$i, $eol, '"'];    # a comment: one line, plain spelling
                $i = $eol;
                $code_start = $i;
            } elsif ($c eq '"') {
                push @out, [$code_start, $i, '"', 1];
                my $j = $i + 1;
                while ($j < $n) {
                    my $d = substr($text, $j, 1);
                    if ($d eq '\\') { $j += 2; next; }
                    last if $d eq '"';
                    $j++;
                }
                push @out, [$i + 1, $j, '\\"'];    # a string: embedded spelling
                $i = $j + 1;
                $code_start = $i;
            } else {
                $i++;
            }
        }
        push @out, [$code_start, $n, '"', 1];
    } elsif ($file =~ /\.md$/) {
        my $pos = 0;
        my $in_fence = 0;
        my $fence_start = 0;
        for my $line (split /(?<=\n)/, $text) {
            my $len = length $line;
            if ($line =~ /^\s*```/) {
                if ($in_fence) { push @out, [$fence_start, $pos, '"']; $in_fence = 0; }
                else { $in_fence = 1; $fence_start = $pos + $len; }
            } elsif (!$in_fence) {
                push @out, [$pos, $pos + $len, '"'];    # prose: one line at a time
            }
            $pos += $len;
        }
        push @out, [$fence_start, $n, '"'] if $in_fence;
    } elsif ($file =~ /\.rs$/) {
        my $i = 0;
        while ($i < $n) {
            my $rest = substr($text, $i, 4);
            if ($rest =~ /^\/\//) {
                my $eol = index($text, "\n", $i);
                $i = $eol < 0 ? $n : $eol;
            } elsif ($rest =~ /^r(#*)"/) {
                my $hashes = $1;
                my $open = $i + 1 + length($hashes) + 1;
                my $close = index($text, '"' . $hashes, $open);
                $close = $n if $close < 0;
                push @out, [$open, $close, '"'];
                $i = $close + 1 + length $hashes;
            } elsif (substr($text, $i, 1) eq "'" && substr($text, $i, 4) =~ /^'(\\.|[^\\'])'/) {
                $i += length $&;    # a char literal, which may be '"'
            } elsif (substr($text, $i, 1) eq '"') {
                my $j = $i + 1;
                while ($j < $n) {
                    my $d = substr($text, $j, 1);
                    if ($d eq '\\') { $j += 2; next; }
                    last if $d eq '"';
                    $j++;
                }
                push @out, [$i + 1, $j, '\\"'];
                $i = $j + 1;
            } else {
                $i++;
            }
        }
    }
    return @out;
}

# Walk the balanced form whose `(` is at POS (an `(impl` or `(defability`), within END.
# For each direct child list, if its second element is a vector, mark that vector's
# brackets for swapping. A form that does not close within END is left alone.
sub scan_form {
    my ($text_ref, $pos, $end, $delim, $swap) = @_;
    my %pending;
    my $depth = 0;
    my $i = $pos;
    # Per open list at depth 2: how many elements it has seen, and whether we are inside one.
    my ($child_elements, $in_element) = (0, 0);
    my @stack;    # the bracket kind of each open level
    while ($i < $end) {
        my $c = substr($$text_ref, $i, 1);
        # strings, in the region's spelling
        if ($delim eq '"' && $c eq '"') {
            my $j = $i + 1;
            while ($j < $end) {
                my $d = substr($$text_ref, $j, 1);
                if ($d eq '\\') { $j += 2; next; }
                last if $d eq '"';
                $j++;
            }
            count_element(\$child_elements, \$in_element, $depth, \@stack);
            $i = $j + 1;
            next;
        }
        if ($delim eq '\\"' && substr($$text_ref, $i, 2) eq '\\"') {
            my $j = $i + 2;
            while ($j < $end) {
                if (substr($$text_ref, $j, 2) eq '\\\\') { $j += 2; next; }
                last if substr($$text_ref, $j, 2) eq '\\"';
                $j++;
            }
            count_element(\$child_elements, \$in_element, $depth, \@stack);
            $i = $j + 2;
            next;
        }
        if ($c eq ';' && $delim eq '"') {    # a comment inside the form
            my $eol = index($$text_ref, "\n", $i);
            $i = ($eol < 0 || $eol > $end) ? $end : $eol;
            next;
        }
        if ($c =~ /[(\[{]/) {
            # an opening bracket starts a new element of the list it is in
            if ($depth == 2 && $stack[-1] eq '(') {
                $child_elements++;
                $in_element = 0;
                if ($c eq '[' && $child_elements == 2) { $pending{$i} = '('; push @stack, '[!'; $depth++; $i++; next; }
            }
            push @stack, $c;
            $depth++;
            if ($depth == 2) { ($child_elements, $in_element) = (0, 0); }
            $i++;
            next;
        }
        if ($c =~ /[)\]}]/) {
            my $open = pop @stack;
            $depth--;
            if (defined $open && $open eq '[!') { $pending{$i} = ')'; }
            if ($depth == 0) {
                %$swap = (%$swap, %pending);
                return;
            }
            if ($depth == 2) { $in_element = 0; }
            $i++;
            next;
        }
        if ($c =~ /\s/) {
            $in_element = 0 if $depth == 2;
            $i++;
            next;
        }
        # an atom character
        if ($depth == 2 && $stack[-1] eq '(' && !$in_element) { $child_elements++; $in_element = 1; }
        $i++;
    }
    # did not close inside the region: leave it alone
}

sub count_element {
    my ($elements, $in_element, $depth, $stack) = @_;
    if ($depth == 2 && $stack->[-1] eq '(') { $$elements++; $$in_element = 0; }
}
