# 09 — Selected-tab border + repo-name consistency

**What to build:** Two header polish fixes in the viewer. The **active representation tab** (Image
/ Text / Metadata / …) gets a clear border/highlight so it's obvious which is selected. And the
**repo-name header decoration** (the identicon chip) is made consistent across the Image and Text
tabs — today the Text view is decorated and the Image view is plain.

**Blocked by:** None — can start immediately.

**Status:** resolved

- [ ] The selected representation tab is visibly distinguished (a border/highlight around it).
- [ ] The repo-name header reads consistently across the Image and Text tabs.
- [ ] A harness test asserts the selected tab is marked and the header is consistent across tabs.
- [ ] Gate green.
