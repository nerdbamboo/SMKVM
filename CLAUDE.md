# Working in this repository

## The READMEs are one document in three languages

`README.md`, `README.ko.md` and `README.ja.md` say the same thing in English,
Korean and Japanese. Any change to one of them is a change to all three: carry
it across in the same edit and commit them together, rather than leaving a
note to translate it later. A README that disagrees with its translation is
worse than no translation, and nothing in the repository would announce it.

Keep the three structurally identical -- same sections, same order, same
commands, paths and crate names, same table rows. Only the prose is
translated. Each begins with the same switcher line, with the language you
are reading left unlinked.

`scripts/check-readmes.sh` refuses a change that touched only some of them; CI
runs it on every push and pull request.

## The rest

Build, test and layout are described in the README's last two sections. The
design reasoning lives in the commit messages, and `docs/NOTES.md` holds what
is not in the code.
