# dedup-rs — open improvements

Open work only. Closed items live in `git log`; documentation lives in `docs/`.

## Performance at scale

- [ ] Banded grouping, staged pipelines and the multi-reference diff's merged content index
  are fine at ~10⁵ files. Revisit content-index memory (`HashMap<(u64,[u8;32]), _>` across all
  references) and timeline streaming before ~10⁷.

## Recognition & extensibility *(far future)*

- [ ] Face and object recognition for photos/images.
- [ ] VLA tagging of files to topics; word clouds for documents.
- [ ] Metadata extraction for all known formats (ID3 tags already handled).
- [ ] Plugin support for new formats; an API to externalise features — specced for 0.5.0 in
  [#2](https://github.com/paxel/dedup-rs/issues/2) (similarity providers + viewer visualisations).

## GUI reshape 3.0 — [#4](https://github.com/paxel/dedup-rs/issues/4), ADR 0004

Start page hub, one command list, one board. Landed one child per commit, in order:

- [x] #5 Start page shell replaces the tab bar
- [x] #6 Start page facts: totals, stale badges, last runs
- [x] #7 Progressive inputs and from/to roles
- [x] #8 Favorites
- [x] #9 Preview and APPLY everywhere
- [ ] #10 The board module, with Browse on it
- [ ] #11 Duplicates on the board
- [ ] #12 Transfer and Grooming plans on the board
- [ ] #13 Diff on the board
- [ ] #14 Skin pass
- [ ] #15 GUI guide rewritten
