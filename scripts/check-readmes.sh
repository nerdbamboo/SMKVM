#!/bin/sh
# README.md, README.ko.md and README.ja.md are one document in three
# languages. Touching one and leaving the others behind does not make a
# partial translation, it makes two documents that disagree, and nothing
# about the repository would say so afterwards. This refuses that shape.
#
#   scripts/check-readmes.sh                 what is staged
#   scripts/check-readmes.sh BASE            working tree against BASE
#   scripts/check-readmes.sh BASE HEAD       one commit range, as CI does
set -eu

READMES="README.md README.ko.md README.ja.md"

if [ $# -ge 2 ]; then
	changed=$(git diff --name-only "$1" "$2" -- $READMES)
elif [ $# -eq 1 ]; then
	changed=$(git diff --name-only "$1" -- $READMES)
else
	changed=$(git diff --cached --name-only -- $READMES)
fi

[ -n "$changed" ] || exit 0

missing=
for f in $READMES; do
	printf '%s\n' "$changed" | grep -qx "$f" || missing="$missing $f"
done

[ -n "$missing" ] || exit 0

{
	echo "The READMEs have gone out of step."
	echo
	echo "  changed:  $(printf '%s ' $changed)"
	echo "  untouched:$missing"
	echo
	echo "Carry the change into the others and commit the three together."
} >&2
exit 1
