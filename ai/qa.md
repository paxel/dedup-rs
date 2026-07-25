# duplicates tab
## when comparing two images:
- in flicker mode the Mark B pill does not change with swap
- in side by side clicking mark a triggers mark a nad mark b
- mark a should not be allowed at all as it is read only
- when in lightbox i can switch through the duplicates, but I dont see which repo the current one is from
- the mark pills with or without A and B shuld be DELETE, DELETE A, DELETE B, and if protected shown disabled, maybe strike through?
- flipping side by side has either mark a and mark b or only mark b so something is wrong about removing mark b i guess
## when displaying two mp3s in lightbox:
- when I click pause the music stops. when I click next mp3 the playing continues. the play state is not checked when resuming play on the new file. 
- the pause is back when switching the mp3s
## when displaying more than 2 audio files in lightbox
- I can flip through the 4 files until I click compare, then the switcher freezes
- I edited one id3 tag to mark 1 of the 4 similar mp3s and it turns out the tag is shown every second time I click next. so 1 and 3 show suddenly the modified idv3 tag. makes me think that the numbers iterate from 1 to 4 but the files iterate from 1 to 2
## when comparing two mp3s:
- no mark buttons exists
## after deleting duplicates
- the repo tab shows now changes in the repo until I update. I think switching to the tab should update the numbers from db, no?
## compare selection. 
- it is completely unclear to me which files are chosen for comparison. the current shown should be on the left, so the user can chose what to compare against. and in the comparison he can flip between the OTHERS and that should be shown in the switcher. like when 3 is left you should never see <3/4> in the selector
# filter
- no negation exists. like all that are NOT *.mp3
- not entirely sure about case sensitivity. a flag for switching it would make sense
# transfer
## compare 
- this should be moved to the sync repo tab. this was what i meant with compare. the compare in groups as it is now comparing against all repos and giving numbers makes absolutely no sense and needs to go away
- the tables should be optimized for the values in the columns. to look compact and not have huge gaps. i think the font can be bigger, or the rows at least taller. see next point
- I expected the tables to have the same little preview images as in duplicates review
## compare by hash
- the diff in the name could be more highlighted. like with a blue bachground for character that differ from the otherside. I assume there is some smart algorithm that prevents a 1:1 comparison and highlighting everything after an additional character?
- in case we have thousands of files we should have buttons for doing the actions for all remaining files? rename all left, rename all right?
## compare by path
- I wanted a column between the paths that has the compare button
- the current buttons in the column handling truncates buttons or removes all text as the columns are too narrow. the buttons could be multiline maybe?
- compare of two audio is completely broken. my demand to show the compare of different types is not working at all and obviously not reused from duplicates view

