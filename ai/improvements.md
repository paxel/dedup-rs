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
- [ ] Plugin support for new formats; an API to externalise features.
