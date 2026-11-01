slug: packdir
shelf: unix
default_edition: full

# Publication contract

This repository owns the code, the manual in `docs/packdir-manual.md`, the cover
and headboards in `cover/`, and the blog post in `docs/blog/packdir/`. FirstPair
owns catalog entries, stable download and reader routes, and Blob manifests.
Follow the shared `~/src/firstpair/AGENTS.md` publication rules.

The manual is by Alexy Khrabrov with Claude as co-author. The code is Apache 2.0.

`cover/packdir-cover.png` is the book cover, `cover/packdir-headboard.png`, without
text, is the shelf headboard, and `cover/packdir-headboard-text.png` is the blog and
textpack headboard.

```sh
PATH=/usr/local/bin:$PATH ~/src/firstpair/publishing/scripts/build-library-book.sh --repo-root ~/src/packdir
cd ~/src/firstpair
npm run library:publish -- ~/src/packdir --source https://github.com/querygraph/packdir
```
